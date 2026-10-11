use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use v4l2r::ioctl::PlaneMapping;

use crate::v4l2::VideoParams;

pub mod waiting;
pub mod network;
pub mod decoder;
pub mod frame;

pub trait CarplayProvider {
	type Connection: CarplayProviderConnection;

	/// handshake, etc
	async fn wait_for_connection(&mut self, video: VideoParams) -> Result<Self::Connection>;

	async fn run(self) -> Result<()>;
}

#[async_trait]
pub trait CarplayProviderConnection {
	type Screen: CarplayProviderScreen + 'static;

	/// initialize/connect the video stream. shouldn't take long
	async fn get_screen(&mut self) -> Result<Self::Screen>;
}

/// buffer must be yuv420
#[async_trait]
pub trait CarplayProviderScreen: Send {
	async fn next_frame(&mut self, buffer: &mut PlaneMapping) -> Result<Option<Instant>>;
}

enum ProviderConnectionCommand {
	GetScreen(oneshot::Sender<Result<Box<dyn CarplayProviderScreen>>>),
}

pub struct CarplayProviderConnectionServer<
	C: CarplayProviderConnection<Screen = S>,
	S: CarplayProviderScreen + 'static,
>(C, mpsc::UnboundedReceiver<ProviderConnectionCommand>);
impl<C: CarplayProviderConnection<Screen = S>, S: CarplayProviderScreen + 'static>
	CarplayProviderConnectionServer<C, S>
{
	pub fn new(conn: C) -> (Self, CarplayProviderConnectionProxy) {
		let (tx, rx) = mpsc::unbounded_channel();
		(Self(conn, rx), CarplayProviderConnectionProxy(tx))
	}

	pub async fn run(mut self) -> Result<()> {
		while let Some(command) = self.1.recv().await {
			match command {
				ProviderConnectionCommand::GetScreen(tx) => {
					let _ = tx.send(
						self.0
							.get_screen()
							.await
							.map(|x| Box::new(x) as Box<dyn CarplayProviderScreen>),
					);
				}
			}
		}

		Ok(())
	}
}

#[derive(Clone)]
pub struct CarplayProviderConnectionProxy(mpsc::UnboundedSender<ProviderConnectionCommand>);
impl CarplayProviderConnectionProxy {
	pub async fn get_screen(&self) -> Result<Box<dyn CarplayProviderScreen>> {
		let (tx, rx) = oneshot::channel();
		self.0
			.send(ProviderConnectionCommand::GetScreen(tx))
			.context("failed to ask for screen")?;
		rx.await.context("failed to wait for screen")?
	}
}
