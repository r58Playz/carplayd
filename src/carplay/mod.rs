use anyhow::{Context, Result};

use crate::{
	carplay::{
		net::{CarplayNetSession, head_unit::HeadUnitServer},
		usb::{CarplayUsbSession, head_unit::HeadUnitUsb},
	},
	pairing::identity::{Identity, PairingStore},
};

pub mod ctrl_server;
pub mod net;
pub mod usb;

pub type MacAddr = [u8; 6];

pub struct CarplaySession {
}
impl CarplaySession {
	pub async fn new(
		head_unit_usb: &HeadUnitUsb,
		head_unit_net: HeadUnitServer,
		identity: &mut Identity,
		pairing: &mut PairingStore,
	) -> Result<(Self, CarplayUsbSession, CarplayNetSession)> {
		// iAP2 first, relax this once pairing is known good
		let usb = CarplayUsbSession::new(head_unit_usb)
			.await
			.context("failed to establish the iAP2 session")?;

		let net = CarplayNetSession::new(head_unit_net, identity, pairing)
			.await
			.context("failed to establish the CarPlay session")?;

		Ok((Self {}, usb, net))
	}

    pub async fn run(self) -> Result<()> {
        // TODO
        Ok(())
    }
}
