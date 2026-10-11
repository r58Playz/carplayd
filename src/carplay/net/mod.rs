use std::{
	future::poll_fn,
	net::SocketAddrV6,
	pin::{Pin, pin},
	task::{Context, Poll},
	time::Duration,
};

use anyhow::{Context as AnyhowContext, Result, anyhow, bail};
use futures::future::{Either, select};
use log::{debug, info, warn};
use tokio::{
	select,
	sync::{mpsc, oneshot, watch},
};
use tokio_util::task::AbortOnDropHandle;

use crate::{
	carplay::{
		CARPLAY_DEVICE_NAME, CARPLAY_MODEL, CARPLAY_OS_BUILD_VERSION, CARPLAY_SESSION_ID,
		CARPLAY_SOURCE_VERSION, DeviceIdentity, format_mac_lower, format_mac_upper,
		modes::{ModesConstraint, ModesEntity, ModesMachine, ModesOwnerState, ModesResourceId},
		net::{
			conn::HeadUnitTcpConnection,
			head_unit::HeadUnitServer,
			proto::{
				command::{AccessoryCommand, ChangeModesResponse, DeviceCommand},
				feedback::{FeedbackRequest, FeedbackResponse},
				info::{AccessoryInfo, InfoRequest},
				modes::Modes,
				setup::{
					SetupRequest, SetupResponse, StreamRequest, StreamSetupRequest,
					StreamSetupResponse, TeardownRequest, TimestampInfo,
				},
			},
			rtsp::{Method, Request, Response},
			screen::{EncoderCommand, ScreenStream},
			timing::Clock,
		},
	},
	pairing::{
		identity::{PairIdentity, PairingStore, random_uuid},
		keys::SessionKeys,
	},
	provider::CarplayProviderConnectionProxy,
	v4l2::VideoParams,
};

pub mod codec;
mod conn;
pub mod head_unit;
pub mod proto;
pub mod rtsp;
mod screen;
pub mod timing;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum NetSessionStream {
	Screen,
}
struct NetSessionStreamFutures {
	screen: Option<(
		mpsc::UnboundedSender<EncoderCommand>,
		AbortOnDropHandle<Result<()>>,
	)>,
}
impl NetSessionStreamFutures {
	pub fn new() -> Self {
		Self { screen: None }
	}

	pub fn has_screen(&self) -> bool {
		self.screen.is_some()
	}
	pub fn replace_screen<F: Future<Output = Result<()>> + Send + 'static>(
		&mut self,
		ctrl: mpsc::UnboundedSender<EncoderCommand>,
		fut: F,
	) {
		self.screen
			.replace((ctrl, AbortOnDropHandle::new(tokio::spawn(fut))));
	}
	pub fn send_screen(&self, cmd: EncoderCommand) -> Result<()> {
		self.screen
			.as_ref()
			.context("no screen task")?
			.0
			.send(cmd)
			.map_err(|_| anyhow!("screen task died"))
	}
	pub fn stop_screen(&mut self) {
		self.screen.take();
	}

	fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<(NetSessionStream, Result<()>)> {
		if let Some(fut) = self.screen.as_mut()
			&& let Poll::Ready(res) = Pin::new(&mut fut.1).poll(cx)
		{
			self.screen = None;
			return Poll::Ready((
				NetSessionStream::Screen,
				res.context("failed to join").flatten(),
			));
		}

		Poll::Pending
	}

	async fn next(&mut self) -> (NetSessionStream, Result<()>) {
		poll_fn(|cx| self.poll_next(cx)).await
	}
}

enum NetSessionEvent {
	Stream(NetSessionStream, Result<()>),
	AccessoryCommand(
		AccessoryCommand,
		oneshot::Sender<Response>,
		oneshot::Receiver<()>,
	),
	FeedbackTimer,
}

pub struct CarplayNetSession {
	server: HeadUnitServer,
	shared_secret: [u8; 32],
	rtsp_url: String,
	clock: Clock,
	info: AccessoryInfo,
	video_params: VideoParams,

	next_connection_id: i64,
	modes: ModesMachine,

	control: HeadUnitTcpConnection,
	events: Option<HeadUnitTcpConnection>,

	proxy: watch::Receiver<Option<CarplayProviderConnectionProxy>>,
}
impl CarplayNetSession {
	pub async fn new(
		pair_identity: &mut PairIdentity,
		pairing: &mut PairingStore,
		head_unit: HeadUnitServer,
		identity: DeviceIdentity,
		proxy: watch::Receiver<Option<CarplayProviderConnectionProxy>>,
		clock: Clock,
		timing_port: u16,
	) -> Result<Self> {
		let rtsp_url = format!("rtsp://[{}]/{CARPLAY_SESSION_ID}", head_unit.addr.ip());
		let mut control = HeadUnitTcpConnection::new(head_unit.addr)
			.await
			.context("failed to connect to control socket")?;

		let ltpk = match pairing.get(&head_unit.pairing_identity) {
			Some(x) => x,
			None => {
				let res = control
					.pair_setup(pair_identity)
					.await
					.context("failed to pair_setup")?;
				if res.accessory_id != head_unit.pairing_identity {
					bail!(
						"{:?} accessory_id did not match {:?} pairing_identity",
						res.accessory_id,
						head_unit.pairing_identity
					);
				}
				pairing
					.insert(&res.accessory_id, res.accessory_ltpk)
					.await
					.context("failed to save pairing identity")?;
				res.accessory_ltpk
			}
		};

		let shared_secret = control
			.pair_verify(pair_identity, ltpk)
			.await
			.context("failed to pair_verify")?;

		control
			.upgrade(SessionKeys::control(&shared_secret))
			.context("failed to upgrade")?;

		control.mfi_auth().await.context("failed to do mfi auth")?;

		let setup_req = SetupRequest {
			device_id: format_mac_lower(identity.device_id),
			mac_address: format_mac_upper(identity.mac_address),
			model: CARPLAY_MODEL.to_string(),
			name: CARPLAY_DEVICE_NAME.to_string(),
			os_build_version: CARPLAY_OS_BUILD_VERSION.to_string(),
			source_version: CARPLAY_SOURCE_VERSION.to_string(),
			timing_port,
			features: Some(
				["uiContext", "viewAreas", "cornerMasks", "focusTransfer"]
					.map(str::to_string)
					.to_vec(),
			),
			keep_alive_low_power: Some(false),

			session_uuid: random_uuid(),
			streams: None,
		};
		let setup = control
			.req::<SetupResponse>(Method::Setup, &rtsp_url, &setup_req)
			.await
			.context("failed to SETUP target")?;

		let info = control
			.req::<AccessoryInfo>(Method::Get, "/info", &InfoRequest::all())
			.await
			.context("failed to GET /info")?;

		let display = info.primary_display().context("no display")?;
		let video_params = ScreenStream::probe(
			display.width_pixels,
			display.height_pixels,
			display.max_fps.unwrap_or(30),
		)
		.await
		.context("failed to probe screen encoder")?;

		let events_addr = SocketAddrV6::new(
			*head_unit.addr.ip(),
			setup
				.event_port
				.context("no event port provided in SETUP")?,
			0,
			head_unit.addr.scope_id(),
		);
		let mut events = HeadUnitTcpConnection::new(events_addr)
			.await
			.context("failed to connect to event port")?;
		events
			.upgrade(SessionKeys::event(&shared_secret))
			.context("failed to upgrade events channel")?;

		Ok(Self {
			server: head_unit,
			shared_secret,
			rtsp_url,
			clock,
			info,
			video_params,

			next_connection_id: 1,
			modes: ModesMachine::new(),

			control,
			events: Some(events),
			proxy,
		})
	}

	pub fn video_params(&self) -> VideoParams {
		self.video_params
	}

	async fn command(&mut self, command: DeviceCommand) -> Result<()> {
		let name = command.type_name();
		let body = command
			.to_binary()
			.with_context(|| format!("failed to encode {name}"))?;

		let response = self
			.control
			.request(Request::new(Method::Post, "/command").body(proto::CONTENT_TYPE, body))
			.await
			.with_context(|| format!("failed to POST {name}"))?;

		if !response.is_success() {
			bail!("{name} was refused: {}", response.status);
		}

		Ok(())
	}

	/// self.modes must match modes
	async fn modes_changed(
		&mut self,
		had_screen: bool,
		streams: &mut NetSessionStreamFutures,
		modes: Modes,
	) -> Result<()> {
		self.command(DeviceCommand::ModesChanged(modes))
			.await
			.context("failed to send modes update")?;

		match (
			self.modes
				.inspect_resource(ModesResourceId::MainScreen, |x| x.owner()),
			had_screen,
		) {
			(ModesEntity::Controller, false) => {
				// start screen
				let (tx, rx) = mpsc::unbounded_channel();

				let provider = Box::pin({
					let mut provider = self.proxy.clone();
					async move {
						let provider = provider
							.wait_for(Option::is_some)
							.await
							.context("failed to wait for provider")?
							.clone()
							.unwrap();

						provider.get_screen().await.context("failed to get screen")
					}
				});
				let carplay = pin!(async {
					let screen = self.info.primary_display().context("no display")?;

					let connection_id = self.next_connection_id;
					self.next_connection_id += 1;

					let req = StreamRequest {
						uuid: Some(screen.uuid.clone()),
						timestamp_info: Some(TimestampInfo::defaults()),
						..StreamRequest::screen(connection_id, 75)
					};
					let keys = SessionKeys::data_stream(&self.shared_secret, req.connection_id());

					let setup = self
						.control
						.req::<StreamSetupResponse>(
							Method::Setup,
							&self.rtsp_url,
							&StreamSetupRequest { streams: vec![req] },
						)
						.await
						.context("failed to send screen setup req")?
						.streams
						.context("no streams in response")?;
					let stream = setup.first().context("no streams in response")?;

					let addr = SocketAddrV6::new(
						*self.server.addr.ip(),
						stream.data_port.context("no data port in response")?,
						self.server.addr.flowinfo(),
						self.server.addr.scope_id(),
					);

					let screen = ScreenStream::new(addr, keys, self.clock, self.video_params)
						.await
						.context("failed to create screen stream")?;

					anyhow::Ok(screen)
				});

				let (screen, pending_provider) = match select(carplay, provider).await {
					Either::Left((screen, provider)) => (screen?, Some(provider)),
					Either::Right((provider, screen)) => {
						tx.send(EncoderCommand::NewProvider(provider?))
							.map_err(|_| anyhow!("failed to send new provider"))?;
						(screen.await?, None)
					}
				};

				streams.replace_screen(tx.clone(), async move {
					let run = pin!(screen.run(rx));

					match pending_provider {
						None => run.await,
						Some(provider) => match select(run, provider).await {
							Either::Left((result, _)) => result,
							Either::Right((provider, run)) => {
								tx.send(EncoderCommand::NewProvider(provider?))
									.map_err(|_| anyhow!("failed to send provider to encoder"))?;
								run.await
							}
						},
					}
				});
			}
			(ModesEntity::Accessory, true) => {
				// stop screen
				let body = proto::to_binary(&TeardownRequest::screen())
					.context("failed to encode the screen TEARDOWN")?;

				let response = self
					.control
					.request(
						Request::new(Method::Teardown, &self.rtsp_url)
							.body(proto::CONTENT_TYPE, body),
					)
					.await
					.context("failed to TEARDOWN the screen stream")?;

				if !response.is_success() {
					bail!("the screen TEARDOWN was refused: {}", response.status);
				}

				streams.stop_screen();
			}
			_ => {}
		}

		Ok(())
	}

	async fn service_events(
		mut events: HeadUnitTcpConnection,
		tx: mpsc::UnboundedSender<NetSessionEvent>,
	) -> Result<()> {
		while let Some(req) = events
			.next_request()
			.await
			.context("failed to wait for next events req")?
		{
			if req.method != Method::Post || req.target != "/command" {
				events
					.respond(&req, Response::new(400, "Bad Request"))
					.await
					.context("failed to respond to invalid events req")?;
				bail!("invalid events req {req:?}");
			}

			let (done_tx, done_rx) = oneshot::channel();
			let res = async {
				let cmd =
					AccessoryCommand::from_binary(&req.body).context("failed to parse cmd")?;
				let (ev_tx, ev_rx) = oneshot::channel();
				tx.send(NetSessionEvent::AccessoryCommand(cmd, ev_tx, done_rx))
					.context("failed to send event")?;
				ev_rx.await.context("failed to wait for event response")
			}
			.await
			.unwrap_or_else(|x| {
				warn!("failed to get event response {x}");
				Response::new(500, "Internal Server Error")
			});
			events
				.respond(&req, res)
				.await
				.context("failed to respond to events req")?;

			let _ = done_tx.send(());
		}

		Ok(())
	}

	async fn service_accessory_command(
		&mut self,
		streams: &mut NetSessionStreamFutures,
		cmd: AccessoryCommand,
		chan: oneshot::Sender<Response>,
		done: oneshot::Receiver<()>,
	) -> Result<()> {
		let mut modes_to_send = None;
		let res = match cmd {
			AccessoryCommand::ChangeModes(mode_changes) => {
				match self
					.modes
					.handle_mode_changes(ModesEntity::Accessory, mode_changes)
				{
					Ok(modes) => {
						modes_to_send.replace(modes.clone());
						Response::ok().body(
							proto::CONTENT_TYPE,
							proto::to_binary(&ChangeModesResponse {
								status: 0,
								params: Some(modes),
							})
							.context("failed to serialize body")?,
						)
					}
					Err(err) => {
						warn!("modes change failed {err:?}");
						Response::new(403, "Forbidden").body(
							proto::CONTENT_TYPE,
							proto::to_binary(&ChangeModesResponse {
								status: 1,
								params: None,
							})
							.context("failed to serialize body")?,
						)
					}
				}
			}
			AccessoryCommand::RequestUi { url } => {
				info!("request for ui {url:?}");
				match self.modes.resource(ModesResourceId::MainScreen, |x| {
					x.take(
						ModesEntity::Controller,
						ModesConstraint::UserInitiated,
						ModesConstraint::Anytime,
						ModesConstraint::Anytime,
					)
				}) {
					Ok(modes) => {
						modes_to_send.replace(modes);
						Response::ok()
					}
					Err(err) => {
						warn!("modes change failed {err:?}");
						Response::new(403, "Forbidden")
					}
				}
			}
			AccessoryCommand::ForceKeyFrame => {
				if let Err(err) = streams.send_screen(EncoderCommand::ForceKeyFrame) {
					warn!("failed to ask for keyframe {err:?}");
				}

				Response::ok()
			}
			AccessoryCommand::Unknown { r#type, body } => {
				warn!("unknown command {type} {body:?}");
				Response::new(418, "I'm a teapot")
			}
			x => {
				warn!("unhandled command {x:?}");
				Response::ok()
			}
		};

		chan.send(res)
			.map_err(|_| anyhow::anyhow!("failed to send reply"))?;
		done.await
			.context("failed to wait for response to be sent")?;

		if let Some(modes) = modes_to_send {
			self.modes_changed(streams.has_screen(), streams, modes)
				.await
				.context("failed to send modes update")?;
		}

		Ok(())
	}

	pub async fn run(mut self) -> Result<()> {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let events = self.events.take().unwrap();
		let mut streams = NetSessionStreamFutures::new();

		let mut events =
			AbortOnDropHandle::new(tokio::spawn(Self::service_events(events, tx.clone())));

		self.control
			.request(Request::new(Method::Record, &self.rtsp_url))
			.await
			.context("failed to RECORD")?;

		if let Some(req_modes) = self.info.modes.clone() {
			let modes = self
				.modes
				.handle_initial_mode_changes(req_modes)
				.context("failed to handle initial mode changes")?;
			self.modes_changed(streams.has_screen(), &mut streams, modes)
				.await?;
		}

		match self.modes.resource(ModesResourceId::MainScreen, |x| {
			x.take(
				ModesEntity::Controller,
				ModesConstraint::UserInitiated,
				ModesConstraint::Anytime,
				ModesConstraint::Anytime,
			)
		}) {
			Ok(modes) => {
				self.modes_changed(streams.has_screen(), &mut streams, modes)
					.await?;
				info!("took screen from accessory during init");
			}
			Err(err) => {
				info!("didn't take screen from accessory during init: {:?}", err);
			}
		}

		let mut feedback_timer = tokio::time::interval(Duration::from_millis(500));
		loop {
			let Some(ev) = select!(
				x = &mut events => bail!("events died!!! {x:?}"),
				event = rx.recv() => event,
				(stream, res) = streams.next() => Some(NetSessionEvent::Stream(stream, res)),
				_ = feedback_timer.tick() => Some(NetSessionEvent::FeedbackTimer)
			) else {
				break Ok(());
			};
			match ev {
				NetSessionEvent::Stream(stream, res) => {
					if let Err(err) = res {
						warn!("task for stream {stream:?} died {err:?}");
					} else {
						debug!("task for stream {stream:?} exited");
					}

					let resource = match stream {
						NetSessionStream::Screen => ModesResourceId::MainScreen,
					};

					let mut modes = None;
					loop {
						match self
							.modes
							.inspect_resource(resource, |x| (x.owner(), x.owner_state()))
						{
							(ModesEntity::Controller, ModesOwnerState::Taken) => {
								modes.replace(
									self.modes
										.resource(resource, |x| {
											x.untake(
												ModesEntity::Controller,
												Some(ModesEntity::Accessory),
											)
											.map(|_| ())
										})
										.context("failed to untake in teardown")?,
								);
							}
							(ModesEntity::Controller, ModesOwnerState::Borrowed) => {
								modes.replace(
									self.modes
										.resource(resource, |x| x.unborrow(ModesEntity::Controller))
										.context("failed to unborrow in teardown")?,
								);
							}
							_ => break,
						}
					}

					if let Some(modes) = modes {
						self.modes_changed(stream == NetSessionStream::Screen, &mut streams, modes)
							.await?;
					}
				}
				NetSessionEvent::AccessoryCommand(cmd, chan, done) => {
					self.service_accessory_command(&mut streams, cmd, chan, done)
						.await
						.context("failed to service accessory command")?;
				}
				NetSessionEvent::FeedbackTimer => {
					self.control
						.req_opt::<FeedbackResponse>(
							Method::Post,
							"/feedback",
							&FeedbackRequest { streams: None },
						)
						.await
						.context("failed to POST /feedback")?;
				}
			}
		}
	}
}
