use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use v4l2r::ioctl::PlaneMapping;

use crate::{
	provider::{
		CarplayProviderScreen,
		frame::{Yuv420Frame, Yuv420Layout},
	},
	v4l2::VideoParams,
};

const WAITING_YUV: &[u8] = include_bytes!("./waiting.yuv");
const WAITING_LAYOUT: Yuv420Layout = Yuv420Layout {
	width: 800,
	height: 480,
	bytes_per_line: 800,
	buffer_height: 480,
};

pub struct WaitingProvider(VideoParams, Duration, Instant);
impl WaitingProvider {
	pub fn new(params: VideoParams) -> Self {
		let duration = Duration::from_micros(1_000_000 / params.fps as u64);
		Self(params, duration, Instant::now() - duration)
	}

	fn generate_frame(&self, buffer: &mut [u8]) -> Result<()> {
		Yuv420Frame::from_bytes(WAITING_YUV, WAITING_LAYOUT)?.copy_centered_to(buffer, self.0)
	}
}
#[async_trait]
impl CarplayProviderScreen for WaitingProvider {
	async fn next_frame(&mut self, buffer: &mut PlaneMapping) -> Result<Option<Instant>> {
		tokio::time::sleep_until(self.2.into()).await;
		let now = Instant::now();
		self.2 = now + self.1;

		self.generate_frame(buffer)?;

		Ok(Some(now))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn waiting_asset_copies_unchanged_at_its_native_size() {
		let provider = WaitingProvider::new(VideoParams {
			width: 800,
			height: 480,
			fps: 30,
			bytes_per_line: 800,
			buffer_height: 480,
		});
		let mut dst = vec![0; WAITING_YUV.len()];
		provider.generate_frame(&mut dst).unwrap();
		assert_eq!(dst, WAITING_YUV);
	}
}
