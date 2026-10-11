//! vibecoded
use std::{
	os::fd::AsFd,
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use log::warn;
use tokio::{
	io::{Interest, Ready, unix::AsyncFd},
	sync::watch,
};
use v4l2r::{
	Format, PlaneLayout, Rect, bindings,
	device::{
		AllocatedQueue, Device, DeviceConfig, Stream, TryDequeue,
		queue::{
			self, BuffersAllocated, GetCaptureBufferByIndex, GetFreeBufferError,
			GetFreeOutputBuffer, Queue, QueueInit,
		},
	},
	ioctl::{self, DqBufIoctlError, IoctlConvertError},
	memory::MmapHandle,
	nix::sys::time::{TimeVal, TimeValLike},
};

use crate::{
	provider::frame::{FrameCrop, Yuv420Frame, Yuv420Layout},
	v4l2::validate_mmap_buffers,
};

type InputQueue = Queue<queue::direction::Output, BuffersAllocated<Vec<MmapHandle>>>;
type CaptureQueue = Queue<queue::direction::Capture, BuffersAllocated<Vec<MmapHandle>>>;

#[derive(Clone, Copy, Debug)]
pub struct VideoDecoderConfig {
	/// Largest complete Annex-B access unit (or standalone SPS/PPS packet).
	pub max_access_unit_size: u32,
	pub input_buffers: u32,
	/// Requested pool size; the driver may allocate more to hold reference frames.
	pub capture_buffers: u32,
}

impl Default for VideoDecoderConfig {
	fn default() -> Self {
		Self {
			max_access_unit_size: 4 * 1024 * 1024,
			input_buffers: 6,
			capture_buffers: 6,
		}
	}
}

impl VideoDecoderConfig {
	fn validate(self) -> Result<()> {
		ensure!(
			self.max_access_unit_size != 0,
			"decoder input size must be nonzero"
		);
		ensure!(
			self.input_buffers != 0 && self.capture_buffers != 0,
			"decoder buffer counts must be nonzero"
		);
		Ok(())
	}
}

#[derive(Clone, Copy)]
struct DecoderClock(Instant);

impl DecoderClock {
	fn encode(self, timestamp: Option<Instant>) -> Result<TimeVal> {
		let Some(timestamp) = timestamp else {
			return Ok(TimeVal::microseconds(0));
		};
		let (elapsed, before_origin) = match timestamp.checked_duration_since(self.0) {
			Some(elapsed) => (elapsed, false),
			None => (self.0.duration_since(timestamp), true),
		};
		// V4L2 copies this value through reordering; it is opaque metadata, not a
		// presentation time for the decoder. Odd values encode times at/after the
		// origin, even values encode earlier times, and zero means no timestamp.
		let micros = i64::try_from(elapsed.as_micros())
			.context("decoder timestamp overflow")?
			.checked_mul(2)
			.and_then(|n| n.checked_add(if before_origin { 2 } else { 1 }))
			.context("decoder timestamp overflow")?;
		Ok(TimeVal::microseconds(micros))
	}

	fn decode(self, timestamp: bindings::timeval) -> Result<Option<Instant>> {
		ensure!(
			timestamp.tv_sec >= 0 && (0..1_000_000).contains(&timestamp.tv_usec),
			"invalid decoder timestamp"
		);
		let micros = (timestamp.tv_sec as u64)
			.checked_mul(1_000_000)
			.and_then(|n| n.checked_add(timestamp.tv_usec as u64))
			.context("decoder timestamp overflow")?;
		if micros == 0 {
			return Ok(None);
		}
		let timestamp = if micros % 2 == 1 {
			self.0.checked_add(Duration::from_micros((micros - 1) / 2))
		} else {
			self.0.checked_sub(Duration::from_micros((micros - 2) / 2))
		};
		Ok(Some(timestamp.context("decoder timestamp overflow")?))
	}
}

struct DecoderState {
	clock: DecoderClock,
	ended: AtomicBool,
}

async fn dequeue<Q: TryDequeue>(
	fd: &AsyncFd<Arc<Device>>,
	queue: &Q,
	interest: Interest,
) -> Result<Q::Dequeued> {
	loop {
		let mut guard = fd.ready(interest).await.context("failed to poll decoder")?;
		match queue.try_dequeue() {
			Err(IoctlConvertError::IoctlError(DqBufIoctlError::NotReady)) => guard.clear_ready(),
			res => return res.context("failed to dequeue decoder buffer"),
		}
	}
}

fn validate_input_size(len: usize, capacity: usize) -> Result<()> {
	ensure!(
		len != 0,
		"empty H.264 input is not a frame; use finish() to drain"
	);
	ensure!(
		len <= capacity,
		"H.264 access unit has {len} bytes, decoder capacity is {capacity}; do not split access units"
	);
	Ok(())
}

/// Exclusive owner of the compressed OUTPUT queue. Drive this concurrently with
/// next_frame(): processing headers can require CAPTURE setup before inputs complete.
pub struct VideoDecoderInput {
	device: Arc<AsyncFd<Arc<Device>>>,
	queue: InputQueue,
	state: Arc<DecoderState>,
	max_access_unit_size: usize,
	finishing: bool,
	capture_ready: watch::Receiver<bool>,
}

impl VideoDecoderInput {
	/// Submit one complete Annex-B H.264 access unit, or a standalone SPS/PPS packet.
	/// Socket reads must be reframed by the provider first. No input/output frame-count
	/// relationship is assumed. Supply a translated local timestamp, or None for headers.
	pub async fn queue_input(&mut self, buf: &[u8], timestamp: Option<Instant>) -> Result<()> {
		ensure!(
			!self.finishing && !self.state.ended.load(Ordering::Acquire),
			"decoder input has ended"
		);
		validate_input_size(buf.len(), self.max_access_unit_size)?;
		let timestamp = self.state.clock.encode(timestamp)?;
		let buffer = loop {
			match self.queue.try_get_free_buffer() {
				Ok(buffer) => break buffer,
				Err(GetFreeBufferError::NoFreeBuffer) => self.reclaim_input().await?,
			}
		};
		let mut mapping = buffer
			.get_plane_mapping(0)
			.context("failed to map decoder input")?;
		validate_input_size(buf.len(), mapping.len())?;
		mapping[..buf.len()].copy_from_slice(buf);
		drop(mapping);
		buffer
			.set_timestamp(timestamp)
			.queue(&[buf.len()])
			.context("failed to queue decoder input")?;
		Ok(())
	}

	async fn reclaim_input(&self) -> Result<()> {
		let output = dequeue(&self.device, &self.queue, Interest::WRITABLE).await?;
		if output.data.has_error() {
			warn!(
				"decoder rejected H.264 input buffer {}",
				output.data.index()
			);
		}
		// Dropping DqBuffer returns its index to the free pool, but does not QBUF it.
		drop(output);
		Ok(())
	}

	/// Drain at the end of a fresh stream, waiting for CAPTURE setup. Keep calling
	/// next_frame() concurrently until it returns None. Do not call this when merely
	/// returning a persistent decoder to the connection on UI hide.
	pub async fn finish(&mut self) -> Result<()> {
		ensure!(!self.finishing, "decoder input is already draining");
		// An in-band EOS may have already finished CAPTURE before socket EOF.
		// Reclaim OUTPUT in that case without issuing another STOP command.
		if !self.state.ended.load(Ordering::Acquire) {
			self.capture_ready
				.wait_for(|ready| *ready)
				.await
				.context("decoder output was dropped before capture setup")?;
			if !self.state.ended.load(Ordering::Acquire) {
				ioctl::decoder_cmd::<_, ioctl::DecoderCmd>(
					self.device.get_ref().as_ref(),
					ioctl::DecoderCmd::stop(),
				)
				.context("failed to drain decoder")?;
			}
		}
		self.finishing = true;
		while self.queue.num_queued_buffers() != 0 {
			self.reclaim_input().await?;
		}
		Ok(())
	}
}

enum CaptureState {
	AwaitingFormat(Queue<queue::direction::Capture, QueueInit>),
	Streaming {
		queue: CaptureQueue,
		layout: Yuv420Layout,
		visible: FrameCrop,
	},
}

/// A borrowed capture view and metadata. The callback must finish using the view
/// before its buffer is recycled. V4L2's sequence can have gaps after corrupt frames.
pub struct DecodedVideoFrame<'a> {
	pub image: Yuv420Frame<'a>,
	pub timestamp: Option<Instant>,
	pub sequence: u32,
}

/// Exclusive owner of CAPTURE and source-change events. Moving this owner between
/// screen providers preserves the queue, reference frames and pending format changes.
pub struct VideoDecoderOutput {
	device: Arc<AsyncFd<Arc<Device>>>,
	capture: Option<CaptureState>,
	state: Arc<DecoderState>,
	capture_buffers: u32,
	capture_ready: watch::Sender<bool>,
	source_change: bool,
	last: bool,
}

impl VideoDecoderOutput {
	fn read_events(&mut self) -> Result<()> {
		loop {
			match ioctl::dqevent::<ioctl::Event>(self.device.get_ref().as_ref()) {
				Ok(ioctl::Event::SrcChangeEvent(changes))
					if changes.contains(ioctl::SrcChanges::RESOLUTION) =>
				{
					self.source_change = true
				}
				Ok(_) => (),
				Err(ioctl::DqEventError::NotReady) => return Ok(()),
				Err(err) => return Err(err).context("failed to dequeue decoder event"),
			}
		}
	}

	fn configure_capture(&mut self) -> Result<()> {
		self.capture_ready.send_replace(false);
		let queue = match self
			.capture
			.take()
			.context("decoder capture queue unavailable")?
		{
			CaptureState::AwaitingFormat(queue) => queue,
			CaptureState::Streaming { queue, .. } => {
				queue
					.stream_off()
					.context("failed to stop decoder capture")?;
				queue
					.free_buffers()
					.context("failed to free decoder capture buffers")?
					.queue
			}
		};
		// OUTPUT stays streaming throughout a source change; stopping it discards
		// the bitstream state. Read the new dimensions only after draining old frames.
		let device = self.device.get_ref().as_ref();
		let mut format: bindings::v4l2_format =
			queue.get_format().context("failed to get decoded format")?;
		let original_visible = visible_rectangle(device)?;
		// Commit the parsed geometry as YU12, keeping color metadata and native Y
		// stride. S_FMT may reset COMPOSE, so preserve the source's visible crop.
		{
			let pix = unsafe { &mut format.fmt.pix_mp };
			let stride = pix.plane_fmt[0].bytesperline;
			pix.pixelformat = u32::from_le_bytes(*b"YU12");
			pix.num_planes = 1;
			pix.plane_fmt.fill(Default::default());
			pix.plane_fmt[0].bytesperline = stride;
			let mut fd = device.as_fd();
			ioctl::s_fmt::<_, bindings::v4l2_format>(&mut fd, format)
				.context("failed to select YU12 decoder capture")?;
			if visible_rectangle(device)? != original_visible {
				let restored: Rect = ioctl::s_selection(
					device,
					ioctl::SelectionType::Capture,
					ioctl::SelectionTarget::Compose,
					original_visible,
					ioctl::SelectionFlags::empty(),
				)
				.context("failed to preserve decoder visible rectangle")?;
				ensure!(
					restored == original_visible,
					"driver changed decoder visible rectangle"
				);
			}
		}
		let format: bindings::v4l2_format = queue
			.get_format()
			.context("failed to read capture layout")?;
		// This queue is VIDEO_CAPTURE_MPLANE, so pix_mp is the active union member.
		let pix = unsafe { format.fmt.pix_mp };
		ensure!(
			pix.pixelformat == u32::from_le_bytes(*b"YU12") && pix.num_planes == 1,
			"decoder must return contiguous single-plane YU12"
		);
		ensure!(
			pix.field == bindings::v4l2_field_V4L2_FIELD_NONE,
			"decoder output must be progressive"
		);
		let layout = Yuv420Layout {
			width: pix.width,
			height: pix.height,
			bytes_per_line: pix.plane_fmt[0].bytesperline as usize,
			buffer_height: pix.height as usize,
		};
		layout
			.validate(pix.plane_fmt[0].sizeimage as usize)
			.context("invalid decoded YU12 layout")?;
		let visible = visible_rectangle(device)?;
		ensure!(
			visible == original_visible,
			"driver changed decoder visible rectangle"
		);
		let visible = FrameCrop {
			left: u32::try_from(visible.left).context("negative decoder crop left")?,
			top: u32::try_from(visible.top).context("negative decoder crop top")?,
			width: visible.width,
			height: visible.height,
		};
		visible
			.validate(layout.width, layout.height)
			.context("invalid decoder visible rectangle")?;
		let minimum = match ioctl::g_ctrl(device, bindings::V4L2_CID_MIN_BUFFERS_FOR_CAPTURE) {
			Ok(count) => u32::try_from(count).context("negative decoder minimum buffer count")?,
			Err(ioctl::GCtrlError::Invalid) => 0, // MMAL raises REQBUFS count itself.
			Err(err) => return Err(err).context("failed to get decoder capture buffer minimum"),
		};
		let queue = queue
			.request_buffers::<Vec<MmapHandle>>(self.capture_buffers.max(minimum))
			.context("failed to allocate decoder capture buffers")?;
		ensure!(
			queue.num_buffers() >= minimum as usize,
			"decoder capture allocation is below its minimum"
		);
		validate_mmap_buffers(
			device,
			queue.get_type(),
			queue.num_buffers(),
			pix.plane_fmt[0].sizeimage as usize,
		)?;
		// Queue the entire actual pool, including additional driver-required buffers.
		for index in 0..queue.num_buffers() {
			queue
				.try_get_buffer(index)
				.context("failed to obtain decoder capture buffer")?
				.queue()
				.context("failed to queue decoder capture buffer")?;
		}
		queue
			.stream_on()
			.context("failed to start decoder capture")?;
		self.capture = Some(CaptureState::Streaming {
			queue,
			layout,
			visible,
		});
		self.capture_ready.send_replace(true);
		self.source_change = false;
		self.last = false;
		Ok(())
	}

	fn recycle_capture(&mut self, index: usize, last: bool) -> Result<()> {
		if last {
			self.last = true;
		} else if let Some(CaptureState::Streaming { queue, .. }) = &self.capture {
			queue
				.try_get_buffer(index)
				.context("failed to reclaim decoder capture buffer")?
				.queue()
				.context("failed to recycle decoder capture buffer")?;
		}
		Ok(())
	}

	/// Wait independently of input submissions. Calls the callback for one valid
	/// decoded frame, then recycles it even if the callback fails. Returns None only
	/// for end of stream; an empty LAST during a source change reconfigures CAPTURE.
	pub async fn next_frame<T>(
		&mut self,
		func: impl FnOnce(DecodedVideoFrame<'_>) -> Result<T>,
	) -> Result<Option<T>> {
		let device = self.device.clone();
		loop {
			if self.state.ended.load(Ordering::Acquire) {
				return Ok(None);
			}
			self.read_events()?;
			let initializing = matches!(self.capture, Some(CaptureState::AwaitingFormat(_)));
			if self.source_change && (initializing || self.last) {
				self.configure_capture()?;
			} else if self.last {
				self.state.ended.store(true, Ordering::Release);
				return Ok(None);
			}
			let interest = if matches!(self.capture, Some(CaptureState::Streaming { .. })) {
				Interest::READABLE | Interest::PRIORITY
			} else {
				Interest::PRIORITY
			};
			let mut guard = device
				.ready(interest)
				.await
				.context("failed to poll decoder capture")?;
			if guard.ready().is_priority() {
				// Drain events before clearing only PRI. DQBUF EAGAIN must not erase it.
				self.read_events()?;
				guard.clear_ready_matching(Ready::PRIORITY);
				continue;
			}
			let Some(CaptureState::Streaming {
				queue,
				layout,
				visible,
			}) = &self.capture
			else {
				continue;
			};
			let output = match queue.try_dequeue() {
				Err(IoctlConvertError::IoctlError(DqBufIoctlError::Eos)) => {
					// MMAL signals a format change by setting last_buffer_dequeued,
					// which can return EPIPE without an actual LAST buffer. Read pending
					// source events on the next iteration before deciding this is EOS.
					self.last = true;
					continue;
				}
				Err(IoctlConvertError::IoctlError(DqBufIoctlError::NotReady)) => {
					guard.clear_ready_matching(Ready::READABLE);
					continue;
				}
				result => result.context("failed to dequeue decoded frame")?,
			};
			drop(guard);
			let index = output.data.index() as usize;
			let last = output.data.is_last();
			let error = output.data.has_error();
			let plane = output
				.data
				.planes_iter()
				.next()
				.context("missing decoder capture plane")?;
			let bytesused = *plane.bytesused;
			let offset = *plane.data_offset.unwrap_or(&0);
			if bytesused > offset && !error {
				let result = (|| {
					ensure!(
						bytesused <= *plane.length,
						"invalid decoder capture payload bounds"
					);
					let mapping = output
						.get_plane_mapping(0)
						.context("failed to map decoded frame")?;
					let image = Yuv420Frame::from_mapping(&mapping, *layout)?.crop(*visible)?;
					func(DecodedVideoFrame {
						image,
						timestamp: self.state.clock.decode(output.data.timestamp())?,
						sequence: output.data.sequence(),
					})
				})();
				drop(output); // mapping/view are gone before QBUF.
				self.recycle_capture(index, last)?;
				return result.map(Some);
			}
			let invalid_offset = offset > bytesused;
			if error {
				warn!(
					"discarding corrupt decoded frame {}",
					output.data.sequence()
				);
			}
			drop(output);
			self.recycle_capture(index, last)?;
			ensure!(!invalid_offset, "invalid decoder capture payload offset");
		}
	}
}

fn visible_rectangle(device: &Device) -> Result<Rect> {
	match ioctl::g_selection(
		device,
		ioctl::SelectionType::Capture,
		ioctl::SelectionTarget::Compose,
	) {
		Err(ioctl::GSelectionError::Invalid) => ioctl::g_selection(
			device,
			ioctl::SelectionType::Capture,
			ioctl::SelectionTarget::CropDefault,
		),
		result => result,
	}
	.context("failed to get decoder visible rectangle")
}

/// Create one shared device registration with exclusive OUTPUT/CAPTURE owners.
/// The source SPS determines CAPTURE geometry; encoder VideoParams belong to the
/// copy destination, not the decoder. Retain both owners for a persistent stream,
/// or drop both and recreate them for each fresh screen socket.
pub fn create_video_decoder(
	device: &Path,
	config: VideoDecoderConfig,
) -> Result<(VideoDecoderInput, VideoDecoderOutput)> {
	config.validate()?;
	let device = Arc::new(
		Device::open(device, DeviceConfig::new().non_blocking_dqbuf())
			.context("failed to open decoder")?,
	);
	let mut output_queue =
		Queue::get_output_mplane_queue(device.clone()).context("failed to get decoder OUTPUT")?;
	let capture_queue =
		Queue::get_capture_mplane_queue(device.clone()).context("failed to get decoder CAPTURE")?;
	ioctl::subscribe_event(
		device.as_ref(),
		ioctl::EventType::SourceChange(0),
		ioctl::SubscribeEventFlags::empty(),
	)
	.context("decoder must support source-change events")?;
	let format = output_queue
		.set_format(Format {
			pixelformat: b"H264".into(),
			plane_fmt: vec![PlaneLayout {
				bytesperline: 0,
				sizeimage: config.max_access_unit_size,
			}],
			..Default::default()
		})
		.context("failed to configure H.264 decoder input")?;
	ensure!(
		format.pixelformat == b"H264".into() && format.plane_fmt.len() == 1,
		"decoder must accept single-plane H.264"
	);
	ensure!(
		format.plane_fmt[0].sizeimage >= config.max_access_unit_size,
		"driver reduced decoder input capacity below configured access-unit limit"
	);
	let output_queue = output_queue
		.request_buffers::<Vec<MmapHandle>>(config.input_buffers)
		.context("failed to allocate decoder input buffers")?;
	validate_mmap_buffers(
		device.as_ref(),
		output_queue.get_type(),
		output_queue.num_buffers(),
		format.plane_fmt[0].sizeimage as usize,
	)?;
	let device = Arc::new(
		AsyncFd::with_interest(
			device,
			Interest::READABLE | Interest::WRITABLE | Interest::PRIORITY,
		)
		.context("failed to register decoder readiness")?,
	);
	let state = Arc::new(DecoderState {
		clock: DecoderClock(Instant::now()),
		ended: AtomicBool::new(false),
	});
	let (capture_ready_tx, capture_ready_rx) = watch::channel(false);
	output_queue
		.stream_on()
		.context("failed to start decoder input")?;
	Ok((
		VideoDecoderInput {
			device: device.clone(),
			queue: output_queue,
			state: state.clone(),
			max_access_unit_size: config.max_access_unit_size as usize,
			finishing: false,
			capture_ready: capture_ready_rx,
		},
		VideoDecoderOutput {
			device,
			capture: Some(CaptureState::AwaitingFormat(capture_queue)),
			state,
			capture_buffers: config.capture_buffers,
			capture_ready: capture_ready_tx,
			source_change: false,
			last: false,
		},
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn input_requires_nonempty_whole_packets_within_capacity() {
		assert!(validate_input_size(0, 8).is_err());
		assert!(validate_input_size(8, 8).is_ok());
		assert!(validate_input_size(9, 8).is_err());
	}

	#[test]
	fn queue_owners_and_driver_futures_can_move_between_tasks() {
		fn is_send<T: Send>() {}
		fn send_future(_: impl Future + Send) {}
		is_send::<VideoDecoderInput>();
		is_send::<VideoDecoderOutput>();
		// Type-check the futures without requiring a hardware device in the test.
		let _ = |input: &mut VideoDecoderInput,
		         output: &mut VideoDecoderOutput,
		         buffer: &mut ioctl::PlaneMapping,
		         params: crate::v4l2::VideoParams| {
			send_future(input.queue_input(&[0, 0, 0, 1, 0x67], None));
			send_future(input.finish());
			send_future(output.next_frame(|frame| {
				frame.image.copy_to(buffer, params)?;
				Ok(frame.timestamp)
			}));
		};
	}

	#[test]
	fn timestamp_round_trip_preserves_reordered_frames_and_none() {
		let origin = Instant::now();
		let clock = DecoderClock(origin);
		for elapsed in [2_000_001i64, -1, 0, -2_000_001, 1_000_000, -41_234, 41_234] {
			let duration = Duration::from_micros(elapsed.unsigned_abs());
			let instant = if elapsed >= 0 {
				origin + duration
			} else {
				origin - duration
			};
			let timestamp = clock.encode(Some(instant)).unwrap();
			assert!(timestamp.num_microseconds() > 0);
			assert_eq!(
				clock
					.decode(bindings::timeval {
						tv_sec: timestamp.tv_sec(),
						tv_usec: timestamp.tv_usec()
					})
					.unwrap(),
				Some(instant)
			);
		}
		assert_eq!(clock.encode(None).unwrap().num_microseconds(), 0);
		assert_eq!(
			clock
				.decode(bindings::timeval {
					tv_sec: 0,
					tv_usec: 0
				})
				.unwrap(),
			None
		);
		assert!(
			clock
				.decode(bindings::timeval {
					tv_sec: 0,
					tv_usec: 1_000_000
				})
				.is_err()
		);
	}
}
