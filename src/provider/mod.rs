use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use v4l2r::ioctl::PlaneMapping;

use crate::v4l2::VideoParams;

pub mod waiting;

#[async_trait]
pub trait CarplayProvider {
	type Connection: CarplayProviderConnection;

	async fn wait_for_connection(&mut self) -> Result<Self::Connection>;
}

#[async_trait]
pub trait CarplayProviderConnection {
	type Screen: CarplayProviderScreen;

	/// screen must target these dimensions
	/// cropping only supported from 0,0 top left
	fn get_screen(&self, dimensions: VideoParams) -> Result<Self::Screen>;
}

/// buffer must be yuv420
#[async_trait]
pub trait CarplayProviderScreen: Send {
	/// must be cancel safe!
	async fn next_frame(&mut self, buffer: &mut PlaneMapping) -> Result<Option<Instant>>;
}
