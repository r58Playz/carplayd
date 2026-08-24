pub mod dwc2;
pub mod head_unit;
pub mod ncm;

use anyhow::{Context, Result};
use log::{info, warn};

use crate::{
	carplay::usb::head_unit::HeadUnitUsb,
	iap2::{
		ControlSession, DefaultPolicy, DeviceProfile, Handshake, Link, LinkParams, MAX_PACKET_LEN,
		Role, policy, transport::bulk,
	},
};

// TODO policy here

pub async fn run_usb_iap2(head_unit: HeadUnitUsb) -> Result<()> {
	let transport = bulk::open(&head_unit.device, head_unit.iap2_interface, MAX_PACKET_LEN)
		.await
		.context("failed to open the iAP2 bulk transport")?;

	let link = Link::new(transport, Role::Device, LinkParams::usb_recommended());
	let mut session = ControlSession::new(link);
	let mut handshake = Handshake::with_policy(DeviceProfile::default(), DefaultPolicy);

	let result = policy::drive(&mut session, &mut handshake, |h, params| {
		info!("iAP2 control session ready over {params:?}");
		let control = head_unit.ncm_control_interface;
		let data = head_unit.ncm_data_interface;
		match h.carplay_interface() {
			// The nominated number must be the NCM control interface (Table 30-1).
			Some(nominated) if nominated == control => {
				info!("accessory nominated {nominated} (matches control), data {data}")
			}
			Some(nominated) => warn!(
				"accessory nominated {nominated} but its NCM interfaces are control {control} / data {data}",
			),
			None => warn!("accessory did not nominate a NCM interface"),
		}
	})
	.await;

	Err(result
		.expect_err("policy::drive only returns on failure")
		.into())
}
