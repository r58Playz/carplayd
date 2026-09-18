pub mod dwc2;
pub mod head_unit;
pub mod ncm;

use anyhow::{Context, Result};
use log::{info, warn};

use crate::{
	carplay::{CARPLAY_DEVICE_NAME, usb::head_unit::HeadUnitUsb}, iap2::{
		ControlSession, DeviceProfile, Handshake, Link, LinkParams, MAX_PACKET_LEN, Role,
		policy::{self, Policy},
		transport::bulk::{self, BulkTransport},
	}, pairing::identity::PairIdentity,
};

struct UsbPolicy {}

impl Policy for UsbPolicy {}

pub struct CarplayUsbSession {
	session: ControlSession<BulkTransport>,
	handshake: Handshake<UsbPolicy>,
	pub params: LinkParams,
}

impl CarplayUsbSession {
	pub async fn new(identity: &PairIdentity, head_unit: &HeadUnitUsb) -> Result<Self> {
		let transport = bulk::open(&head_unit.device, head_unit.iap2_interface, MAX_PACKET_LEN)
			.await
			.context("failed to open the iAP2 bulk transport")?;

		let link = Link::new(transport, Role::Device, LinkParams::usb_recommended());
		let mut session = ControlSession::new(link);
		let mut handshake = Handshake::with_policy(
			DeviceProfile {
				name: CARPLAY_DEVICE_NAME.to_string(),
				device_uuid: identity.pairing_id().to_string(),
				..Default::default()
			},
			UsbPolicy {},
		);

		let params = policy::drive_until_ready(&mut session, &mut handshake)
			.await
			.context("iAP2 handshake failed")?;

		info!("iAP2 control session ready over {params:?}");

		let control = head_unit.ncm_control_interface;
		let data = head_unit.ncm_data_interface;
		match handshake.carplay_interface() {
			// The nominated number must be the NCM control interface (Table 30-1).
			Some(nominated) if nominated == control => {
				info!("accessory nominated {nominated} (matches control), data {data}")
			}
			Some(nominated) => warn!(
				"accessory nominated {nominated} but its NCM interfaces are control {control} / data {data}",
			),
			None => warn!("accessory did not nominate a NCM interface"),
		}

		Ok(Self {
			session,
			handshake,
			params,
		})
	}

	pub async fn run(mut self) -> Result<()> {
		policy::drive(&mut self.session, &mut self.handshake)
			.await
			.context("iAP2 session ended")
	}
}
