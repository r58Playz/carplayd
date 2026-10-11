//! Shared V4L2 memory-to-memory device and buffer helpers.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use tokio::io::{Interest, unix::AsyncFd};
use v4l2r::{
	QueueType,
	device::{
		Device, DeviceConfig, TryDequeue,
		queue::{self, BuffersAllocated, Queue},
	},
	ioctl::{DqBufIoctlError, IoctlConvertError},
	memory::MmapHandle,
};

pub(crate) type OutputQueue = Queue<queue::direction::Output, BuffersAllocated<Vec<MmapHandle>>>;
pub(crate) type CaptureQueue = Queue<queue::direction::Capture, BuffersAllocated<Vec<MmapHandle>>>;

pub(crate) fn open_device(path: &Path) -> Result<Arc<Device>> {
	Ok(Arc::new(
		Device::open(path, DeviceConfig::new().non_blocking_dqbuf())
			.context("failed to open V4L2 device")?,
	))
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct VideoParams {
	/// video width
	pub width: u32,
	/// video height
	pub height: u32,
	/// intended fps
	pub fps: u32,

	/// Y row stride in bytes, including padding
	pub bytes_per_line: usize,
	/// storage rows, including padding
	pub buffer_height: usize,
}

impl VideoParams {
	pub(crate) fn ensure_matches(&self, expected: &Self) -> Result<()> {
		if self != expected {
			bail!(
				"encoder video parameters changed since probe: expected {expected:?}, driver returned {self:?}"
			);
		}
		Ok(())
	}

	/// Populated size of the single YU12 memory plane, including Y, U and V.
	pub(crate) fn frame_size(&self) -> Result<usize> {
		let y_size = self
			.bytes_per_line
			.checked_mul(self.buffer_height)
			.context("YU12 Y plane size overflow")?;
		y_size
			.checked_add(y_size / 2)
			.context("YU12 buffer size overflow")
	}

	/// Validate a contiguous YU12 layout against a buffer's available capacity.
	pub fn validate(&self, buffer_len: usize) -> Result<()> {
		if self.width == 0 || self.height == 0 || self.fps == 0 {
			bail!("video dimensions and fps must be nonzero");
		}
		if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
			bail!("YU12 visible dimensions must be even");
		}
		if self.bytes_per_line == 0 || !self.bytes_per_line.is_multiple_of(2) {
			bail!("YU12 Y stride must be nonzero and even");
		}
		if self.buffer_height == 0 || !self.buffer_height.is_multiple_of(2) {
			bail!("YU12 storage height must be nonzero and even");
		}
		let width = usize::try_from(self.width).context("video width does not fit usize")?;
		let height = usize::try_from(self.height).context("video height does not fit usize")?;
		if width > self.bytes_per_line {
			bail!(
				"visible width {width} exceeds Y stride {}",
				self.bytes_per_line
			);
		}
		if height > self.buffer_height {
			bail!(
				"visible height {height} exceeds storage height {}",
				self.buffer_height
			);
		}

		let required = self.frame_size()?;
		if required > buffer_len {
			bail!("YU12 layout needs {required} bytes, buffer has {buffer_len}");
		}

		Ok(())
	}
}

pub(crate) fn validate_mmap_buffers(
	device: &Device,
	queue: QueueType,
	count: usize,
	required_size: usize,
) -> Result<()> {
	if count == 0 {
		bail!("{queue:?} has no allocated buffers");
	}
	for index in 0..count {
		let buffer: v4l2r::ioctl::QueryBuffer = v4l2r::ioctl::querybuf(device, queue, index)
			.with_context(|| format!("failed to query {queue:?} buffer {index}"))?;
		let [plane] = buffer.planes.as_slice() else {
			bail!("{queue:?} buffer {index} must have one memory plane");
		};
		if (plane.length as usize) < required_size {
			bail!(
				"{queue:?} buffer {index} needs {required_size} bytes, allocation has {}",
				plane.length
			);
		}
	}
	Ok(())
}

pub(crate) async fn dequeue<Q: TryDequeue>(
	fd: &AsyncFd<Arc<Device>>,
	queue: &Q,
	interest: Interest,
) -> Result<Q::Dequeued> {
	loop {
		let mut guard = fd.ready(interest).await.context("failed to poll device")?;
		match queue.try_dequeue() {
			Err(IoctlConvertError::IoctlError(DqBufIoctlError::NotReady)) => guard.clear_ready(),
			res => return res.context("failed to dequeue buffer"),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn yu12_frame_size_includes_chroma_and_storage_padding() {
		for (width, height, bytes_per_line, buffer_height, expected) in [
			(800, 480, 800, 480, 576_000),
			(800, 480, 832, 480, 599_040),
			(800, 480, 832, 496, 619_008),
			(1920, 720, 1920, 720, 2_073_600),
		] {
			let params = VideoParams {
				width,
				height,
				fps: 30,
				bytes_per_line,
				buffer_height,
			};
			assert_eq!(params.frame_size().unwrap(), expected);
			assert!(params.validate(expected).is_ok());
			assert!(params.validate(expected - 1).is_err());
		}
	}

	#[test]
	fn probe_comparison_rejects_changed_padding_with_the_same_visible_geometry() {
		let probed = VideoParams {
			width: 800,
			height: 480,
			fps: 30,
			bytes_per_line: 832,
			buffer_height: 496,
		};
		assert!(probed.ensure_matches(&probed).is_ok());

		for actual in [
			VideoParams {
				bytes_per_line: 864,
				..probed
			},
			VideoParams {
				buffer_height: 512,
				..probed
			},
		] {
			// Both layouts are valid, but the provider has already prepared for the probe.
			actual.validate(actual.frame_size().unwrap()).unwrap();
			let error = actual.ensure_matches(&probed).unwrap_err().to_string();
			assert!(error.contains("changed since probe"));
			assert!(error.contains(&format!("expected {probed:?}")));
			assert!(error.contains(&format!("driver returned {actual:?}")));
		}
	}

	#[test]
	fn yu12_frame_size_rejects_overflow() {
		let mut params = VideoParams {
			width: 2,
			height: 2,
			fps: 30,
			bytes_per_line: usize::MAX - 1,
			buffer_height: 2,
		};
		assert_eq!(
			params.frame_size().unwrap_err().to_string(),
			"YU12 Y plane size overflow"
		);
		params.bytes_per_line = (usize::MAX / 8 * 3) & !1;
		assert_eq!(
			params.frame_size().unwrap_err().to_string(),
			"YU12 buffer size overflow"
		);
	}
}
