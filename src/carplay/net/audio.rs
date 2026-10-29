//! Wired CarPlay audio. AudioSource.c in the decrypted 2024.01.10.0953 firmware:
//! sender 0x42d1c, microphone receive/decrypt 0x42114. PCM is network endian;
//! RTP bytes 4..12 are AAD, followed by ciphertext, tag, and the explicit LE nonce.
use std::{
	collections::BTreeMap,
	net::{Ipv6Addr, SocketAddr, SocketAddrV6},
	time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use bytes::{BufMut, Bytes, BytesMut};
use chacha20poly1305::{AeadInPlace, ChaCha20Poly1305, KeyInit};
use log::{debug, warn};
use tokio::{
	net::UdpSocket,
	sync::watch,
	time::{MissedTickBehavior, sleep_until},
};
use tokio_util::task::AbortOnDropHandle;

use super::{
	CarplayNetSession, NetSessionStreamFutures,
	proto::{
		self,
		command::DeviceCommand,
		feedback::FeedbackStream,
		info::AccessoryInfo,
		modes::SpeechMode,
		setup::{
			StreamRequest, StreamSetupRequest, StreamSetupResponse, StreamTeardown, StreamType,
			TeardownRequest, audio_format,
		},
	},
	rtsp::{Method, Request},
};
use crate::{
	carplay::modes::{ModesConstraint, ModesEntity, ModesResourceId},
	pairing::keys::{SessionKeys, stream_nonce},
	provider::{
		audio::*,
		resampler::{
			ConverterWorker, DriftController, MICROPHONE_TARGET_SECONDS, TARGET_BUFFER_SECONDS,
		},
	},
};

const HEADER_LEN: usize = 12;
const TRAILER_LEN: usize = 24;
pub const MAX_PAYLOAD: usize = 1416;
pub const AUDIO_LATENCY_MS: u32 = 32;
const REORDER_TIME: Duration = Duration::from_millis(16);
/// Late playback wakeups are caught up while the head unit's latency still covers
/// them. Later packets are useless to it, so the schedule restarts instead.
const CATCH_UP: Duration = Duration::from_millis(AUDIO_LATENCY_MS as u64);

impl AudioKind {
	pub fn stream_type(self) -> StreamType {
		match self {
			Self::Main => StreamType::MainAudio,
			Self::Alternate => StreamType::AltAudio,
		}
	}
}

pub fn negotiate(info: &AccessoryInfo, request: &AudioRequest) -> Result<(PcmFormat, u64)> {
	request.validate()?;
	let formats = info.audio_formats.as_deref().unwrap_or_default();
	let kind = request.kind.stream_type();
	// Alternate has no audioType in SETUP; its advertised profile is default.
	let name = if request.kind == AudioKind::Alternate {
		"default"
	} else {
		request.audio_type.as_str()
	};
	let exact = formats
		.iter()
		.any(|x| x.r#type == kind && x.audio_type == name);
	let mut output = 0;
	let mut input = 0;
	for format in formats
		.iter()
		.filter(|x| x.r#type == kind && x.audio_type == if exact { name } else { "compatibility" })
	{
		output |= format.audio_output_formats.unwrap_or(0);
		input |= format.audio_input_formats.unwrap_or(0);
	}
	let mask = if request.microphone.is_some() {
		input & output
	} else {
		output
	};
	let mut candidates = vec![
		(48000, 2),
		(44100, 2),
		(48000, 1),
		(44100, 1),
		(24000, 1),
		(16000, 1),
		(32000, 1),
		(8000, 1),
		(24000, 2),
		(16000, 2),
		(32000, 2),
		(8000, 2),
	];
	if request.kind == AudioKind::Main
		&& matches!(
			request.audio_type,
			AudioUse::SpeechRecognition | AudioUse::Default
		) {
		candidates.sort_by_key(|x| {
			[
				(24000, 1),
				(16000, 1),
				(32000, 1),
				(48000, 1),
				(44100, 1),
				(8000, 1),
			]
			.iter()
			.position(|p| p == x)
			.unwrap_or(10)
		});
	} else if request.kind == AudioKind::Main && request.audio_type == AudioUse::Telephony {
		candidates.sort_by_key(|x| {
			[
				(32000, 1),
				(16000, 1),
				(8000, 1),
				(24000, 1),
				(48000, 1),
				(44100, 1),
			]
			.iter()
			.position(|p| p == x)
			.unwrap_or(10)
		});
	}
	for (rate, channels) in candidates {
		let bit = match (rate, channels) {
			(8000, 1) => audio_format::PCM_8000_16_MONO,
			(8000, 2) => audio_format::PCM_8000_16_STEREO,
			(16000, 1) => audio_format::PCM_16000_16_MONO,
			(16000, 2) => audio_format::PCM_16000_16_STEREO,
			(24000, 1) => audio_format::PCM_24000_16_MONO,
			(24000, 2) => audio_format::PCM_24000_16_STEREO,
			(32000, 1) => audio_format::PCM_32000_16_MONO,
			(32000, 2) => audio_format::PCM_32000_16_STEREO,
			(44100, 1) => audio_format::PCM_44100_16_MONO,
			(44100, 2) => audio_format::PCM_44100_16_STEREO,
			(48000, 1) => audio_format::PCM_48000_16_MONO,
			(48000, 2) => audio_format::PCM_48000_16_STEREO,
			_ => unreachable!(),
		};
		if mask & bit != 0 {
			return Ok((PcmFormat::s16(rate, channels), bit));
		}
	}
	anyhow::bail!("head unit has no supported wired PCM format for {name}")
}

pub struct AudioPacketCodec {
	read: ChaCha20Poly1305,
	write: ChaCha20Poly1305,
	payload_type: u8,
	sequence: u16,
	timestamp: u32,
	nonce: u64,
	replay: ReplayWindow,
}
#[derive(Default)]
struct ReplayWindow {
	highest: Option<u64>,
	mask: u64,
}
impl ReplayWindow {
	fn accept(&mut self, nonce: u64) -> bool {
		let Some(highest) = self.highest else {
			self.highest = Some(nonce);
			self.mask = 1;
			return true;
		};
		if nonce > highest {
			self.mask = if nonce - highest >= 64 {
				1
			} else {
				(self.mask << (nonce - highest)) | 1
			};
			self.highest = Some(nonce);
			true
		} else {
			let distance = highest - nonce;
			if distance >= 64 || self.mask & (1 << distance) != 0 {
				false
			} else {
				self.mask |= 1 << distance;
				true
			}
		}
	}
}
impl AudioPacketCodec {
	pub fn new(keys: SessionKeys, kind: AudioKind) -> Self {
		Self {
			read: ChaCha20Poly1305::new(&keys.read.into()),
			write: ChaCha20Poly1305::new(&keys.write.into()),
			payload_type: i64::from(kind.stream_type()) as u8,
			sequence: 0,
			timestamp: 0,
			nonce: 0,
			replay: ReplayWindow::default(),
		}
	}
	pub fn seal(&mut self, pcm: &[u8], format: PcmFormat) -> Result<Bytes> {
		let frames = format.frames(pcm)?;
		ensure!(
			frames > 0 && pcm.len() <= MAX_PAYLOAD,
			"invalid audio payload size"
		);
		ensure!(self.nonce != u64::MAX, "audio nonce exhausted");
		let mut packet = BytesMut::with_capacity(HEADER_LEN + pcm.len() + TRAILER_LEN);
		packet.put_u8(0x80);
		packet.put_u8(self.payload_type);
		packet.put_u16(self.sequence);
		packet.put_u32(self.timestamp);
		packet.put_u32(0);
		for sample in pcm.chunks_exact(2) {
			packet.extend_from_slice(&[sample[1], sample[0]]);
		}
		let (header, body) = packet.split_at_mut(HEADER_LEN);
		let tag = self
			.write
			.encrypt_in_place_detached((&stream_nonce(self.nonce)).into(), &header[4..12], body)
			.map_err(|_| anyhow::anyhow!("failed to encrypt audio"))?;
		packet.extend_from_slice(&tag);
		packet.put_u64_le(self.nonce);
		self.sequence = self.sequence.wrapping_add(1);
		self.timestamp = self.timestamp.wrapping_add(frames as u32);
		self.nonce += 1;
		Ok(packet.freeze())
	}
	pub fn open(&mut self, packet: &[u8], format: PcmFormat) -> Result<(u16, u32, Bytes)> {
		ensure!(
			packet.len() > HEADER_LEN + TRAILER_LEN && packet.len() <= 1472,
			"invalid audio packet length"
		);
		ensure!(
			packet[0] == 0x80 && packet[1] & 0x7f == self.payload_type,
			"invalid audio RTP header"
		);
		let end = packet.len() - TRAILER_LEN;
		format.frames(&packet[HEADER_LEN..end])?;
		let nonce = u64::from_le_bytes(packet[packet.len() - 8..].try_into().unwrap());
		let mut pcm = packet[HEADER_LEN..end].to_vec();
		self.read
			.decrypt_in_place_detached(
				(&stream_nonce(nonce)).into(),
				&packet[4..12],
				&mut pcm,
				packet[end..end + 16].into(),
			)
			.map_err(|_| anyhow::anyhow!("audio authentication failed"))?;
		ensure!(self.replay.accept(nonce), "duplicate or stale audio nonce");
		for sample in pcm.chunks_exact_mut(2) {
			sample.swap(0, 1);
		}
		Ok((
			u16::from_be_bytes(packet[2..4].try_into().unwrap()),
			u32::from_be_bytes(packet[4..8].try_into().unwrap()),
			pcm.into(),
		))
	}
}

struct MicPacket {
	timestamp: u32,
	pcm: Bytes,
	arrived: Instant,
}
#[derive(Default)]
struct MicrophoneQueue {
	next: Option<u64>,
	timestamp: Option<u32>,
	packets: BTreeMap<u64, MicPacket>,
}
impl MicrophoneQueue {
	fn push(&mut self, sequence: u16, timestamp: u32, pcm: Bytes, now: Instant) {
		let expected = *self.next.get_or_insert(sequence as u64);
		let delta = sequence.wrapping_sub(expected as u16) as i16 as i64;
		if (-64..0).contains(&delta) {
			return;
		}
		if !(0..64).contains(&delta) {
			// Recover after a long outage instead of waiting for the 16-bit
			// sequence number to make another complete lap.
			self.packets.clear();
			let sequence = expected + sequence.wrapping_sub(expected as u16) as u64;
			self.next = Some(sequence);
			self.packets.insert(
				sequence,
				MicPacket {
					timestamp,
					pcm,
					arrived: now,
				},
			);
			return;
		}
		self.packets
			.entry(expected + delta as u64)
			.or_insert(MicPacket {
				timestamp,
				pcm,
				arrived: now,
			});
	}
	fn pop(&mut self, format: PcmFormat, now: Instant) -> Option<(Bytes, usize, bool)> {
		let (&first, packet) = self.packets.first_key_value()?;
		if first != self.next? && now.duration_since(packet.arrived) < REORDER_TIME {
			return None;
		}
		let packet = self.packets.pop_first()?.1;
		self.next = Some(first + 1);
		let gap = self
			.timestamp
			.map_or(0, |last| packet.timestamp.wrapping_sub(last));
		let discontinuity = gap > format.sample_rate * 16 / 1000;
		let frames = packet.pcm.len() / format.frame_bytes();
		self.timestamp = Some(packet.timestamp.wrapping_add(frames as u32));
		Some((packet.pcm, gap as usize, discontinuity))
	}
}

#[derive(Clone, Copy)]
pub struct AudioControl {
	pub rate: f64,
	pub epoch: u64,
	pub stop: bool,
}
impl AudioControl {
	pub fn update_rate(&mut self, feedback: &FeedbackStream, nominal: u32) {
		if let Some(rate) = feedback.sr
			&& rate.is_finite()
			&& (nominal as f64 * 0.995..=nominal as f64 * 1.005).contains(&rate)
		{
			self.rate = rate;
		}
	}
}

pub struct RunningAudio {
	pub id: u64,
	pub connection_id: i64,
	pub format: PcmFormat,
	pub control: watch::Sender<AudioControl>,
	pub task: AbortOnDropHandle<Result<()>>,
	pub last_clock: Option<(f64, f64)>,
}

pub struct RequestedAudio {
	pub request: AudioRequest,
	pub endpoints: AudioEndpoints,
	pub format: PcmFormat,
	pub format_bit: u64,
	pub ducked: bool,
	pub remote_setup: bool,
	pub started: bool,
}

pub struct AudioStream {
	output: UdpSocket,
	microphone: Option<UdpSocket>,
}
impl AudioStream {
	pub async fn bind(input: bool) -> Result<Self> {
		let output = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).await?;
		let microphone = if input {
			Some(UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).await?)
		} else {
			None
		};
		Ok(Self { output, microphone })
	}
	pub fn microphone_port(&self) -> Result<Option<u16>> {
		self.microphone
			.as_ref()
			.map(|x| x.local_addr().map(|x| x.port()))
			.transpose()
			.map_err(Into::into)
	}
	pub async fn connect(&self, addr: SocketAddrV6) -> Result<()> {
		self.output
			.connect(addr)
			.await
			.context("failed to connect audio UDP")
	}
	pub async fn run(
		self,
		addr: SocketAddrV6,
		keys: SessionKeys,
		request: AudioRequest,
		wire: PcmFormat,
		endpoints: AudioEndpoints,
		control: watch::Receiver<AudioControl>,
		resumed: bool,
	) -> Result<()> {
		let output_worker = ConverterWorker::new(
			request.playback,
			wire,
			request.playback_clock == AudioClockMode::Live,
		);
		let microphone_worker = request
			.microphone
			.map(|format| ConverterWorker::new(wire, format, true));
		let output = self.playback(
			keys,
			&request,
			wire,
			&endpoints,
			control.clone(),
			&output_worker,
			resumed,
		);
		let microphone = async {
			let Some(socket) = &self.microphone else {
				return std::future::pending::<Result<()>>().await;
			};
			self.record(
				socket,
				addr,
				keys,
				&request,
				wire,
				&endpoints,
				control,
				microphone_worker.as_ref().unwrap(),
			)
			.await
		};
		let result = tokio::select! { x = output => x, x = microphone => x };
		output_worker.shutdown().await?;
		if let Some(worker) = microphone_worker {
			worker.shutdown().await?;
		}
		result
	}

	async fn playback(
		&self,
		keys: SessionKeys,
		request: &AudioRequest,
		wire: PcmFormat,
		endpoints: &AudioEndpoints,
		mut control: watch::Receiver<AudioControl>,
		converter: &ConverterWorker,
		resumed: bool,
	) -> Result<()> {
		let mut codec = AudioPacketCodec::new(keys, request.kind);
		let mut provider = endpoints.output.lock().await;
		if resumed && request.playback_clock == AudioClockMode::Live {
			provider.discard_buffered();
		}
		let mut next_position = None;
		let mut eof = false;
		let mut buffered = 0.0;
		let mut primed = false;
		let mut drift = DriftController::new(TARGET_BUFFER_SECONDS);
		let mut deadline =
			tokio::time::Instant::now() + Duration::from_secs_f64(TARGET_BUFFER_SECONDS);
		let nominal_ratio = wire.sample_rate as f64 / request.playback.sample_rate as f64;
		let mut epoch = control.borrow().epoch;
		let silence = Bytes::from(vec![0; wire.block_frames() * wire.frame_bytes()]);
		// Tells a provider outrunning real time apart from this task falling behind.
		let mut window = Instant::now();
		let (mut received, mut played, mut late) = (0, 0, 0);
		let (mut behind, mut worst) = (Duration::ZERO, Duration::ZERO);
		// How much of the cushion delivery jitter actually uses, for tuning the target.
		let mut report = Instant::now();
		let mut lowest = f64::INFINITY;
		loop {
			tokio::select! {
				chunk = provider.read(), if !eof && (request.playback_clock == AudioClockMode::Live || buffered < TARGET_BUFFER_SECONDS) => {
					match chunk? {
						Some(chunk) => {
							if next_position.is_some_and(|x| x != chunk.position) { converter.reset().await?; drift = DriftController::new(TARGET_BUFFER_SECONDS); primed = false; }
							let frames = request.playback.frames(&chunk.data)? as u64;
							next_position = Some(chunk.position.checked_add(frames).context("PCM position overflow")?);
							received += frames;
							let (queued, dropped) = converter.push(chunk.data).await?;
							buffered = queued;
							if dropped {
								drift = DriftController::new(TARGET_BUFFER_SECONDS);
								warn!(
									"dropped stale playback PCM for audio {}: in {:.0} ms the provider sent {:.0} ms and {:.0} ms played; {} late wakeups lost {:.0} ms (worst {:.0} ms late)",
									request.id,
									window.elapsed().as_secs_f64() * 1000.0,
									received as f64 * 1000.0 / request.playback.sample_rate as f64,
									played as f64 * 1000.0 / wire.sample_rate as f64,
									late,
									behind.as_secs_f64() * 1000.0,
									worst.as_secs_f64() * 1000.0,
								);
								(window, received, played, late) = (Instant::now(), 0, 0, 0);
								(behind, worst) = (Duration::ZERO, Duration::ZERO);
							}
						}
						None => eof = true,
					}
				}
				changed = control.changed() => {
					changed.context("audio control closed")?;
					if control.borrow().stop { return Ok(()); }
					let new_epoch = control.borrow().epoch;
					if new_epoch != epoch { provider.discard_buffered(); converter.reset().await?; buffered = 0.0; primed = false; drift = DriftController::new(TARGET_BUFFER_SECONDS); epoch = new_epoch; }
				}
				_ = sleep_until(deadline) => {
					let rate = control.borrow().rate;
					let duration = wire.block_frames() as f64 / rate;
					// Rebuffer to the target after starting or running dry. Resuming on the
					// first chunk leaves no cushion, so the next provider stall underruns again.
					primed |= eof || buffered >= TARGET_BUFFER_SECONDS;
					let pcm = if primed {
						let ratio = if request.playback_clock == AudioClockMode::Live {
							rate / request.playback.sample_rate as f64 * drift.update(buffered, duration)
						} else { nominal_ratio };
						let (block, queued) = converter.read(ratio, eof).await?;
						buffered = queued;
						lowest = lowest.min(buffered);
						match block {
							Some(block) => block,
							None if eof => return Ok(()),
							None => { debug!("playback underrun for audio {}, rebuffering", request.id); primed = false; silence.clone() }
						}
					} else { silence.clone() };
					for piece in pcm.chunks(MAX_PAYLOAD / wire.frame_bytes() * wire.frame_bytes()) {
						self.output.send(&codec.seal(piece, wire)?).await.context("failed to send audio packet")?;
					}
					played += pcm.len() / wire.frame_bytes();
					deadline += Duration::from_secs_f64(duration);
					// A wakeup a few ms late is routine under load. Skipping it would drop
					// that time from the stream for good, leaving its PCM queued here; the
					// next iterations send the missed blocks instead. Only a backlog the head
					// unit could no longer play is skipped.
					let now = tokio::time::Instant::now();
					if deadline + CATCH_UP < now {
						late += 1;
						worst = worst.max(now - deadline);
						behind += now - deadline;
						deadline = now + Duration::from_secs_f64(duration);
					}
					if report.elapsed() >= Duration::from_secs(10) {
						if lowest.is_finite() {
							debug!(
								"audio {} cushion over 10 s: lowest {:.0} ms of a {:.0} ms target",
								request.id,
								lowest * 1000.0,
								TARGET_BUFFER_SECONDS * 1000.0,
							);
						}
						(report, lowest) = (Instant::now(), f64::INFINITY);
					}
				}
			}
		}
	}

	async fn record(
		&self,
		socket: &UdpSocket,
		peer: SocketAddrV6,
		keys: SessionKeys,
		request: &AudioRequest,
		wire: PcmFormat,
		endpoints: &AudioEndpoints,
		mut control: watch::Receiver<AudioControl>,
		converter: &ConverterWorker,
	) -> Result<()> {
		let format = request.microphone.unwrap();
		let mut provider = endpoints
			.input
			.as_ref()
			.context("missing microphone endpoint")?
			.lock()
			.await;
		let mut codec = AudioPacketCodec::new(keys, AudioKind::Main);
		let mut reorder = MicrophoneQueue::default();
		let mut buffer = [0u8; 2048];
		let mut position = provider.next_position();
		let mut drift = DriftController::new(MICROPHONE_TARGET_SECONDS);
		let mut epoch = control.borrow().epoch;
		let mut timer = tokio::time::interval(Duration::from_millis(5));
		timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
		loop {
			tokio::select! {
				packet = socket.recv_from(&mut buffer) => {
					let (len, addr) = packet.context("failed to receive microphone audio")?;
					if !matches!(addr, SocketAddr::V6(addr) if addr.ip() == peer.ip() && (addr.scope_id() == 0 || addr.scope_id() == peer.scope_id())) { continue; }
					match codec.open(&buffer[..len], wire) {
						Ok((sequence, timestamp, pcm)) => reorder.push(sequence, timestamp, pcm, Instant::now()),
						Err(error) => debug!("discarded microphone packet: {error}"),
					}
				}
				changed = control.changed() => {
					changed.context("audio control closed")?;
					if control.borrow().stop { return Ok(()); }
					let new_epoch = control.borrow().epoch;
					if new_epoch != epoch { converter.reset().await?; reorder = MicrophoneQueue::default(); drift = DriftController::new(MICROPHONE_TARGET_SECONDS); epoch = new_epoch; }
				}
				_ = timer.tick() => {}
			}
			while let Some((pcm, gap, discontinuity)) = reorder.pop(wire, Instant::now()) {
				if discontinuity {
					converter.reset().await?;
					drift = DriftController::new(MICROPHONE_TARGET_SECONDS);
					let skipped = if gap < (1 << 31) {
						(gap as u64 * format.sample_rate as u64 / wire.sample_rate as u64).max(1)
					} else {
						1
					};
					position = position
						.checked_add(skipped)
						.context("microphone discontinuity overflow")?;
				}
				if gap > 0 && !discontinuity {
					converter
						.push(Bytes::from(vec![0; gap * wire.frame_bytes()]))
						.await?;
				}
				// Head units may use a block size larger than a provider block.
				for piece in pcm.chunks(wire.sample_rate as usize / 50 * wire.frame_bytes()) {
					converter.push(Bytes::copy_from_slice(piece)).await?;
				}
				loop {
					let correction = if request.microphone_clock == AudioClockMode::Live {
						provider
							.progress()
							.filter(|p| p.received.elapsed() < Duration::from_secs(2))
							.map_or(1.0, |p| {
								drift.update(
									position.saturating_sub(p.consumed) as f64
										/ format.sample_rate as f64,
									format.block_frames() as f64 / format.sample_rate as f64,
								)
							})
					} else {
						1.0
					};
					let ratio = format.sample_rate as f64 / control.borrow().rate * correction;
					let (block, _) = converter.read(ratio, false).await?;
					let Some(block) = block else {
						break;
					};
					let frames = format.frames(&block)?;
					// A stalled microphone consumer must never block playback. Its queue is
					// bounded separately, and a dropped block leaves a visible position gap.
					let chunk = AudioChunk {
						position,
						data: block,
					};
					position = position
						.checked_add(frames as u64)
						.context("microphone position overflow")?;
					match tokio::time::timeout(Duration::from_millis(5), provider.write(chunk))
						.await
					{
						Ok(result) => result?,
						Err(_) => {
							warn!("microphone consumer stalled for audio {}", request.id);
							converter.reset().await?;
							break;
						}
					}
				}
			}
		}
	}
}

impl CarplayNetSession {
	fn audio_notice(
		&mut self,
		id: u64,
		state: AudioState,
		format: Option<PcmFormat>,
		detail: Option<String>,
	) {
		self.audio_status.push(AudioStatus {
			id,
			state,
			wire_format: format,
			detail,
		});
	}

	async fn teardown_audio(&mut self, kind: AudioKind) -> Result<()> {
		let body = proto::to_binary(&TeardownRequest {
			streams: Some(vec![StreamTeardown {
				r#type: kind.stream_type(),
			}]),
		})?;
		let response = self
			.control
			.request(Request::new(Method::Teardown, &self.rtsp_url).body(proto::CONTENT_TYPE, body))
			.await?;
		ensure!(
			response.is_success(),
			"audio TEARDOWN refused: {}",
			response.status
		);
		Ok(())
	}

	async fn stop_audio_task(
		&mut self,
		kind: AudioKind,
		streams: &mut NetSessionStreamFutures,
	) -> Result<()> {
		if let Some(mut stream) = streams.audio.remove(&kind) {
			stream.control.send_modify(|x| x.stop = true);
			match tokio::time::timeout(Duration::from_secs(2), &mut stream.task).await {
				Ok(Ok(Ok(()))) => {}
				result => debug!("audio {} stopped: {result:?}", stream.id),
			}
		}
		let configured = self
			.audio_requests
			.get_mut(&kind)
			.is_some_and(|x| std::mem::take(&mut x.remote_setup));
		if configured {
			self.teardown_audio(kind).await?;
		}
		Ok(())
	}

	async fn stop_audio(
		&mut self,
		kind: AudioKind,
		streams: &mut NetSessionStreamFutures,
		state: AudioState,
		detail: Option<String>,
	) -> Result<()> {
		let stopped = self.stop_audio_task(kind, streams).await;
		let Some(request) = self.audio_requests.remove(&kind) else {
			return stopped;
		};
		if request.ducked {
			if let Err(error) = self
				.command(DeviceCommand::UnduckAudio { duration_ms: 100 })
				.await
			{
				warn!("failed to restore ducked audio: {error:#}");
			}
		}
		if kind == AudioKind::Main {
			self.modes.resource(ModesResourceId::MainAudio, |x| {
				x.release(ModesEntity::Controller, ModesEntity::Accessory);
				Ok(())
			})?;
			self.modes
				.set_speech(ModesEntity::Controller, SpeechMode::None)?;
			self.modes.set_phone_call(ModesEntity::Controller, false)?;
		} else {
			self.modes.set_turn_by_turn(ModesEntity::Controller, false);
		}
		self.command(DeviceCommand::ModesChanged(self.modes.current_modes()))
			.await?;
		self.audio_notice(request.request.id, state, Some(request.format), detail);
		stopped
	}

	async fn start_audio_stream(
		&mut self,
		kind: AudioKind,
		streams: &mut NetSessionStreamFutures,
	) -> Result<()> {
		let requested = self.audio_requests.get(&kind).unwrap();
		let request = requested.request.clone();
		let format = requested.format;
		let format_bit = requested.format_bit;
		let endpoints = requested.endpoints.clone();
		let resumed = requested.started;
		let stream = AudioStream::bind(request.microphone.is_some()).await?;
		let connection_id = self.next_connection_id;
		self.next_connection_id = self
			.next_connection_id
			.checked_add(1)
			.context("stream connection IDs exhausted")?;
		let descriptor = StreamRequest {
			r#type: kind.stream_type(),
			input: request.microphone.map(|_| true),
			data_port: stream.microphone_port()?,
			audio_type: (kind == AudioKind::Main).then(|| request.audio_type.as_str().to_owned()),
			..StreamRequest::main_audio(
				connection_id,
				format_bit,
				AUDIO_LATENCY_MS,
				request.audio_type.as_str(),
			)
		};
		// Even a malformed/lost response can leave a configured stream remotely.
		self.audio_requests.get_mut(&kind).unwrap().remote_setup = true;
		let response = self
			.control
			.req::<StreamSetupResponse>(
				Method::Setup,
				&self.rtsp_url,
				&StreamSetupRequest {
					streams: vec![descriptor],
				},
			)
			.await?;
		let response = response
			.streams
			.as_deref()
			.unwrap_or_default()
			.iter()
			.find(|x| x.r#type == kind.stream_type())
			.context("no matching audio stream in SETUP response")?;
		ensure!(
			response
				.stream_connection_id
				.is_none_or(|id| id == connection_id),
			"audio SETUP connection ID mismatch"
		);
		let port = response
			.data_port
			.filter(|&port| port != 0)
			.context("no audio data port in SETUP response")?;
		let addr = SocketAddrV6::new(
			*self.server.addr.ip(),
			port,
			self.server.addr.flowinfo(),
			self.server.addr.scope_id(),
		);
		stream.connect(addr).await?;
		let (control, rx) = watch::channel(AudioControl {
			rate: format.sample_rate as f64,
			epoch: 0,
			stop: false,
		});
		let keys = SessionKeys::data_stream(&self.shared_secret, connection_id as u64);
		let last_clock = response
			.timestamp
			.zip(response.sample_time)
			.filter(|(time, sample)| time.is_finite() && sample.is_finite());
		let id = request.id;
		let task = AbortOnDropHandle::new(tokio::spawn(
			stream.run(addr, keys, request, format, endpoints, rx, resumed),
		));
		self.audio_requests.get_mut(&kind).unwrap().started = true;
		streams.audio.insert(
			kind,
			RunningAudio {
				id,
				connection_id,
				format,
				control,
				task,
				last_clock,
			},
		);
		self.audio_notice(id, AudioState::Active, Some(format), None);
		Ok(())
	}

	pub(super) async fn reconcile_audio(
		&mut self,
		streams: &mut NetSessionStreamFutures,
	) -> Result<()> {
		for kind in [AudioKind::Main, AudioKind::Alternate] {
			if !self.audio_requests.contains_key(&kind) {
				continue;
			}
			let allowed = kind == AudioKind::Alternate
				|| self
					.modes
					.inspect_resource(ModesResourceId::MainAudio, |x| x.owner())
					== ModesEntity::Controller;
			if !allowed && streams.audio.contains_key(&kind) {
				self.stop_audio_task(kind, streams).await?;
				self.modes
					.set_speech(ModesEntity::Controller, SpeechMode::None)?;
				self.modes.set_phone_call(ModesEntity::Controller, false)?;
				self.command(DeviceCommand::ModesChanged(self.modes.current_modes()))
					.await?;
				let request = &self.audio_requests[&kind];
				self.audio_notice(
					request.request.id,
					AudioState::Suspended,
					Some(request.format),
					Some("main audio belongs to the accessory".into()),
				);
			} else if allowed && !streams.audio.contains_key(&kind) {
				if let Err(error) = self.start_audio_stream(kind, streams).await {
					self.stop_audio(kind, streams, AudioState::Error, Some(format!("{error:#}")))
						.await?;
				}
			}
		}
		Ok(())
	}

	pub(super) async fn audio_finished(
		&mut self,
		kind: AudioKind,
		id: u64,
		result: Result<()>,
		streams: &mut NetSessionStreamFutures,
	) -> Result<()> {
		if self
			.audio_requests
			.get(&kind)
			.is_none_or(|x| x.request.id != id)
		{
			return Ok(());
		}
		let (state, detail) = match result {
			Ok(()) => (AudioState::Stopped, None),
			Err(error) => (AudioState::Error, Some(format!("{error:#}"))),
		};
		self.stop_audio(kind, streams, state, detail).await
	}

	pub(super) async fn handle_audio_event(
		&mut self,
		event: AudioEvent,
		streams: &mut NetSessionStreamFutures,
	) -> Result<()> {
		match event {
			AudioEvent::Start(request, endpoints) => {
				let id = request.id;
				let mut installed = false;
				let result: Result<()> = async {
					ensure!(
						!self.audio_requests.values().any(|x| x.request.id == id),
						"audio request ID already active"
					);
					let (format, format_bit) = negotiate(&self.info, &request)?;
					if request.kind == AudioKind::Main && request.audio_type == AudioUse::Telephony
					{
						ensure!(
							self.modes
								.phone_call()
								.is_none_or(|entity| entity == ModesEntity::Controller),
							"head unit already has a phone call"
						);
					}
					if self.audio_requests.contains_key(&request.kind) {
						self.stop_audio(request.kind, streams, AudioState::Stopped, None)
							.await?;
					}
					if request.kind == AudioKind::Main {
						self.modes
							.resource(ModesResourceId::MainAudio, |resource| {
								if request.audio_type.temporary() {
									resource.borrow(
										ModesEntity::Controller,
										ModesConstraint::UserInitiated,
										match request.audio_type {
											AudioUse::Telephony => ModesConstraint::Never,
											AudioUse::SpeechRecognition => {
												ModesConstraint::UserInitiated
											}
											_ => ModesConstraint::Anytime,
										},
									)
								} else {
									resource.take(
										ModesEntity::Controller,
										ModesConstraint::UserInitiated,
										ModesConstraint::UserInitiated,
										ModesConstraint::Anytime,
									)
								}
							})?;
					}
					self.audio_requests.insert(
						request.kind,
						RequestedAudio {
							request,
							endpoints,
							format,
							format_bit,
							ducked: false,
							remote_setup: false,
							started: false,
						},
					);
					installed = true;
					self.modes_changed(streams.has_screen(), streams, self.modes.current_modes())
						.await
				}
				.await;
				if let Err(error) = result {
					if installed {
						if let Some(kind) = self
							.audio_requests
							.iter()
							.find(|(_, x)| x.request.id == id)
							.map(|(&kind, _)| kind)
						{
							let _ = self
								.stop_audio(kind, streams, AudioState::Stopped, None)
								.await;
						}
					}
					self.audio_notice(id, AudioState::Error, None, Some(format!("{error:#}")));
				}
			}
			AudioEvent::AppState {
				speech,
				phone_call,
				turn_by_turn,
			} => {
				let previous = self.modes.clone();
				let result: Result<()> = async {
					ensure!(
						(matches!(speech, AudioSpeech::None) && !phone_call)
							|| (self.audio_requests.contains_key(&AudioKind::Main)
								&& self
									.modes
									.inspect_resource(ModesResourceId::MainAudio, |x| x.owner())
									== ModesEntity::Controller),
						"speech/call state requires owned main audio"
					);
					self.modes.set_speech(
						ModesEntity::Controller,
						match speech {
							AudioSpeech::None => SpeechMode::None,
							AudioSpeech::Speaking => SpeechMode::Speaking,
							AudioSpeech::Listening => SpeechMode::RecognizingSpeech,
						},
					)?;
					self.modes
						.set_phone_call(ModesEntity::Controller, phone_call)?;
					self.modes
						.set_turn_by_turn(ModesEntity::Controller, turn_by_turn);
					self.command(DeviceCommand::ModesChanged(self.modes.current_modes()))
						.await
				}
				.await;
				if let Err(error) = result {
					self.modes = previous;
					self.audio_notice(0, AudioState::Error, None, Some(format!("{error:#}")));
				}
			}
			event => {
				let id = match event {
					AudioEvent::Stop(id)
					| AudioEvent::Flush(id)
					| AudioEvent::Duck { id, .. }
					| AudioEvent::Unduck { id, .. } => id,
					_ => unreachable!(),
				};
				let Some(kind) = self
					.audio_requests
					.iter()
					.find(|(_, x)| x.request.id == id)
					.map(|(&kind, _)| kind)
				else {
					if matches!(event, AudioEvent::Stop(_)) {
						self.audio_notice(id, AudioState::Stopped, None, None);
						return Ok(());
					}
					self.audio_notice(
						id,
						AudioState::Error,
						None,
						Some("no audio stream with that ID".into()),
					);
					return Ok(());
				};
				match event {
					AudioEvent::Stop(_) => {
						self.stop_audio(kind, streams, AudioState::Stopped, None)
							.await?
					}
					AudioEvent::Flush(_) => {
						if let Some(stream) = streams.audio.get(&kind) {
							stream
								.control
								.send_modify(|x| x.epoch = x.epoch.wrapping_add(1));
						}
						self.command(DeviceCommand::FlushAudio).await?;
					}
					AudioEvent::Duck {
						duration_ms,
						volume,
						..
					} => {
						self.command(DeviceCommand::DuckAudio {
							duration_ms,
							volume,
						})
						.await?;
						self.audio_requests.get_mut(&kind).unwrap().ducked = true;
					}
					AudioEvent::Unduck { duration_ms, .. } => {
						self.command(DeviceCommand::UnduckAudio { duration_ms })
							.await?;
						self.audio_requests.get_mut(&kind).unwrap().ducked = false;
					}
					_ => unreachable!(),
				}
			}
		}
		Ok(())
	}

	pub(super) async fn close_audio(&mut self, streams: &mut NetSessionStreamFutures) {
		for kind in [AudioKind::Alternate, AudioKind::Main] {
			if let Err(error) = self
				.stop_audio(kind, streams, AudioState::Stopped, None)
				.await
			{
				warn!("audio shutdown failed: {error:#}");
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{super::proto::info::AudioFormat, *};
	use crate::carplay::{
		modes::{ModesMachine, ModesOwnerState},
		net::{conn::HeadUnitTcpConnection, head_unit::HeadUnitServer, rtsp, timing::Clock},
	};
	use async_trait::async_trait;
	use futures::{SinkExt, StreamExt};
	use std::{collections::HashMap, sync::Arc};
	use tokio::{net::TcpListener, sync::Mutex};
	use tokio_util::codec::Framed;

	fn keys() -> SessionKeys {
		SessionKeys {
			read: [7; 32],
			write: [7; 32],
		}
	}
	fn request(id: u64, kind: AudioKind, audio_type: AudioUse) -> AudioRequest {
		AudioRequest {
			id,
			kind,
			audio_type,
			playback: PcmFormat::s16(44100, 2),
			microphone: None,
			playback_clock: AudioClockMode::Live,
			microphone_clock: AudioClockMode::Live,
		}
	}
	fn info() -> AccessoryInfo {
		let mask = audio_format::PCM_48000_16_STEREO
			| audio_format::PCM_24000_16_MONO
			| audio_format::PCM_16000_16_MONO;
		AccessoryInfo {
			audio_formats: Some(vec![
				AudioFormat {
					r#type: StreamType::MainAudio,
					audio_type: "compatibility".into(),
					audio_output_formats: Some(mask),
					audio_input_formats: Some(mask & !audio_format::PCM_48000_16_STEREO),
				},
				AudioFormat {
					r#type: StreamType::AltAudio,
					audio_type: "compatibility".into(),
					audio_output_formats: Some(audio_format::PCM_48000_16_STEREO),
					audio_input_formats: None,
				},
			]),
			..Default::default()
		}
	}
	#[test]
	fn packet_matches_independent_chacha_fixture() {
		// Python cryptography ChaCha20Poly1305, key 07*32, nonce 00*12,
		// plaintext 1234edcc and eight zero AAD bytes. No codec round-trip oracle.
		let expected =
			"80640000000000000000000025b0c4c0863bc8c342155d27e7413c20def78d2c0000000000000000";
		let expected: Vec<_> = expected
			.as_bytes()
			.chunks_exact(2)
			.map(|x| u8::from_str_radix(std::str::from_utf8(x).unwrap(), 16).unwrap())
			.collect();
		let pcm = [0x34, 0x12, 0xcc, 0xed];
		let packet = AudioPacketCodec::new(keys(), AudioKind::Main)
			.seal(&pcm, PcmFormat::s16(16000, 1))
			.unwrap();
		assert_eq!(packet.as_ref(), expected);
		let mut receiver = AudioPacketCodec::new(keys(), AudioKind::Main);
		assert_eq!(
			receiver
				.open(&packet, PcmFormat::s16(16000, 1))
				.unwrap()
				.2
				.as_ref(),
			pcm
		);
		assert!(receiver.open(&packet, PcmFormat::s16(16000, 1)).is_err());
	}
	#[test]
	fn loss_reorder_authentication_and_counter_wrap() {
		let format = PcmFormat::s16(48000, 2);
		let mut sender = AudioPacketCodec::new(keys(), AudioKind::Alternate);
		sender.sequence = u16::MAX;
		sender.timestamp = u32::MAX;
		let first = sender.seal(&[0; 4], format).unwrap();
		let second = sender.seal(&[0; 4], format).unwrap();
		assert_eq!(&first[2..8], &[255, 255, 255, 255, 255, 255]);
		assert_eq!(&second[2..8], &[0; 6]);
		let mut receiver = AudioPacketCodec::new(keys(), AudioKind::Alternate);
		let mut tampered = second.to_vec();
		tampered[4] ^= 1;
		assert!(receiver.open(&tampered, format).is_err());
		assert!(receiver.open(&second, format).is_ok());
		assert!(receiver.open(&first, format).is_ok());
		for _ in 0..80 {
			let packet = sender.seal(&[0; 4], format).unwrap();
			receiver.open(&packet, format).unwrap();
		}
		assert!(receiver.open(&first, format).is_err());
		assert!(
			AudioPacketCodec::new(
				SessionKeys {
					read: [8; 32],
					write: [8; 32]
				},
				AudioKind::Alternate
			)
			.open(&second, format)
			.is_err()
		);
	}
	#[test]
	fn microphone_reorders_wraps_and_bounds_silence() {
		let now = Instant::now();
		let format = PcmFormat::s16(16000, 1);
		let mut queue = MicrophoneQueue::default();
		queue.push(65535, u32::MAX - 79, Bytes::from(vec![0; 160]), now);
		assert_eq!(queue.pop(format, now).unwrap().1, 0);
		queue.push(1, 80, Bytes::from(vec![0; 160]), now);
		assert!(queue.pop(format, now).is_none());
		queue.push(0, 0, Bytes::from(vec![0; 160]), now);
		assert_eq!(queue.pop(format, now).unwrap().1, 0);
		assert_eq!(queue.pop(format, now).unwrap().1, 0);
		queue.push(3, 240, Bytes::from(vec![0; 160]), now);
		assert_eq!(queue.pop(format, now + REORDER_TIME).unwrap().1, 80);
		queue.push(5, 999999, Bytes::from(vec![0; 160]), now);
		assert!(queue.pop(format, now + REORDER_TIME).unwrap().2);
		queue.push(300, 1000000, Bytes::from(vec![0; 160]), now);
		assert!(
			queue.pop(format, now).is_some(),
			"a long outage must reanchor the sequence window"
		);
	}
	#[test]
	fn stop_while_preempted_releases_the_underlying_take() {
		let mut modes = ModesMachine::new();
		modes
			.resource(ModesResourceId::MainAudio, |x| {
				x.take(
					ModesEntity::Controller,
					ModesConstraint::UserInitiated,
					ModesConstraint::UserInitiated,
					ModesConstraint::Anytime,
				)
			})
			.unwrap();
		modes
			.resource(ModesResourceId::MainAudio, |x| {
				x.borrow(
					ModesEntity::Accessory,
					ModesConstraint::UserInitiated,
					ModesConstraint::Anytime,
				)
			})
			.unwrap();
		modes
			.resource(ModesResourceId::MainAudio, |x| {
				x.release(ModesEntity::Controller, ModesEntity::Accessory);
				Ok(())
			})
			.unwrap();
		assert_eq!(
			modes.inspect_resource(ModesResourceId::MainAudio, |x| x.owner()),
			ModesEntity::Accessory
		);
		modes
			.resource(ModesResourceId::MainAudio, |x| {
				x.unborrow(ModesEntity::Accessory)
			})
			.unwrap();
		assert_eq!(
			modes.inspect_resource(ModesResourceId::MainAudio, |x| x.owner()),
			ModesEntity::Accessory
		);
	}
	#[test]
	fn formats_are_independent_and_duplex_intersects_masks() {
		let mut request = request(1, AudioKind::Main, AudioUse::Media);
		assert_eq!(
			negotiate(&info(), &request).unwrap().0,
			PcmFormat::s16(48000, 2)
		);
		request.audio_type = AudioUse::SpeechRecognition;
		request.microphone = Some(PcmFormat::s16(96000, 2));
		assert_eq!(
			negotiate(&info(), &request).unwrap().0,
			PcmFormat::s16(24000, 1)
		);
		let mut info = info();
		info.audio_formats.as_mut().unwrap().push(AudioFormat {
			r#type: StreamType::MainAudio,
			audio_type: "speechRecognition".into(),
			audio_output_formats: Some(audio_format::PCM_48000_16_STEREO),
			audio_input_formats: Some(audio_format::PCM_16000_16_MONO),
		});
		assert!(
			negotiate(&info, &request).is_err(),
			"an existing incompatible profile must not use compatibility masks"
		);
		request.kind = AudioKind::Alternate;
		assert!(negotiate(&info, &request).is_err());
	}

	struct IdleOutput;
	struct FileOutput(std::collections::VecDeque<AudioChunk>);
	#[async_trait]
	impl CarplayProviderAudioOutput for FileOutput {
		async fn read(&mut self) -> Result<Option<AudioChunk>> {
			Ok(self.0.pop_front())
		}
	}
	#[async_trait]
	impl CarplayProviderAudioOutput for IdleOutput {
		async fn read(&mut self) -> Result<Option<AudioChunk>> {
			std::future::pending().await
		}
	}
	struct CollectInput(Arc<Mutex<Vec<AudioChunk>>>);
	#[async_trait]
	impl CarplayProviderAudioInput for CollectInput {
		async fn write(&mut self, chunk: AudioChunk) -> Result<()> {
			self.0.lock().await.push(chunk);
			Ok(())
		}
		fn progress(&self) -> Option<AudioProgress> {
			None
		}
	}
	fn endpoints(microphone: Option<Arc<Mutex<Vec<AudioChunk>>>>) -> AudioEndpoints {
		AudioEndpoints {
			output: Arc::new(Mutex::new(Box::new(IdleOutput))),
			input: microphone.map(|x| {
				Arc::new(Mutex::new(
					Box::new(CollectInput(x)) as Box<dyn CarplayProviderAudioInput>
				))
			}),
		}
	}

	async fn mock_session(
		bad_port: bool,
	) -> (
		CarplayNetSession,
		Arc<UdpSocket>,
		Arc<Mutex<Vec<Method>>>,
		AbortOnDropHandle<()>,
	) {
		let listener = TcpListener::bind("[::1]:0").await.unwrap();
		let SocketAddr::V6(addr) = listener.local_addr().unwrap() else {
			unreachable!()
		};
		let udp = Arc::new(UdpSocket::bind("[::1]:0").await.unwrap());
		let port = udp.local_addr().unwrap().port();
		let seen = Arc::new(Mutex::new(Vec::new()));
		let seen_task = seen.clone();
		let sender = udp.clone();
		let task = tokio::spawn(async move {
			let (socket, _) = listener.accept().await.unwrap();
			let mut framed = Framed::new(socket, rtsp::Codec::new());
			while let Some(Ok(rtsp::Message::Request(request))) = framed.next().await {
				seen_task.lock().await.push(request.method.clone());
				let mut response = rtsp::Response::ok();
				if request.method == Method::Setup {
					let setup: StreamSetupRequest = proto::from_binary(&request.body).unwrap();
					let stream = &setup.streams[0];
					if stream.input == Some(true) {
						assert!(
							stream.data_port.is_some(),
							"microphone socket must exist before SETUP"
						);
						let key = SessionKeys::data_stream(&[9; 32], stream.connection_id());
						let mut codec = AudioPacketCodec::new(
							SessionKeys {
								read: key.write,
								write: key.read,
							},
							AudioKind::Main,
						);
						let packet = codec.seal(&vec![0; 480], PcmFormat::s16(24000, 1)).unwrap();
						sender
							.send_to(&packet, (Ipv6Addr::LOCALHOST, stream.data_port.unwrap()))
							.await
							.unwrap();
					}
					let body = StreamSetupResponse {
						streams: Some(vec![
							super::super::proto::setup::StreamResponse {
								r#type: StreamType::Screen,
								data_port: Some(123),
								..Default::default()
							},
							super::super::proto::setup::StreamResponse {
								r#type: stream.r#type,
								data_port: Some(if bad_port { 0 } else { port }),
								stream_connection_id: Some(stream.stream_connection_id),
								..Default::default()
							},
						]),
						..Default::default()
					};
					response = response.body(proto::CONTENT_TYPE, proto::to_binary(&body).unwrap());
				}
				if let Some(cseq) = request.cseq() {
					response.headers.insert("CSeq", cseq.to_string());
				}
				if framed
					.send(rtsp::Message::Response(response))
					.await
					.is_err()
				{
					break;
				}
			}
		});
		let control = HeadUnitTcpConnection::new(addr).await.unwrap();
		let (_, proxy) = watch::channel(None);
		let session = CarplayNetSession {
			server: HeadUnitServer {
				addr,
				mac: [0; 6],
				pairing_identity: "mock".into(),
				model: "mock".into(),
			},
			shared_secret: [9; 32],
			rtsp_url: "rtsp://mock/1".into(),
			clock: Clock::new(),
			info: info(),
			video_params: crate::v4l2::VideoParams {
				width: 800,
				height: 480,
				fps: 30,
				bytes_per_line: 800,
				buffer_height: 480,
			},
			next_connection_id: 1,
			modes: ModesMachine::new(),
			hid: HashMap::new(),
			control,
			events: None,
			proxy,
			audio_requests: HashMap::new(),
			audio_status: Vec::new(),
		};
		(session, udp, seen, AbortOnDropHandle::new(task))
	}

	#[tokio::test]
	async fn session_streams_preempt_resume_and_restore_radio() {
		let (mut session, _udp, seen, _accessory) = mock_session(false).await;
		let mut streams = NetSessionStreamFutures::new();
		session
			.handle_audio_event(
				AudioEvent::Start(
					request(1, AudioKind::Main, AudioUse::Media),
					endpoints(None),
				),
				&mut streams,
			)
			.await
			.unwrap();
		session
			.handle_audio_event(
				AudioEvent::Start(
					request(2, AudioKind::Alternate, AudioUse::Default),
					endpoints(None),
				),
				&mut streams,
			)
			.await
			.unwrap();
		assert_eq!(streams.audio.len(), 2);
		let old_id = streams.audio[&AudioKind::Main].connection_id;
		session
			.modes
			.resource(ModesResourceId::MainAudio, |x| {
				x.borrow(
					ModesEntity::Accessory,
					ModesConstraint::UserInitiated,
					ModesConstraint::Anytime,
				)
			})
			.unwrap();
		session.reconcile_audio(&mut streams).await.unwrap();
		assert!(!streams.audio.contains_key(&AudioKind::Main));
		assert!(streams.audio.contains_key(&AudioKind::Alternate));
		assert!(
			session
				.audio_status
				.iter()
				.any(|x| x.id == 1 && x.state == AudioState::Suspended)
		);
		session
			.modes
			.resource(ModesResourceId::MainAudio, |x| {
				x.unborrow(ModesEntity::Accessory)
			})
			.unwrap();
		session.reconcile_audio(&mut streams).await.unwrap();
		assert!(
			streams.audio[&AudioKind::Main].connection_id > old_id,
			"resume requires fresh keys/nonces"
		);
		session
			.handle_audio_event(AudioEvent::Flush(1), &mut streams)
			.await
			.unwrap();
		session
			.handle_audio_event(
				AudioEvent::Duck {
					id: 2,
					duration_ms: 100,
					volume: 0.2,
				},
				&mut streams,
			)
			.await
			.unwrap();
		session.close_audio(&mut streams).await;
		assert!(streams.audio.is_empty());
		assert!(session.audio_requests.is_empty());
		assert_eq!(
			session
				.modes
				.inspect_resource(ModesResourceId::MainAudio, |x| x.owner()),
			ModesEntity::Accessory
		);
		assert_eq!(
			seen.lock()
				.await
				.iter()
				.filter(|x| **x == Method::Teardown)
				.count(),
			3
		);
	}

	#[tokio::test]
	async fn duplex_microphone_is_ready_at_setup_and_voice_borrow_returns() {
		let (mut session, _udp, _, _accessory) = mock_session(false).await;
		let mut streams = NetSessionStreamFutures::new();
		let captured = Arc::new(Mutex::new(Vec::new()));
		let mut request = request(3, AudioKind::Main, AudioUse::SpeechRecognition);
		request.microphone = Some(PcmFormat::s16(48000, 1));
		session
			.handle_audio_event(
				AudioEvent::Start(request, endpoints(Some(captured.clone()))),
				&mut streams,
			)
			.await
			.unwrap();
		assert!(matches!(
			session
				.modes
				.inspect_resource(ModesResourceId::MainAudio, |x| x.owner_state()),
			ModesOwnerState::Borrowed
		));
		tokio::time::timeout(Duration::from_secs(2), async {
			while captured.lock().await.is_empty() {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.unwrap();
		assert!(!captured.lock().await[0].data.is_empty());
		session
			.handle_audio_event(AudioEvent::Stop(3), &mut streams)
			.await
			.unwrap();
		assert_eq!(
			session
				.modes
				.inspect_resource(ModesResourceId::MainAudio, |x| x.owner()),
			ModesEntity::Accessory
		);
	}

	#[tokio::test]
	async fn invalid_setup_rolls_back_remote_stream_and_ownership() {
		let (mut session, _, seen, _accessory) = mock_session(true).await;
		let mut streams = NetSessionStreamFutures::new();
		session
			.handle_audio_event(
				AudioEvent::Start(
					request(4, AudioKind::Main, AudioUse::Media),
					endpoints(None),
				),
				&mut streams,
			)
			.await
			.unwrap();
		assert!(streams.audio.is_empty());
		assert!(session.audio_requests.is_empty());
		assert!(seen.lock().await.contains(&Method::Teardown));
		assert_eq!(
			session
				.modes
				.inspect_resource(ModesResourceId::MainAudio, |x| x.owner()),
			ModesEntity::Accessory
		);
		assert!(
			session
				.audio_status
				.iter()
				.any(|x| x.id == 4 && x.state == AudioState::Error)
		);
	}

	#[tokio::test]
	async fn file_eof_drains_conversion_and_tears_down_without_extra_samples() {
		let (mut session, udp, _, _accessory) = mock_session(false).await;
		let mut streams = NetSessionStreamFutures::new();
		let mut request = request(5, AudioKind::Main, AudioUse::Media);
		request.playback_clock = AudioClockMode::DemandDriven;
		let mut source = std::collections::VecDeque::new();
		for index in 0..10 {
			source.push_back(AudioChunk {
				position: index * 441,
				data: Bytes::from(vec![0; 441 * 4]),
			});
		}
		let endpoint = AudioEndpoints {
			output: Arc::new(Mutex::new(Box::new(FileOutput(source)))),
			input: None,
		};
		session
			.handle_audio_event(AudioEvent::Start(request, endpoint), &mut streams)
			.await
			.unwrap();
		let connection_id = streams.audio[&AudioKind::Main].connection_id;
		let keys = SessionKeys::data_stream(&[9; 32], connection_id as u64);
		let mut decoder = AudioPacketCodec::new(
			SessionKeys {
				read: keys.write,
				write: keys.read,
			},
			AudioKind::Main,
		);
		let mut buffer = [0; 2048];
		let mut frames = 0;
		let (kind, id, result) = tokio::time::timeout(Duration::from_secs(3), async {
			loop {
				tokio::select! {
					packet = udp.recv(&mut buffer) => {
						let len = packet.unwrap();
						frames += decoder.open(&buffer[..len], PcmFormat::s16(48000, 2)).unwrap().2.len() / 4;
					}
					stream = streams.next() => {
						let (super::super::NetSessionStream::Audio(kind, id), result) = stream else { panic!("unexpected stream"); };
						break (kind, id, result);
					}
				}
			}
		}).await.unwrap();
		while let Ok(len) = udp.try_recv(&mut buffer) {
			frames += decoder
				.open(&buffer[..len], PcmFormat::s16(48000, 2))
				.unwrap()
				.2
				.len()
				/ 4;
		}
		assert_eq!(
			frames, 4800,
			"100 ms of 44.1 kHz must produce exactly 100 ms of 48 kHz"
		);
		session
			.audio_finished(kind, id, result, &mut streams)
			.await
			.unwrap();
		assert!(session.audio_requests.is_empty());
		assert_eq!(
			session
				.modes
				.inspect_resource(ModesResourceId::MainAudio, |x| x.owner()),
			ModesEntity::Accessory
		);
	}

	#[tokio::test]
	async fn failed_start_cannot_activate_speech_or_interrupt_an_existing_call() {
		let (mut session, _, seen, _accessory) = mock_session(false).await;
		let mut streams = NetSessionStreamFutures::new();
		session
			.handle_audio_event(
				AudioEvent::AppState {
					speech: AudioSpeech::Listening,
					phone_call: false,
					turn_by_turn: false,
				},
				&mut streams,
			)
			.await
			.unwrap();
		assert!(session.modes.speech().is_none());
		session
			.modes
			.set_phone_call(ModesEntity::Accessory, true)
			.unwrap();
		session
			.handle_audio_event(
				AudioEvent::Start(
					request(6, AudioKind::Main, AudioUse::Telephony),
					endpoints(None),
				),
				&mut streams,
			)
			.await
			.unwrap();
		assert!(session.audio_requests.is_empty());
		assert!(seen.lock().await.is_empty());
		assert_eq!(session.modes.phone_call(), Some(ModesEntity::Accessory));
	}
}
