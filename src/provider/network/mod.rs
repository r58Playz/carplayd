use std::{
	future::Future,
	net::{IpAddr, Ipv4Addr, SocketAddr},
	path::Path,
	pin::pin,
	time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::{Buf, BytesMut};
use futures::{SinkExt, StreamExt};
use log::info;
use tokio::{
	net::{TcpListener, TcpStream},
	select,
	sync::{mpsc, oneshot, watch},
};
use tokio_util::{
	codec::{Framed, FramedRead, LengthDelimitedCodec},
	task::AbortOnDropHandle,
};
use v4l2r::ioctl::PlaneMapping;

use crate::{
	carplay::{
		input::{InputDevice, InputReport},
		media::NowPlaying,
	},
	provider::{
		CarplayProvider, CarplayProviderConnection, CarplayProviderMedia, CarplayProviderScreen,
		MediaEvent,
		decoder::{
			VideoDecoderConfig, VideoDecoderInput, VideoDecoderOutput, create_video_decoder,
		},
		network::protocol::{ProviderReceivedMessage, ProviderSentMessage},
	},
	v4l2::VideoParams,
};

mod protocol;

const V4L2_DEVICE: &str = "/dev/video10";

enum ProviderEvent {
	ReceivedMessage(BytesMut),
	GetScreen(oneshot::Sender<Result<CarplayDebugNetworkProviderScreen>>),
	GetMedia(oneshot::Sender<CarplayDebugNetworkProviderMedia>),
	ProcessInput(InputDevice, InputReport, oneshot::Sender<Result<()>>),
}

pub struct CarplayDebugNetworkProvider {
	port: u16,
	stream: Option<Framed<TcpStream, LengthDelimitedCodec>>,
	video: Option<VideoParams>,
	input: Option<Vec<InputDevice>>,
	ctrl: Option<mpsc::UnboundedReceiver<ProviderEvent>>,
}
impl CarplayDebugNetworkProvider {
	pub fn new(port: u16) -> Self {
		Self {
			port,
			stream: None,
			video: None,
			input: None,
			ctrl: None,
		}
	}
}
impl CarplayProvider for CarplayDebugNetworkProvider {
	type Connection = CarplayDebugNetworkProviderConnection;

	async fn wait_for_connection(
		&mut self,
		video: VideoParams,
		input: Vec<InputDevice>,
	) -> Result<Self::Connection> {
		let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();

		self.video.replace(video);
		self.input.replace(input);
		self.ctrl.replace(ctrl_rx);

		let listener = TcpListener::bind(SocketAddr::new(
			IpAddr::V4(Ipv4Addr::UNSPECIFIED),
			self.port,
		))
		.await
		.context("failed to bind to port")?;

		info!("debug network provider listening on 0.0.0.0:{}", self.port);

		let (stream, addr) = listener
			.accept()
			.await
			.context("failed to accept connection")?;

		info!("debug network provider accepted from {addr:?}");

		self.stream.replace(
			LengthDelimitedCodec::builder()
				.little_endian()
				.new_framed(stream),
		);

		Ok(CarplayDebugNetworkProviderConnection(ctrl_tx))
	}

	async fn run(mut self) -> Result<()> {
		let video_params = self.video.take().unwrap();
		let input_devices = self.input.take().unwrap();
		let mut stream = self.stream.take().unwrap();
		let mut ctrl = self.ctrl.take().unwrap();

		let (now_playing_tx, now_playing_rx) = watch::channel(None);

		stream
			.send(
				serde_json::to_vec(&ProviderSentMessage::Init {
					display_width: video_params.width,
					display_height: video_params.height,
					display_fps: video_params.fps,
					input_devices,
				})
				.context("failed to serialize")?
				.into(),
			)
			.await
			.context("failed to send init message")?;

		loop {
			let Some(ev) = select!(
				command = stream.next() => command.transpose().context("failed to read from stream")?.map(ProviderEvent::ReceivedMessage),
				event = ctrl.recv() => event
			) else {
				break Ok(());
			};
			match ev {
				ProviderEvent::ReceivedMessage(x) => {
					let parsed = serde_json::from_slice::<ProviderReceivedMessage>(&x)
						.context("failed to read provider recvd message")?;
					match parsed {
						ProviderReceivedMessage::NowPlayingUpdate { now_playing } => {
							now_playing_tx
								.send(now_playing)
								.context("failed to send now_playing")?;
						}
					}
				}
				ProviderEvent::GetScreen(tx) => {
					let _ = tx.send(
						async {
							let listener = TcpListener::bind(SocketAddr::new(
								IpAddr::V4(Ipv4Addr::UNSPECIFIED),
								0,
							))
							.await
							.context("failed to bind to a port")?;
							let port = listener
								.local_addr()
								.context("failed to get local addr")?
								.port();

							info!("waiting for display stream connection on 0.0.0.0:{port}");
							stream
								.send(
									serde_json::to_vec(&ProviderSentMessage::DisplayRequest {
										port,
									})
									.context("failed to serialize")?
									.into(),
								)
								.await
								.context("failed to send request message")?;
							let (stream, addr) = listener
								.accept()
								.await
								.context("failed to accept screen stream")?;
							info!("display stream accepted from {addr:?}");

							let (input, output) = create_video_decoder(
								Path::new(V4L2_DEVICE),
								VideoDecoderConfig::default(),
							)
							.context("failed to create video decoder")?;

							Ok(CarplayDebugNetworkProviderScreen {
								params: video_params,
								input: DecoderInputTask::new(
									LengthDelimitedCodec::builder()
										.little_endian()
										.new_read(stream),
									input,
								),
								output,
							})
						}
						.await,
					);
				}
				ProviderEvent::GetMedia(tx) => {
					let _ = tx.send(CarplayDebugNetworkProviderMedia {
						now_playing_rx: now_playing_rx.clone(),
					});
				}
				ProviderEvent::ProcessInput(device, report, tx) => {
					let _ = tx.send(
						async {
							stream
								.send(
									serde_json::to_vec(&ProviderSentMessage::InputEvent {
										device,
										events: report.events,
									})
									.context("failed to serialize")?
									.into(),
								)
								.await
								.context("failed to send request message")?;
							Ok(())
						}
						.await,
					);
				}
			}
		}
	}
}

pub struct CarplayDebugNetworkProviderConnection(mpsc::UnboundedSender<ProviderEvent>);
impl CarplayProviderConnection for CarplayDebugNetworkProviderConnection {
	type Screen = CarplayDebugNetworkProviderScreen;
	type Media = CarplayDebugNetworkProviderMedia;

	async fn get_media(&mut self) -> Result<Self::Media> {
		let (tx, rx) = oneshot::channel();
		self.0
			.send(ProviderEvent::GetMedia(tx))
			.context("failed to send media request")?;
		rx.await.context("failed to wait for media")
	}

	async fn get_screen(&mut self) -> Result<Self::Screen> {
		let (tx, rx) = oneshot::channel();
		self.0
			.send(ProviderEvent::GetScreen(tx))
			.context("failed to send screen request")?;
		rx.await.context("failed to wait for screen")?
	}

	async fn process_input(&mut self, device: InputDevice, events: InputReport) -> Result<()> {
		let (tx, rx) = oneshot::channel();
		self.0
			.send(ProviderEvent::ProcessInput(device, events, tx))
			.context("failed to send input request")?;
		rx.await.context("failed to wait for input process")?
	}
}

#[derive(Default)]
struct FrameClock {
	anchor: Option<(Instant, u64)>,
}

impl FrameClock {
	fn translate(&mut self, pts_us: u64, received: Instant) -> Result<Instant> {
		let (anchor, first_pts_us) = *self.anchor.get_or_insert((received, pts_us));
		// PTS can run backwards in decode order. Later packet arrival times must
		// not change the first-frame anchor or introduce network jitter into PTS.
		let timestamp = if pts_us >= first_pts_us {
			anchor.checked_add(Duration::from_micros(pts_us - first_pts_us))
		} else {
			anchor.checked_sub(Duration::from_micros(first_pts_us - pts_us))
		};
		timestamp.context("frame timestamp overflow")
	}
}

enum DecoderInputOutcome {
	EmptyStream,
	Finished,
}

struct DecoderInputTask {
	task: Option<AbortOnDropHandle<Result<DecoderInputOutcome>>>,
	output_ended: Option<oneshot::Sender<()>>,
	ended: bool,
}

impl DecoderInputTask {
	fn new(stream: FramedRead<TcpStream, LengthDelimitedCodec>, input: VideoDecoderInput) -> Self {
		let (tx, rx) = oneshot::channel();
		Self {
			task: Some(AbortOnDropHandle::new(tokio::spawn(Self::run(
				stream, input, rx,
			)))),
			output_ended: Some(tx),
			ended: false,
		}
	}

	async fn run(
		mut stream: FramedRead<TcpStream, LengthDelimitedCodec>,
		mut input: VideoDecoderInput,
		mut output_ended: oneshot::Receiver<()>,
	) -> Result<DecoderInputOutcome> {
		let mut clock = FrameClock::default();
		loop {
			let packet = select! {
				_ = &mut output_ended => return Ok(DecoderInputOutcome::Finished),
				packet = stream.next() => packet,
			};
			let received = Instant::now();
			let Some(packet) = packet else { break };
			let mut packet = packet.context("failed to receive input")?;
			let pts_us = packet.try_get_u64_le().context("failed to get frame PTS")?;
			let timestamp = clock.translate(pts_us, received)?;
			select! {
				_ = &mut output_ended => return Ok(DecoderInputOutcome::Finished),
				queued = input.queue_input(&packet, Some(timestamp)) => {
					queued.context("failed to queue input")?;
				}
			}
		}
		// No CAPTURE setup is possible without input; do not wait for it on an
		// empty socket. Otherwise finish runs alongside output across calls.
		if clock.anchor.is_none() {
			return Ok(DecoderInputOutcome::EmptyStream);
		}
		input
			.finish()
			.await
			.context("failed to finish decoder input")?;
		// Dropping INPUT stops its queue, which could discard decoded frames still
		// waiting in CAPTURE. Retain it until output has consumed the LAST buffer.
		let _ = output_ended.await;
		Ok(DecoderInputOutcome::Finished)
	}

	async fn next_frame<T>(
		&mut self,
		frame: impl Future<Output = Result<Option<T>>>,
	) -> Result<Option<T>> {
		if self.ended {
			return Ok(None);
		}
		let mut frame = pin!(frame);

		loop {
			let Some(task) = self.task.as_mut() else {
				return frame.await;
			};
			select! {
				done = task => {
					self.task.take();
					if let DecoderInputOutcome::EmptyStream = done.context("failed to join decoder input task")?? {
						self.ended = true;
						return Ok(None);
					}
				},
				decoded = &mut frame => {
					if matches!(decoded, Ok(None)) {
						// Also release a socket reader if the bitstream ended before
						// socket EOF. A drain already in progress still completes.
						if let Some(tx) = self.output_ended.take() {
							let _ = tx.send(());
						}
						if let Some(task) = self.task.take() {
							task.await.context("failed to join decoder input task")??;
						}
						self.ended = true;
					}
					return decoded;
				}
			}
		}
	}
}

pub struct CarplayDebugNetworkProviderScreen {
	params: VideoParams,
	input: DecoderInputTask,
	output: VideoDecoderOutput,
}

#[async_trait]
impl CarplayProviderScreen for CarplayDebugNetworkProviderScreen {
	async fn next_frame(&mut self, buffer: &mut PlaneMapping) -> Result<Option<Instant>> {
		self.input
			.next_frame(self.output.next_frame(|x| {
				let timestamp = x.timestamp.unwrap_or_else(Instant::now);
				x.image
					.copy_to(buffer, self.params)
					.context("failed to copy")?;
				Ok(timestamp)
			}))
			.await
	}
}

pub struct CarplayDebugNetworkProviderMedia {
	now_playing_rx: watch::Receiver<Option<NowPlaying>>,
}
#[async_trait]
impl CarplayProviderMedia for CarplayDebugNetworkProviderMedia {
	async fn next_now_playing_change(&mut self) -> Result<MediaEvent> {
		self.now_playing_rx
			.changed()
			.await
			.context("failed to wait for now_playing change")?;
		Ok(MediaEvent::NowPlayingChanged(
			self.now_playing_rx.borrow_and_update().clone(),
		))
	}

	fn get_now_playing(&mut self) -> Result<Option<NowPlaying>> {
		Ok(self.now_playing_rx.borrow_and_update().clone())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::time::timeout;

	#[test]
	fn first_pts_anchors_the_clock_and_later_arrival_jitter_is_ignored() {
		let received = Instant::now();
		let mut clock = FrameClock::default();
		// A sender clock need not start at zero or share the daemon's epoch.
		let first_pts = u64::MAX - 100_000;
		assert_eq!(clock.translate(first_pts, received).unwrap(), received);
		assert_eq!(
			clock
				.translate(first_pts + 33_333, received + Duration::from_secs(1))
				.unwrap(),
			received + Duration::from_micros(33_333),
		);
		assert_eq!(
			clock
				.translate(first_pts - 33_333, received + Duration::from_secs(2))
				.unwrap(),
			received - Duration::from_micros(33_333),
		);
	}

	#[tokio::test]
	async fn drain_preserves_frames_across_calls_and_waits_for_input_completion() {
		timeout(Duration::from_secs(1), async {
			let (frames_tx, mut frames_rx) = mpsc::channel(1);
			let (output_ended_tx, output_ended_rx) = oneshot::channel();
			let (reclaimed_tx, reclaimed_rx) = oneshot::channel();
			let task = tokio::spawn(async move {
				// Model CAPTURE returning buffered frames after socket EOF/STOP.
				frames_tx.send(Some(10u64)).await.unwrap();
				frames_tx.send(Some(20)).await.unwrap();
				frames_tx.send(None).await.unwrap();
				// INPUT must stay alive until CAPTURE ends, even if it has no
				// remaining compressed buffers. Completion is joined after LAST.
				output_ended_rx.await.unwrap();
				tokio::task::yield_now().await;
				reclaimed_tx.send(()).unwrap();
				Ok(DecoderInputOutcome::Finished)
			});
			let mut input = DecoderInputTask {
				task: Some(AbortOnDropHandle::new(task)),
				output_ended: Some(output_ended_tx),
				ended: false,
			};
			for expected in [Some(10), Some(20), None] {
				let frame = input
					.next_frame(async { Ok(frames_rx.recv().await.unwrap()) })
					.await
					.unwrap();
				assert_eq!(frame, expected);
			}
			assert!(input.task.is_none());
			reclaimed_rx.await.unwrap();
		})
		.await
		.expect("decoder drain stalled");
	}

	#[tokio::test]
	async fn empty_stream_ends_without_waiting_for_capture_setup() {
		let mut input = DecoderInputTask {
			task: Some(AbortOnDropHandle::new(tokio::spawn(async {
				Ok(DecoderInputOutcome::EmptyStream)
			}))),
			output_ended: None,
			ended: false,
		};
		let frame = timeout(
			Duration::from_secs(1),
			input.next_frame(std::future::pending::<Result<Option<Instant>>>()),
		)
		.await
		.expect("empty stream waited for CAPTURE")
		.unwrap();
		assert!(frame.is_none());
		assert!(
			input
				.next_frame(std::future::pending::<Result<Option<Instant>>>())
				.await
				.unwrap()
				.is_none()
		);
	}

	#[tokio::test]
	async fn input_errors_are_reported_while_waiting_for_capture() {
		let mut input = DecoderInputTask {
			task: Some(AbortOnDropHandle::new(tokio::spawn(async {
				anyhow::bail!("input failed")
			}))),
			output_ended: None,
			ended: false,
		};
		let error = timeout(
			Duration::from_secs(1),
			input.next_frame(std::future::pending::<Result<Option<Instant>>>()),
		)
		.await
		.expect("input failure was hidden")
		.unwrap_err();
		assert_eq!(error.to_string(), "input failed");
	}

	#[tokio::test]
	async fn drain_errors_are_reported_after_the_last_capture_buffer() {
		let (tx, rx) = oneshot::channel();
		let mut input = DecoderInputTask {
			task: Some(AbortOnDropHandle::new(tokio::spawn(async {
				rx.await.unwrap();
				anyhow::bail!("drain failed")
			}))),
			output_ended: Some(tx),
			ended: false,
		};
		let error = timeout(
			Duration::from_secs(1),
			input.next_frame(async { Ok(None::<Instant>) }),
		)
		.await
		.expect("drain completion was not joined")
		.unwrap_err();
		assert_eq!(error.to_string(), "drain failed");
	}
}
