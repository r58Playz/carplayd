//! Each audio stream gets its own full-duplex data socket, just like the display.
use std::{
	net::{IpAddr, Ipv4Addr},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bytes::{Buf, BufMut, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::{
	net::TcpListener,
	sync::{Mutex, mpsc, watch},
	time::timeout,
};
use tokio_util::{codec::LengthDelimitedCodec, task::AbortOnDropHandle};

use super::ProviderEvent;
use crate::provider::audio::*;

pub struct NetworkAudio {
	pub(super) rx: mpsc::Receiver<AudioEvent>,
	pub(super) tx: mpsc::UnboundedSender<ProviderEvent>,
}
#[async_trait]
impl CarplayProviderAudio for NetworkAudio {
	async fn next_event(&mut self) -> Result<Option<AudioEvent>> {
		Ok(self.rx.recv().await)
	}
	async fn status(&mut self, status: AudioStatus) -> Result<()> {
		self.tx
			.send(ProviderEvent::AudioStatus(status))
			.context("provider control ended")
	}
}

pub struct SinkProgress {
	tx: watch::Sender<Option<AudioProgress>>,
	sent: Arc<AtomicU64>,
}
impl SinkProgress {
	pub fn update(&self, consumed: u64) -> Result<()> {
		ensure!(
			consumed <= self.sent.load(Ordering::Relaxed),
			"microphone progress exceeds sent frames"
		);
		ensure!(
			self.tx.borrow().is_none_or(|x| consumed >= x.consumed),
			"microphone progress went backwards"
		);
		self.tx.send_replace(Some(AudioProgress {
			consumed,
			received: Instant::now(),
		}));
		Ok(())
	}
}

struct StreamLifetime {
	_task: AbortOnDropHandle<()>,
}
struct NetworkOutput {
	rx: mpsc::Receiver<Result<AudioChunk>>,
	_lifetime: Arc<StreamLifetime>,
}
#[async_trait]
impl CarplayProviderAudioOutput for NetworkOutput {
	async fn read(&mut self) -> Result<Option<AudioChunk>> {
		self.rx.recv().await.transpose()
	}
	fn discard_buffered(&mut self) {
		while self.rx.try_recv().is_ok() {}
	}
}
struct NetworkInput {
	tx: mpsc::Sender<AudioChunk>,
	progress: watch::Receiver<Option<AudioProgress>>,
	_lifetime: Arc<StreamLifetime>,
	position: u64,
	frame_bytes: usize,
}
#[async_trait]
impl CarplayProviderAudioInput for NetworkInput {
	async fn write(&mut self, chunk: AudioChunk) -> Result<()> {
		let position = chunk.position;
		let bytes = chunk.data.len();
		self.tx
			.send(chunk)
			.await
			.context("microphone socket closed")?;
		self.position = position + bytes as u64 / self.frame_bytes as u64;
		Ok(())
	}
	fn progress(&self) -> Option<AudioProgress> {
		*self.progress.borrow()
	}
	fn next_position(&self) -> u64 {
		self.position
	}
}

pub async fn open(
	request: &AudioRequest,
	peer: IpAddr,
) -> Result<(u16, AudioEndpoints, SinkProgress)> {
	request.validate()?;
	let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
	let port = listener.local_addr()?.port();
	let (output_tx, output_rx) = mpsc::channel(3);
	let (input_tx, mut input_rx) = mpsc::channel::<AudioChunk>(3);
	let (progress_tx, progress_rx) = watch::channel(None);
	let sent = Arc::new(AtomicU64::new(0));
	let sent_task = sent.clone();
	let playback = request.playback;
	let microphone = request.microphone;
	let task = tokio::spawn(async move {
		let result = async {
			let (socket, addr) = timeout(Duration::from_secs(10), listener.accept())
				.await
				.context("audio connection timed out")??;
			ensure!(
				addr.ip() == peer,
				"audio connection peer does not match control peer"
			);
			socket.set_nodelay(true)?;
			let (read, write) = socket.into_split();
			let mut read = LengthDelimitedCodec::builder()
				.little_endian()
				.max_frame_length(8 + playback.sample_rate as usize / 50 * playback.frame_bytes())
				.new_read(read);
			let mut write = LengthDelimitedCodec::builder()
				.little_endian()
				.new_write(write);
			let receive = async {
				while let Some(packet) = read.next().await {
					let mut packet = packet.context("failed to read PCM packet")?;
					let position = packet
						.try_get_u64_le()
						.context("missing PCM frame position")?;
					let frames = playback.frames(&packet)?;
					ensure!(frames > 0, "empty PCM packet");
					position
						.checked_add(frames as u64)
						.context("PCM frame position overflow")?;
					if output_tx
						.send(Ok(AudioChunk {
							position,
							data: packet.freeze(),
						}))
						.await
						.is_err()
					{
						break;
					}
				}
				anyhow::Ok(())
			};
			let transmit = async {
				if microphone.is_none() {
					return std::future::pending::<Result<()>>().await;
				}
				while let Some(chunk) = input_rx.recv().await {
					let format = microphone.context("microphone was not requested")?;
					let frames = format.frames(&chunk.data)?;
					let end = chunk
						.position
						.checked_add(frames as u64)
						.context("microphone frame position overflow")?;
					let mut packet = BytesMut::with_capacity(8 + chunk.data.len());
					packet.put_u64_le(chunk.position);
					packet.extend_from_slice(&chunk.data);
					// Publish before send: a fast client can report progress immediately.
					sent_task.store(end, Ordering::Relaxed);
					write
						.send(packet.freeze())
						.await
						.context("failed to write microphone PCM")?;
				}
				anyhow::Ok(())
			};
			tokio::select! { x = receive => x, x = transmit => x }
		}
		.await;
		if let Err(error) = result {
			let _ = output_tx.send(Err(error)).await;
		}
	});
	let lifetime = Arc::new(StreamLifetime {
		_task: AbortOnDropHandle::new(task),
	});
	let endpoints = AudioEndpoints {
		output: Arc::new(Mutex::new(Box::new(NetworkOutput {
			rx: output_rx,
			_lifetime: lifetime.clone(),
		}))),
		input: microphone.map(|format| {
			Arc::new(Mutex::new(Box::new(NetworkInput {
				tx: input_tx,
				progress: progress_rx,
				_lifetime: lifetime.clone(),
				position: 0,
				frame_bytes: format.frame_bytes(),
			})
				as Box<dyn CarplayProviderAudioInput>))
		}),
	};
	Ok((
		port,
		endpoints,
		SinkProgress {
			tx: progress_tx,
			sent,
		},
	))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::provider::audio::{AudioClockMode, AudioKind, AudioUse, PcmFormat};
	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::TcpStream,
	};

	#[tokio::test]
	async fn pending_connection_duplex_framing_progress_and_eof() {
		let format = PcmFormat::s16(48000, 1);
		let request = AudioRequest {
			id: 1,
			kind: AudioKind::Main,
			audio_type: AudioUse::Default,
			playback: format,
			microphone: Some(format),
			playback_clock: AudioClockMode::Live,
			microphone_clock: AudioClockMode::Live,
		};
		let (port, endpoints, progress) = open(&request, IpAddr::V4(Ipv4Addr::LOCALHOST))
			.await
			.unwrap();
		assert!(
			timeout(
				Duration::from_millis(10),
				endpoints.output.lock().await.read()
			)
			.await
			.is_err(),
			"open must return before the data connection arrives"
		);
		let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
			.await
			.unwrap();
		let mut bytes = BytesMut::new();
		bytes.put_u32_le(12);
		bytes.put_u64_le(42);
		bytes.extend_from_slice(&[1, 2, 3, 4]);
		for byte in bytes {
			socket.write_all(&[byte]).await.unwrap();
		}
		let chunk = timeout(Duration::from_secs(1), endpoints.output.lock().await.read())
			.await
			.unwrap()
			.unwrap()
			.unwrap();
		assert_eq!(chunk.position, 42);
		assert_eq!(chunk.data.as_ref(), &[1, 2, 3, 4]);
		let mut input = endpoints.input.as_ref().unwrap().lock().await;
		input
			.write(AudioChunk {
				position: 0,
				data: bytes::Bytes::from_static(&[5, 6, 7, 8]),
			})
			.await
			.unwrap();
		let mut received = [0; 16];
		timeout(Duration::from_secs(1), socket.read_exact(&mut received))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(&received[..4], &12u32.to_le_bytes());
		assert_eq!(&received[12..], &[5, 6, 7, 8]);
		assert!(progress.update(3).is_err());
		progress.update(2).unwrap();
		assert_eq!(input.progress().unwrap().consumed, 2);
		assert!(progress.update(1).is_err());
		drop(input);
		socket.shutdown().await.unwrap();
		assert!(
			timeout(Duration::from_secs(1), endpoints.output.lock().await.read())
				.await
				.unwrap()
				.unwrap()
				.is_none()
		);
	}
	#[tokio::test]
	async fn playback_only_does_not_close_when_microphone_is_absent() {
		let request = AudioRequest {
			id: 1,
			kind: AudioKind::Main,
			audio_type: AudioUse::Media,
			playback: PcmFormat::s16(44100, 2),
			microphone: None,
			playback_clock: AudioClockMode::DemandDriven,
			microphone_clock: AudioClockMode::DemandDriven,
		};
		let (port, endpoints, _) = open(&request, IpAddr::V4(Ipv4Addr::LOCALHOST))
			.await
			.unwrap();
		let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
			.await
			.unwrap();
		let mut packet = BytesMut::new();
		packet.put_u32_le(12);
		packet.put_u64_le(0);
		packet.extend_from_slice(&[0; 4]);
		socket.write_all(&packet).await.unwrap();
		assert!(
			timeout(Duration::from_secs(1), endpoints.output.lock().await.read())
				.await
				.unwrap()
				.unwrap()
				.is_some()
		);
	}
}
