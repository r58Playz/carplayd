//! Stateful PCM conversion, independent of RTP and provider socket framing.
use std::collections::VecDeque;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use rubato::{
	Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
	WindowFunction, audioadapter_buffers::direct::InterleavedSlice,
};
use tokio::sync::{mpsc, oneshot};

use super::audio::{PcmFormat, SampleType};

/// Playback cushion against provider delivery jitter (network stalls, coarse provider
/// packets), and the only playback slack that adds latency. Playback (re)starts only
/// once this much is queued.
pub const TARGET_BUFFER_SECONDS: f64 = 0.050;
/// Queued input past this is stale and is trimmed back to the target. A ceiling for
/// bursts after a stall, not latency in steady state.
pub const MAX_BUFFER_SECONDS: f64 = 0.400;
/// Microphone frames in flight to the provider's consumer.
pub const MICROPHONE_TARGET_SECONDS: f64 = 0.020;
const RATIO_RANGE: f64 = 1.01;

pub struct AudioConverter {
	input: PcmFormat,
	output: PcmFormat,
	resampler: Option<Async<f32>>,
	queue: VecDeque<f32>,
	input_buffer: Vec<f32>,
	output_buffer: Vec<f32>,
	encoded: Vec<u8>,
	max_frames: usize,
	target_frames: usize,
	input_frames: u64,
	output_frames: u64,
	skip_delay: usize,
	ratio: f64,
	ended: bool,
}

impl AudioConverter {
	pub fn new(input: PcmFormat, output: PcmFormat, adaptive: bool) -> Result<Self> {
		input.validate()?;
		output.validate()?;
		let ratio = output.sample_rate as f64 / input.sample_rate as f64;
		// Build the filter at the lowest allowed ratio so drift correction cannot move
		// its stopband above the output Nyquist frequency.
		let resampler = if adaptive || input.sample_rate != output.sample_rate {
			let parameters = SincInterpolationParameters {
				sinc_len: 128,
				f_cutoff: None,
				oversampling_factor: 128,
				interpolation: SincInterpolationType::Cubic,
				window: WindowFunction::BlackmanHarris2,
			};
			let mut resampler = Async::<f32>::new_sinc(
				ratio / RATIO_RANGE,
				RATIO_RANGE * RATIO_RANGE,
				&parameters,
				output.block_frames(),
				output.channels as usize,
				FixedAsync::Output,
			)
			.context("failed to create PCM resampler")?;
			resampler.set_resample_ratio(ratio, false)?;
			Some(resampler)
		} else {
			None
		};
		let skip_delay = resampler.as_ref().map_or(0, |x| x.output_delay());
		let max_frames = (input.sample_rate as f64 * MAX_BUFFER_SECONDS).ceil() as usize;
		let target_frames = (input.sample_rate as f64 * TARGET_BUFFER_SECONDS).ceil() as usize;
		let input_capacity = resampler
			.as_ref()
			.map_or(output.block_frames(), |x| x.input_frames_max());
		let channels = output.channels as usize;
		Ok(Self {
			input,
			output,
			resampler,
			queue: VecDeque::with_capacity((max_frames + input_capacity) * channels),
			input_buffer: vec![0.0; input_capacity * channels],
			output_buffer: vec![0.0; output.block_frames() * channels],
			encoded: Vec::with_capacity(output.block_frames() * output.frame_bytes()),
			max_frames,
			target_frames,
			input_frames: 0,
			output_frames: 0,
			skip_delay,
			ratio,
			ended: false,
		})
	}

	pub fn buffered_seconds(&self) -> f64 {
		self.queue.len() as f64 / self.output.channels as f64 / self.input.sample_rate as f64
	}

	pub fn reset(&mut self) {
		self.queue.clear();
		if let Some(resampler) = &mut self.resampler {
			resampler.reset();
			// reset restores the construction ratio, not the current stream ratio.
			resampler
				.set_resample_ratio(self.ratio, false)
				.expect("previously valid ratio");
		}
		self.skip_delay = self.resampler.as_ref().map_or(0, |x| x.output_delay());
		self.input_frames = 0;
		self.output_frames = 0;
		self.ended = false;
	}

	/// Returns true when stale audio was dropped. Validate the complete block first.
	pub fn push(&mut self, data: &[u8]) -> Result<bool> {
		ensure!(!self.ended, "PCM after EOF");
		let frames = self.input.frames(data)?;
		ensure!(
			frames <= (self.input.sample_rate as usize).div_ceil(50),
			"PCM block exceeds 20 ms"
		);
		if self.input.sample_type == SampleType::F32Le {
			ensure!(
				data.chunks_exact(4)
					.all(|x| f32::from_le_bytes(x.try_into().unwrap()).is_finite()),
				"nonfinite PCM sample"
			);
		}
		let queued = self.queue.len() / self.output.channels as usize;
		let overflow = queued + frames > self.max_frames;
		if overflow {
			// Keep the target cushion: restarting empty would underrun at the next stall.
			let stale = (queued + frames - self.target_frames).min(queued);
			self.queue.drain(..stale * self.output.channels as usize);
			self.input_frames -= stale as u64;
		}
		let sample_bytes = self.input.frame_bytes() / self.input.channels as usize;
		for frame in data.chunks_exact(self.input.frame_bytes()) {
			let decode = |channel: usize| {
				let sample = &frame[channel * sample_bytes..(channel + 1) * sample_bytes];
				match self.input.sample_type {
					SampleType::S16Le => {
						i16::from_le_bytes(sample.try_into().unwrap()) as f32 / 32768.0
					}
					SampleType::F32Le => f32::from_le_bytes(sample.try_into().unwrap()),
				}
			};
			let left = decode(0);
			let right = if self.input.channels == 2 {
				decode(1)
			} else {
				left
			};
			if self.output.channels == 1 {
				self.queue.push_back(left * 0.5 + right * 0.5);
			} else {
				self.queue.push_back(left);
				self.queue.push_back(right);
			}
		}
		self.input_frames += frames as u64;
		Ok(overflow)
	}

	pub fn finish(&mut self) {
		self.ended = true;
	}

	pub fn set_ratio(&mut self, ratio: f64) -> Result<()> {
		let nominal = self.output.sample_rate as f64 / self.input.sample_rate as f64;
		ensure!(
			ratio.is_finite() && (nominal / RATIO_RANGE..=nominal * RATIO_RANGE).contains(&ratio),
			"PCM rate correction out of range"
		);
		if let Some(resampler) = &mut self.resampler {
			resampler.set_resample_ratio(ratio, true)?;
		}
		self.ratio = ratio;
		Ok(())
	}

	/// The returned slice borrows reusable storage. None means more input is needed,
	/// or (after finish) that the filter tail and final partial block have been drained.
	pub fn next_block(&mut self) -> Result<Option<&[u8]>> {
		let channels = self.output.channels as usize;
		loop {
			let wanted = (self.input_frames as f64 * self.ratio).round() as u64;
			if self.ended && self.output_frames >= wanted {
				return Ok(None);
			}
			let needed = self
				.resampler
				.as_ref()
				.map_or(self.output.block_frames(), |x| x.input_frames_next());
			if self.queue.len() < needed * channels && !self.ended {
				return Ok(None);
			}
			let available = (self.queue.len() / channels).min(needed);
			self.input_buffer[..needed * channels].fill(0.0);
			for sample in &mut self.input_buffer[..available * channels] {
				*sample = self.queue.pop_front().unwrap();
			}
			let frames = if let Some(resampler) = &mut self.resampler {
				let input = InterleavedSlice::new(&self.input_buffer, channels, needed)?;
				let mut output = InterleavedSlice::new_mut(
					&mut self.output_buffer,
					channels,
					self.output.block_frames(),
				)?;
				resampler.process_into_buffer(&input, &mut output, None)?.1
			} else {
				self.output_buffer[..available * channels]
					.copy_from_slice(&self.input_buffer[..available * channels]);
				available
			};
			let skip = self.skip_delay.min(frames);
			self.skip_delay -= skip;
			let count = if self.ended {
				(frames - skip).min(wanted.saturating_sub(self.output_frames) as usize)
			} else {
				frames - skip
			};
			if count == 0 {
				continue;
			}
			self.encoded.clear();
			for &sample in &self.output_buffer[skip * channels..(skip + count) * channels] {
				match self.output.sample_type {
					SampleType::S16Le => self.encoded.extend_from_slice(
						&((sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
					),
					SampleType::F32Le => self.encoded.extend_from_slice(&sample.to_le_bytes()),
				}
			}
			self.output_frames += count as u64;
			return Ok(Some(&self.encoded));
		}
	}
}

/// Occupancy servo in seconds/ppm. Positive error reduces output/input ratio to
/// consume queued playback sooner (or generate less microphone output).
pub struct DriftController {
	target: f64,
	filtered: f64,
	integral: f64,
	correction: f64,
	initialized: bool,
}
impl DriftController {
	pub fn new(target: f64) -> Self {
		Self {
			target,
			filtered: 0.0,
			integral: 0.0,
			correction: 0.0,
			initialized: false,
		}
	}
	pub fn update(&mut self, queued_seconds: f64, seconds: f64) -> f64 {
		let dt = seconds.clamp(0.0, 0.5);
		if !self.initialized {
			self.filtered = queued_seconds;
			self.initialized = true;
		}
		self.filtered += (queued_seconds - self.filtered) * dt / (1.0 + dt);
		let error_ms = (self.filtered - self.target) * 1000.0;
		let candidate = (self.integral + error_ms * 10.0 * dt).clamp(-2000.0, 2000.0);
		let desired = (200.0 * error_ms + candidate).clamp(-2000.0, 2000.0);
		// Anti-windup: integrate only when unsaturated or moving back toward zero.
		if desired.abs() < 2000.0 || error_ms.signum() != desired.signum() {
			self.integral = candidate;
		}
		self.correction += (desired - self.correction).clamp(-100.0 * dt, 100.0 * dt);
		1.0 / (1.0 + self.correction * 1e-6)
	}
}

enum ConverterCommand {
	Push(Bytes, oneshot::Sender<Result<(f64, bool)>>),
	Read(f64, bool, oneshot::Sender<Result<(Option<Bytes>, f64)>>),
	Reset(oneshot::Sender<()>),
}

/// One long-lived DSP worker. Drop closes its channel; the finite processing call
/// completes and blocking_recv exits. Normal shutdown joins it explicitly.
pub struct ConverterWorker {
	tx: Option<mpsc::Sender<ConverterCommand>>,
	task: Option<tokio::task::JoinHandle<Result<()>>>,
}
impl ConverterWorker {
	pub fn new(input: PcmFormat, output: PcmFormat, adaptive: bool) -> Self {
		let (tx, mut rx) = mpsc::channel(2);
		let task = tokio::task::spawn_blocking(move || {
			let mut converter = AudioConverter::new(input, output, adaptive)?;
			while let Some(command) = rx.blocking_recv() {
				match command {
					ConverterCommand::Push(data, tx) => {
						let result = converter
							.push(&data)
							.map(|overflow| (converter.buffered_seconds(), overflow));
						let _ = tx.send(result);
					}
					ConverterCommand::Read(ratio, finish, tx) => {
						let result = (|| {
							converter.set_ratio(ratio)?;
							if finish {
								converter.finish();
							}
							let data = converter.next_block()?.map(Bytes::copy_from_slice);
							Ok((data, converter.buffered_seconds()))
						})();
						let _ = tx.send(result);
					}
					ConverterCommand::Reset(tx) => {
						converter.reset();
						let _ = tx.send(());
					}
				}
			}
			Ok(())
		});
		Self {
			tx: Some(tx),
			task: Some(task),
		}
	}
	pub async fn push(&self, data: Bytes) -> Result<(f64, bool)> {
		let (tx, rx) = oneshot::channel();
		self.tx
			.as_ref()
			.unwrap()
			.send(ConverterCommand::Push(data, tx))
			.await
			.context("PCM worker ended")?;
		rx.await.context("PCM worker failed")?
	}
	pub async fn read(&self, ratio: f64, finish: bool) -> Result<(Option<Bytes>, f64)> {
		let (tx, rx) = oneshot::channel();
		self.tx
			.as_ref()
			.unwrap()
			.send(ConverterCommand::Read(ratio, finish, tx))
			.await
			.context("PCM worker ended")?;
		rx.await.context("PCM worker failed")?
	}
	pub async fn reset(&self) -> Result<()> {
		let (tx, rx) = oneshot::channel();
		self.tx
			.as_ref()
			.unwrap()
			.send(ConverterCommand::Reset(tx))
			.await
			.context("PCM worker ended")?;
		rx.await.context("PCM worker failed")
	}
	pub async fn shutdown(mut self) -> Result<()> {
		self.tx.take();
		self.task
			.take()
			.unwrap()
			.await
			.context("failed to join PCM worker")?
	}
}

/// Runs without USB/audio devices, so the cross-built daemon can measure DSP cost
/// on the Pi while another session exercises video. Three simultaneous converters.
pub fn benchmark() -> Result<()> {
	use std::time::Instant;
	let cases = [
		(PcmFormat::s16(96000, 2), PcmFormat::s16(48000, 2)),
		(PcmFormat::s16(24000, 1), PcmFormat::s16(96000, 1)),
		(PcmFormat::s16(96000, 2), PcmFormat::s16(48000, 2)),
	];
	let mut converters: Vec<_> = cases
		.into_iter()
		.map(|(input, output)| {
			Ok((
				AudioConverter::new(input, output, true)?,
				vec![0; input.block_frames() * input.frame_bytes()],
			))
		})
		.collect::<Result<_>>()?;
	let mut timings = Vec::with_capacity(2000);
	for index in 0..2020 {
		let start = Instant::now();
		for (converter, input) in &mut converters {
			converter.push(input)?;
			while converter.next_block()?.is_some() {}
		}
		if index >= 20 {
			timings.push(start.elapsed().as_secs_f64() * 1000.0);
		}
	}
	let mean = timings.iter().sum::<f64>() / timings.len() as f64;
	let maximum = timings.iter().copied().fold(0.0, f64::max);
	let mut sorted = timings;
	sorted.sort_by(f64::total_cmp);
	println!("Audio DSP: three simultaneous converters, 5 ms source blocks");
	println!(
		"mean={mean:.3} ms p99={:.3} ms max={maximum:.3} ms; mean CPU={:.1}% of one core",
		sorted[sorted.len() * 99 / 100],
		mean / 5.0 * 100.0
	);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	fn convert(input: PcmFormat, output: PcmFormat, data: &[u8]) -> Vec<u8> {
		let mut converter = AudioConverter::new(input, output, false).unwrap();
		let mut result = Vec::new();
		for block in data.chunks(input.block_frames() * input.frame_bytes()) {
			converter.push(block).unwrap();
			while let Some(block) = converter.next_block().unwrap() {
				result.extend_from_slice(block);
			}
		}
		converter.finish();
		while let Some(block) = converter.next_block().unwrap() {
			result.extend_from_slice(block);
		}
		result
	}
	#[test]
	fn matching_s16_is_bit_exact_and_partial_eof_is_preserved() {
		let data: Vec<_> = (-32768i32..32768)
			.flat_map(|x| (x as i16).to_le_bytes())
			.collect();
		let format = PcmFormat::s16(44100, 1);
		assert_eq!(convert(format, format, &data), data);
	}
	#[test]
	fn remix_and_saturation() {
		let float = PcmFormat {
			sample_rate: 48000,
			channels: 1,
			sample_type: SampleType::F32Le,
		};
		let data: Vec<_> = [-2.0f32, 0.5, 2.0]
			.into_iter()
			.flat_map(f32::to_le_bytes)
			.collect();
		let result = convert(float, PcmFormat::s16(48000, 2), &data);
		let samples: Vec<_> = result
			.chunks_exact(2)
			.map(|x| i16::from_le_bytes(x.try_into().unwrap()))
			.collect();
		assert_eq!(samples, [-32768, -32768, 16384, 16384, 32767, 32767]);
		let result = convert(PcmFormat::s16(48000, 2), PcmFormat::s16(48000, 1), &result);
		assert_eq!(result.len(), 6);
	}
	#[test]
	fn resampling_drains_exact_duration_without_startup_delay() {
		for (from, to) in [(44100, 48000), (48000, 24000), (8000, 96000), (96000, 8000)] {
			let frames = from / 10 + 7;
			let data: Vec<_> = (0..frames).flat_map(|_| 10000i16.to_le_bytes()).collect();
			let result = convert(PcmFormat::s16(from, 1), PcmFormat::s16(to, 1), &data);
			assert_eq!(
				result.len() / 2,
				(frames as f64 * to as f64 / from as f64).round() as usize
			);
			let middle = result.len() / 4 * 2;
			assert!(
				(i16::from_le_bytes(result[middle..middle + 2].try_into().unwrap()) as i32 - 10000)
					.abs()
					< 20
			);
		}
	}
	#[test]
	fn downsampling_rejects_above_nyquist_energy() {
		let input = PcmFormat {
			sample_rate: 48000,
			channels: 1,
			sample_type: SampleType::F32Le,
		};
		let output = PcmFormat {
			sample_rate: 16000,
			..input
		};
		let tone = |hz: f32| {
			(0..48000)
				.flat_map(|i| {
					(std::f32::consts::TAU * hz * i as f32 / 48000.0)
						.sin()
						.to_le_bytes()
				})
				.collect::<Vec<_>>()
		};
		let rms = |bytes: Vec<u8>| {
			let samples: Vec<_> = bytes
				.chunks_exact(4)
				.map(|x| f32::from_le_bytes(x.try_into().unwrap()))
				.collect();
			(samples[200..samples.len() - 200]
				.iter()
				.map(|x| x * x)
				.sum::<f32>()
				/ (samples.len() - 400) as f32)
				.sqrt()
		};
		assert!(rms(convert(input, output, &tone(1000.0))) > 0.65);
		assert!(rms(convert(input, output, &tone(12000.0))) < 0.002);
	}
	#[test]
	fn validates_before_mutating_and_reset_discards_filter_history() {
		let format = PcmFormat {
			sample_rate: 48000,
			channels: 1,
			sample_type: SampleType::F32Le,
		};
		let mut converter = AudioConverter::new(format, format, true).unwrap();
		assert!(converter.push(&f32::NAN.to_le_bytes()).is_err());
		assert!(converter.push(&[0]).is_err());
		assert_eq!(converter.buffered_seconds(), 0.0);
		converter.push(&vec![0; 480 * 4]).unwrap();
		converter.reset();
		converter.finish();
		assert!(converter.next_block().unwrap().is_none());
		assert!(converter.set_ratio(f64::NAN).is_err());
	}
	#[test]
	fn overflow_trims_stale_input_to_the_target_cushion() {
		let format = PcmFormat::s16(48000, 1);
		let mut converter = AudioConverter::new(format, format, false).unwrap();
		let block = vec![0; 960 * 2];
		for _ in 0..20 {
			assert!(!converter.push(&block).unwrap());
		}
		assert!(converter.push(&block).unwrap());
		assert!((converter.buffered_seconds() - TARGET_BUFFER_SECONDS).abs() < 1e-9);
		// Trimmed frames are not owed at EOF.
		converter.finish();
		let mut frames = 0;
		while let Some(block) = converter.next_block().unwrap() {
			frames += block.len() / 2;
		}
		assert_eq!(frames, (48000.0 * TARGET_BUFFER_SECONDS) as usize);
	}
	#[test]
	fn servo_tracks_clock_drift_without_unbounded_queues() {
		for ppm in [-1000.0f64, -500.0, -100.0, 100.0, 500.0, 1000.0] {
			let mut servo = DriftController::new(TARGET_BUFFER_SECONDS);
			let mut queue = TARGET_BUFFER_SECONDS;
			for step in 0..6000 {
				let jitter = if step % 2 == 0 { 0.001 } else { -0.001 };
				let ratio = servo.update(queue + jitter, 0.1);
				queue += ((1.0 + ppm * 1e-6) - 1.0 / ratio) * 0.1;
				assert!((0.0..MAX_BUFFER_SECONDS).contains(&queue), "{ppm}: {queue}");
			}
			assert!(
				(queue - TARGET_BUFFER_SECONDS).abs() < 0.001,
				"{ppm}: {queue}"
			);
		}
	}
}
