use std::net::SocketAddrV6;

use anyhow::{Context, Result, bail};
use log::{info, warn};
use tokio::{
	select,
	sync::{mpsc, oneshot},
};

use crate::{
	carplay::{
		CARPLAY_DEVICE_NAME, CARPLAY_MODEL, CARPLAY_OS_BUILD_VERSION, CARPLAY_SESSION_ID,
		CARPLAY_SOURCE_VERSION, DeviceIdentity, format_mac_lower, format_mac_upper,
		modes::{ModesConstraint, ModesEntity, ModesMachine, ModesResourceId},
		net::{
			conn::HeadUnitTcpConnection,
			head_unit::HeadUnitServer,
			proto::{
				command::{AccessoryCommand, ChangeModesResponse, DeviceCommand},
				info::{AccessoryInfo, InfoRequest},
				setup::{SetupRequest, SetupResponse},
			},
			rtsp::{Method, Request, Response},
		},
	},
	pairing::{
		identity::{PairIdentity, PairingStore, random_uuid},
		keys::SessionKeys,
	},
};

pub mod codec;
pub mod conn;
pub mod head_unit;
pub mod proto;
pub mod rtsp;
pub mod timing;

enum NetSessionEvent {
	AccessoryCommand(AccessoryCommand, oneshot::Sender<Response>),
}

pub struct CarplayNetSession {
	server: HeadUnitServer,
	shared_secret: [u8; 32],
	info: AccessoryInfo,

	modes: ModesMachine,

	control: HeadUnitTcpConnection,
	events: Option<HeadUnitTcpConnection>,
}
impl CarplayNetSession {
	pub async fn new(
		pair_identity: &mut PairIdentity,
		pairing: &mut PairingStore,
		head_unit: HeadUnitServer,
		identity: DeviceIdentity,
		timing_port: u16,
	) -> Result<Self> {
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
			.req::<SetupResponse>(
				Method::Setup,
				&format!("rtsp://[{}]/{CARPLAY_SESSION_ID}", head_unit.addr.ip()),
				&setup_req,
			)
			.await
			.context("failed to SETUP target")?;

		let info = control
			.req::<AccessoryInfo>(Method::Get, "/info", &InfoRequest::all())
			.await
			.context("failed to GET /info")?;

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

			info,
			modes: ModesMachine::new(),

			control,
			events: Some(events),
		})
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

			let res = async {
				let cmd =
					AccessoryCommand::from_binary(&req.body).context("failed to parse cmd")?;
				info!("got events command {cmd:?}");

				let (ev_tx, ev_rx) = oneshot::channel();
				tx.send(NetSessionEvent::AccessoryCommand(cmd, ev_tx))
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
		}

		Ok(())
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

	pub async fn run(mut self) -> Result<()> {
		let (tx, mut rx) = mpsc::unbounded_channel();

		let events = self.events.take().unwrap();

		if let Some(req_modes) = self.info.modes.clone() {
			let new_modes = self
				.modes
				.handle_initial_mode_changes(req_modes)
				.context("failed to handle initial mode changes")?;
			self.command(DeviceCommand::ModesChanged(new_modes)).await?;
		}

		if let Ok(modes) = self.modes.resource(ModesResourceId::MainScreen, |x| {
			x.take(
				ModesEntity::Controller,
				ModesConstraint::NiceToHave,
				ModesConstraint::Anytime,
				ModesConstraint::Anytime,
			)
		}) {
			self.command(DeviceCommand::ModesChanged(modes)).await?;
			info!("took screen from accessory during init");
		}

		let events = Self::service_events(events, tx.clone());

		let main = async {
			while let Some(event) = rx.recv().await {
				match event {
					NetSessionEvent::AccessoryCommand(cmd, chan) => {
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

						if let Some(modes) = modes_to_send {
							self.command(DeviceCommand::ModesChanged(modes))
								.await
								.context("failed to send modes update")?;
						}
					}
				}
			}
			anyhow::Ok(())
		};

		select! {
			x = events => bail!("events died!!! {x:?}"),
			x = main => x.context("main task failed")?,
		};

		Ok(())
	}
}
