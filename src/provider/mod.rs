use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use v4l2r::ioctl::PlaneMapping;

use crate::{
	carplay::{
		input::{InputDevice, InputReport},
		media::NowPlaying,
	},
	v4l2::VideoParams,
};

pub mod decoder;
pub mod frame;
pub mod network;
pub mod waiting;

pub trait CarplayProvider {
	type Connection: CarplayProviderConnection;

	/// handshake, etc
	async fn wait_for_connection(
		&mut self,
		video: VideoParams,
		input: Vec<InputDevice>,
	) -> Result<Self::Connection>;

	async fn run(self) -> Result<()>;
}

pub trait CarplayProviderConnection {
	type Screen: CarplayProviderScreen + 'static;
	type Media: CarplayProviderMedia + 'static;

	async fn process_input(&mut self, device: InputDevice, events: InputReport) -> Result<()>;

	/// initialize/connect the video stream. shouldn't take long
	async fn get_screen(&mut self) -> Result<Self::Screen>;

	async fn get_media(&mut self) -> Result<Self::Media>;
}

/// buffer must be yuv420
#[async_trait]
pub trait CarplayProviderScreen: Send {
	async fn next_frame(&mut self, buffer: &mut PlaneMapping) -> Result<Option<Instant>>;
}

pub enum MediaEvent {
	NowPlayingChanged(Option<NowPlaying>),
}

#[async_trait]
pub trait CarplayProviderMedia: Send {
	/// must be cancel safe
	async fn next_now_playing_change(&mut self) -> Result<MediaEvent>;

	/// None: no media source
	fn get_now_playing(&mut self) -> Result<Option<NowPlaying>>;
}

enum ProviderConnectionCommand {
	ProcessInput(InputDevice, InputReport, oneshot::Sender<Result<()>>),
	GetScreen(oneshot::Sender<Result<Box<dyn CarplayProviderScreen>>>),
	GetMedia(oneshot::Sender<Result<Box<dyn CarplayProviderMedia>>>),
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
				ProviderConnectionCommand::ProcessInput(device, event, tx) => {
					let _ = tx.send(self.0.process_input(device, event).await);
				}
				ProviderConnectionCommand::GetScreen(tx) => {
					let _ = tx.send(
						self.0
							.get_screen()
							.await
							.map(|x| Box::new(x) as Box<dyn CarplayProviderScreen>),
					);
				}
				ProviderConnectionCommand::GetMedia(tx) => {
					let _ = tx.send(
						self.0
							.get_media()
							.await
							.map(|x| Box::new(x) as Box<dyn CarplayProviderMedia>),
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

	pub async fn get_media(&self) -> Result<Box<dyn CarplayProviderMedia>> {
		let (tx, rx) = oneshot::channel();
		self.0
			.send(ProviderConnectionCommand::GetMedia(tx))
			.context("failed to ask for media")?;
		rx.await.context("failed to wait for media")?
	}

	pub async fn process_input(&self, device: InputDevice, event: InputReport) -> Result<()> {
		let (tx, rx) = oneshot::channel();
		self.0
			.send(ProviderConnectionCommand::ProcessInput(device, event, tx))
			.context("failed to ask for input process")?;
		rx.await.context("failed to wait for processed input")?
	}
}
