use std::{
	net::{SocketAddr, SocketAddrV6},
	path::Path,
	pin::pin,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Instant,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::{BufMut, BytesMut};
use chacha20poly1305::{AeadInPlace, ChaCha20Poly1305, KeyInit};
use log::{trace, warn};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpSocket, TcpStream},
	select,
	sync::mpsc,
	task::spawn_blocking,
};
use v4l2r::ioctl::PlaneMapping;

use crate::{
	carplay::{
		encoder::{EncodedVideoFrame, HEADER_LEN, TAG_LEN, VideoEncoder},
		net::timing::Clock,
	},
	pairing::keys::{SessionKeys, stream_nonce},
	provider::{CarplayProviderScreen, waiting::WaitingProvider},
	v4l2::VideoParams,
};

const V4L2_DEVICE: &str = "/dev/video11";

const VIDEO_PARAMS_LEN: usize = 112;

const OPCODE_VIDEO_CONFIG: u8 = 1;
const VIDEO_CONFIG_SMALL_PARAMS: [u8; 3] = [0x00, 0x16, 0x01];

pub enum EncoderCommand {
	NewProvider(Box<dyn CarplayProviderScreen>),
	ForceKeyFrame,
}

enum EncoderProviderInternal {
	Provider(Box<dyn CarplayProviderScreen>),
	Waiting(WaitingProvider),
}
impl EncoderProviderInternal {
	fn as_mut(&mut self) -> &mut dyn CarplayProviderScreen {
		match self {
			EncoderProviderInternal::Waiting(x) => x,
			EncoderProviderInternal::Provider(x) => x.as_mut(),
		}
	}
}
enum EncoderProviderOutcome {
	NewProvider(Box<dyn CarplayProviderScreen>),
	FrameOk(Instant),
	FrameErr(anyhow::Error),
	ProviderEnded,
	CtrlEnded,
}

struct EncoderProvider {
	force_key_frame: Arc<AtomicBool>,
	ctrl: mpsc::UnboundedReceiver<EncoderCommand>,
	current: EncoderProviderInternal,
	params: VideoParams,
}
#[async_trait]
impl CarplayProviderScreen for EncoderProvider {
	async fn next_frame(&mut self, buffer: &mut PlaneMapping) -> Result<Option<Instant>> {
		loop {
			match loop {
				let fut = pin!(self.current.as_mut().next_frame(buffer));

				select! {
					frame = fut => {
						match frame {
							Ok(Some(frame)) => break EncoderProviderOutcome::FrameOk(frame),
							Ok(None) => break EncoderProviderOutcome::ProviderEnded,
							Err(err) => break EncoderProviderOutcome::FrameErr(err),
						}
					}
					cmd = self.ctrl.recv() => {
						match cmd {
							Some(EncoderCommand::NewProvider(provider)) => break EncoderProviderOutcome::NewProvider(provider),
							Some(EncoderCommand::ForceKeyFrame) => self.force_key_frame.store(true, Ordering::SeqCst),
							None => break EncoderProviderOutcome::CtrlEnded,
						}
					}
				}
			} {
				EncoderProviderOutcome::NewProvider(provider) => {
					self.current = EncoderProviderInternal::Provider(provider);
				}
				EncoderProviderOutcome::ProviderEnded => {
					self.current =
						EncoderProviderInternal::Waiting(WaitingProvider::new(self.params));
				}
				EncoderProviderOutcome::FrameOk(frame) => {
					break Ok(Some(frame));
				}
				EncoderProviderOutcome::CtrlEnded => {
					break Ok(None);
				}
				EncoderProviderOutcome::FrameErr(err) => {
					break Err(err);
				}
			}
		}
	}
}

pub struct ScreenStream {
	stream: Option<TcpStream>,
	clock: Clock,
	encoder: VideoEncoder,

	cipher: ChaCha20Poly1305,
	counter: u64,
}
impl ScreenStream {
	pub async fn probe(width: u32, height: u32, fps: u32) -> Result<VideoParams> {
		spawn_blocking(move || VideoEncoder::probe(Path::new(V4L2_DEVICE), width, height, fps))
			.await
			.context("failed to join encoder probe")?
	}

	pub async fn new(
		addr: SocketAddrV6,
		keys: SessionKeys,
		clock: Clock,
		params: VideoParams,
	) -> Result<Self> {
		let encoder = spawn_blocking(move || VideoEncoder::new(Path::new(V4L2_DEVICE), params))
			.await
			.context("failed to join encoder")?
			.context("failed to build encoder")?;

		let socket = TcpSocket::new_v6().context("failed to create socket")?;
		socket.set_nodelay(true).context("failed to set nodelay")?;
		socket
			.set_send_buffer_size(65536)
			.context("failed to set send buffer")?;
		socket
			.set_recv_buffer_size(65536)
			.context("failed to set recv buffer")?;
		let stream = socket
			.connect(SocketAddr::V6(addr))
			.await
			.context("failed to connect")?;

		Ok(Self {
			stream: Some(stream),
			clock,
			encoder,

			cipher: ChaCha20Poly1305::new(&keys.write.into()),
			counter: 0,
		})
	}

	fn seal_frame(&mut self, frame: &mut BytesMut, body_size: u32, timestamp: u64) -> Result<()> {
		let (header, body) = frame.split_at_mut(HEADER_LEN);

		// opcode 0, smallParams and params are all zero, already true of the zeroed header
		header[0..4].copy_from_slice(&body_size.to_le_bytes());
		header[8..16].copy_from_slice(&timestamp.to_le_bytes());

		// the whole 128-byte header is the AAD
		let tag = self
			.cipher
			.encrypt_in_place_detached((&stream_nonce(self.counter)).into(), header, body)
			.map_err(|_| anyhow::anyhow!("failed to seal a screen frame"))?;
		self.counter += 1;

		frame.put_slice(&tag);
		Ok(())
	}

	pub async fn run(mut self, rx: mpsc::UnboundedReceiver<EncoderCommand>) -> Result<()> {
		let stream = self.stream.take().unwrap();
		let (mut read, mut write) = stream.into_split();
		let params = video_config_params(self.encoder.params);

		let force_key_frame = Arc::new(AtomicBool::new(false));
		let mut provider = EncoderProvider {
			force_key_frame: force_key_frame.clone(),
			ctrl: rx,
			params: self.encoder.params,
			current: EncoderProviderInternal::Waiting(WaitingProvider::new(self.encoder.params)),
		};

		let read_task = async {
			let mut scratch = BytesMut::with_capacity(512);
			loop {
				match read
					.read_buf(&mut scratch)
					.await
					.context("failed to read")?
				{
					0 => {
						break;
					}
					len => {
						warn!("head unit sent something on screen: {:?}", &scratch[0..len]);
						scratch.clear();
					}
				}
			}

			anyhow::Ok(())
		};

		let write_task = async {
			let mut sent_config = false;
			loop {
				let mut keyframe = false;
				let Some(EncodedVideoFrame {
					mut bytes,
					len,
					timestamp,
					is_idr,
				}) = self
					.encoder
					.next_frame(&mut provider, || {
						keyframe = force_key_frame.load(Ordering::SeqCst);
						keyframe
					})
					.await
					.context("failed to get next frame")?
				else {
					break anyhow::Ok(());
				};
				if keyframe {
					force_key_frame.store(false, Ordering::SeqCst);
				}

				if !sent_config && let Some(config) = self.encoder.get_config() {
					let mut header = [0u8; HEADER_LEN];
					build_header(
						&mut header,
						OPCODE_VIDEO_CONFIG,
						VIDEO_CONFIG_SMALL_PARAMS,
						self.clock.instant_ntp(timestamp),
						params,
						config.len() as u32,
					);

					let mut buf = BytesMut::with_capacity(HEADER_LEN + config.len());
					buf.put_slice(&header);
					buf.put_slice(&config);

					write
						.write_all(&buf)
						.await
						.context("failed to write config")?;
					sent_config = true;
				}

				self.seal_frame(
					&mut bytes,
					len + TAG_LEN as u32,
					self.clock.instant_ntp(timestamp),
				)
				.context("failed to seal frame")?;

				write
					.write_all(&bytes)
					.await
					.context("failed to write video frame")?;

				if is_idr {
					trace!("sent keyframe");
				}
			}
		};

		select! {
			x = read_task => x.context("read task failed")?,
			x = write_task => x.context("write task failed")?,
		}

		Ok(())
	}
}

fn build_header(
	header: &mut [u8; HEADER_LEN],
	opcode: u8,
	small_params: [u8; 3],
	timestamp: u64,
	params: [u8; VIDEO_PARAMS_LEN],
	body_size: u32,
) {
	header[0..4].copy_from_slice(&body_size.to_le_bytes());
	header[4] = opcode;
	header[5..8].copy_from_slice(&small_params);
	header[8..16].copy_from_slice(&timestamp.to_le_bytes());
	header[16..].copy_from_slice(&params);
}

fn video_config_params(format: VideoParams) -> [u8; VIDEO_PARAMS_LEN] {
	let mut params = [0u8; VIDEO_PARAMS_LEN];
	let width = (format.width as f32).to_le_bytes();
	let height = (format.height as f32).to_le_bytes();

	for offset in [0, 24, 40] {
		params[offset..offset + 4].copy_from_slice(&width);
		params[offset + 4..offset + 8].copy_from_slice(&height);
	}

	params
}
