use std::time::Duration;

use anyhow::{Context, Result};
use nusb::{
	Interface,
	io::{EndpointRead, EndpointWrite},
	transfer::Bulk,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::iap2::state_machine::{Iap2Machine, Iap2MachineConfig, MachineOutput};

pub struct UsbIap2 {
	read: EndpointRead<Bulk>,
	write: EndpointWrite<Bulk>,
	machine: Iap2Machine,
}
impl UsbIap2 {
	async fn handle_actions(
		write: &mut EndpointWrite<Bulk>,
		actions: Vec<MachineOutput>,
	) -> Result<()> {
		for action in actions {
			if let MachineOutput::Transmit(buf) = action {
				write
					.write_all(&buf)
					.await
					.context("failed to write to usb")?;
			}
		}
		Ok(())
	}

	pub async fn new(interface: Interface) -> Result<Self> {
		let read = EndpointRead::new(
			interface
				.endpoint(0x01)
				.context("failed to get read endpoint")?,
			10240,
		);
		let mut write = EndpointWrite::new(
			interface
				.endpoint(0x81)
				.context("failed to get write endpoint")?,
			10240,
		)
		.with_write_timeout(Duration::from_secs(1));

		let mut machine = Iap2Machine::new(Iap2MachineConfig::iphone_default());
		Self::handle_actions(&mut write, machine.start().context("failed to start iap2")?).await?;

		Ok(Self {
			read,
			write,
			machine,
		})
	}

	pub async fn step(&mut self) -> Result<()> {
		let mut read = self.read.until_short_packet();
		let mut buf = Vec::with_capacity(10240);
		read.read_to_end(&mut buf)
			.await
			.context("failed to read from usb")?;
		read.consume_end().context("failed to consume usb packet")?;
		let actions = self
			.machine
			.on_transport_bytes(&buf)
			.context("failed to step iap2")?;
		Self::handle_actions(&mut self.write, actions).await?;
		Ok(())
	}

	pub async fn until_carplay_ready(&mut self) -> Result<()> {
		while !self.machine.is_ready_for_carplay() {
			self.step().await?;
		}
		Ok(())
	}
}
