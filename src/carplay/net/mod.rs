use std::net::SocketAddrV6;

use anyhow::{Context, Result, bail};
use log::info;

use crate::{
	carplay::{
		CARPLAY_DEVICE_NAME, CARPLAY_MODEL, CARPLAY_OS_BUILD_VERSION, CARPLAY_SESSION_ID,
		CARPLAY_SOURCE_VERSION, DeviceIdentity, format_mac_lower, format_mac_upper,
		net::{
			conn::HeadUnitTcpConnection,
			head_unit::HeadUnitServer,
			proto::{
				command::AccessoryCommand,
				info::{AccessoryInfo, InfoRequest},
				setup::{SetupRequest, SetupResponse},
			},
			rtsp::{Method, Response},
		},
	},
	pairing::{
		identity::{PairIdentity, PairingStore},
		keys::SessionKeys,
	},
};

pub mod codec;
pub mod conn;
pub mod head_unit;
pub mod proto;
pub mod rtsp;
pub mod timing;

pub struct CarplayNetSession {
	server: HeadUnitServer,
	shared_secret: [u8; 32],
	info: AccessoryInfo,

	control: HeadUnitTcpConnection,
	events: HeadUnitTcpConnection,
}
impl CarplayNetSession {
	pub async fn new(
		pair_identity: &mut PairIdentity,
		pairing: &mut PairingStore,
		head_unit: HeadUnitServer,
		identity: DeviceIdentity,
		timing_port: u16,
	) -> Result<Self> {
		let mut control = HeadUnitTcpConnection::new(head_unit.addr)
			.await
			.context("failed to connect to control socket")?;

		let ltpk = match pairing.get(&head_unit.pairing_identity) {
			Some(x) => x,
			None => {
				let res = control
					.pair_setup(pair_identity)
					.await
					.context("failed to pair_setup")?;
				if res.accessory_id != head_unit.pairing_identity {
					bail!(
						"{:?} accessory_id did not match {:?} pairing_identity",
						res.accessory_id,
						head_unit.pairing_identity
					);
				}
				pairing
					.insert(&res.accessory_id, res.accessory_ltpk)
					.await
					.context("failed to save pairing identity")?;
				res.accessory_ltpk
			}
		};

		let shared_secret = control
			.pair_verify(pair_identity, ltpk)
			.await
			.context("failed to pair_verify")?;

		control
			.upgrade(SessionKeys::control(&shared_secret))
			.context("failed to upgrade")?;

		control.mfi_auth().await.context("failed to do mfi auth")?;

		let info = control
			.req::<AccessoryInfo>(Method::Get, "/info", &InfoRequest::all())
			.await
			.context("failed to GET /info")?;

		let setup_req = SetupRequest {
			device_id: format_mac_lower(identity.device_id),
			mac_address: format_mac_upper(identity.mac_address),
			model: CARPLAY_MODEL.to_string(),
			name: CARPLAY_DEVICE_NAME.to_string(),
			os_build_version: CARPLAY_OS_BUILD_VERSION.to_string(),
			source_version: CARPLAY_SOURCE_VERSION.to_string(),
			timing_port,
			features: None,
			keep_alive_low_power: None,

			session_uuid: CARPLAY_SESSION_ID.to_string(),
			streams: None,
		};
		let setup = control
			.req::<SetupResponse>(Method::Setup, "/", &setup_req)
			.await
			.context("failed to SETUP /")?;

		let events_addr = SocketAddrV6::new(
			*head_unit.addr.ip(),
			setup
				.event_port
				.context("no event port provided in SETUP")?,
			0,
			head_unit.addr.scope_id(),
		);
		let mut events = HeadUnitTcpConnection::new(events_addr)
			.await
			.context("failed to connect to event port")?;
		events
			.upgrade(SessionKeys::event(&shared_secret))
			.context("failed to upgrade events channel")?;

		Ok(Self {
			server: head_unit,
			shared_secret,
			info,

			control,
			events,
		})
	}

	pub async fn run(mut self) -> Result<()> {
		let events = async {
			while let Some(req) = self
				.events
				.next_request()
				.await
				.context("failed to wait for next events req")?
			{
				if req.method != Method::Post || req.target != "/command" {
					bail!("invalid events req {req:?}");
				}
				let cmd = AccessoryCommand::from_binary(&req.body);

				info!("got events command {cmd:?}");

				self.events
					.respond(&req, Response::new(400, "Bad Request"))
					.await
					.context("failed to respond to events req")?;
			}

			Ok(())
		};

		events.await
	}
}
