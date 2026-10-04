use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;

use crate::{
	carplay::{
		net::{
			CarplayNetSession,
			head_unit::HeadUnitServer,
			timing::{Clock, TimingServer},
		},
		usb::ncm::NcmLink,
	},
	pairing::identity::{PairIdentity, PairingStore},
};

pub mod ctrl_server;
pub mod net;
pub mod usb;
pub mod modes;

pub type MacAddr = [u8; 6];

pub const CARPLAY_SESSION_ID: u64 = 676767676767676767;
pub const CARPLAY_DEVICE_NAME: &str = "carplayd";
pub const CARPLAY_MODEL: &str = "iPhone10,2";
pub const CARPLAY_OS_BUILD_VERSION: &str = "20H350";
pub const CARPLAY_SOURCE_VERSION: &str = "695.5.1";

pub fn format_mac_lower(mac: MacAddr) -> String {
	mac.map(|byte| format!("{byte:02x}")).join(":")
}
pub fn format_mac_upper(mac: MacAddr) -> String {
	mac.map(|byte| format!("{byte:02X}")).join(":")
}

/// Derive a stable pseudo-MAC from the box's serial number.
pub fn derive_device_id(serial: &str) -> MacAddr {
	let digest = Sha256::digest(serial.as_bytes());
	let mut mac = MacAddr::default();
	mac.copy_from_slice(&digest[..6]);
	mac[0] = (mac[0] | 0x02) & 0xFE;
	mac
}

/// Who we claim to be on the wire.
#[derive(Clone, Copy, Debug)]
pub struct DeviceIdentity {
	/// Derived from the serial number by [`derive_device_id`].
	pub device_id: MacAddr,
	/// `macAddress` in `SETUP`: the address of the interface actually carrying the session.
	pub mac_address: MacAddr,
}

pub struct CarplaySession {
	clock: Clock,
	device: DeviceIdentity,
}
impl CarplaySession {
	pub async fn new(
		set: &mut JoinSet<Result<()>>,
		pair_identity: &mut PairIdentity,
		pairing: &mut PairingStore,

		device_id: MacAddr,
		link: NcmLink,

		head_unit_net: HeadUnitServer,
	) -> Result<(Self, CarplayNetSession)> {
		let timing_port = 6768;
		let clock = Clock::new();
		let device = DeviceIdentity {
			device_id,
			mac_address: link.mac,
		};

		set.spawn(
			TimingServer::bind(link.ip, link.scope_id, timing_port, clock)
				.await
				.context("failed to bind timing server")?
				.run(),
		);

		let net =
			CarplayNetSession::new(pair_identity, pairing, head_unit_net, device, timing_port)
				.await
				.context("failed to establish the CarPlay session")?;

		Ok((Self { clock, device }, net))
	}

	pub async fn run(self) -> Result<()> {
		// TODO
		Ok(())
	}
}
