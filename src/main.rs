use std::{
	sync::Arc,
	time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use log::{LevelFilter, debug, info, warn};
use tokio::task::spawn_blocking;

use crate::{
	dwc2::{Dwc2, Mode},
	gadget::{run_gadget, udc_name},
	host::head_unit::{HeadUnit, find_iap2_device},
	iap2::{
		ControlSession, DefaultPolicy, DeviceProfile, Handshake, Link, LinkParams, MAX_PACKET_LEN,
		Role, policy, transport::bulk,
	},
};

mod dwc2;
mod gadget;
mod host;
mod iap2;

/// §60.6 step 7: the accessory waits at least 1000 ms for us to start enumerating.
const HEAD_UNIT_ATTEMPTS: u32 = 200;
const HEAD_UNIT_INTERVAL: Duration = Duration::from_millis(10);

async fn force_mode(dwc2: &Arc<Dwc2>, mode: Mode) -> Result<()> {
	let dwc2 = Arc::clone(dwc2);
	spawn_blocking(move || dwc2.force_mode(mode)).await?
}

#[tokio::main]
async fn main() -> Result<()> {
	env_logger::builder()
		.filter_level(LevelFilter::Warn)
		.filter_module("carplayd", LevelFilter::Trace)
		.format_timestamp_millis()
		.parse_default_env()
		.init();

	let sn = std::env::args().nth(1).context("no serial provided")?;

	let controller_name = spawn_blocking(udc_name)
		.await?
		.context("failed to get controller name")?;
	debug!("got controller {controller_name}");

	let dwc2 = Arc::new(Dwc2::open(&controller_name).context("failed to open dwc2 registers")?);

	info!("starting gadget mode");
	force_mode(&dwc2, Mode::Device)
		.await
		.context("failed to force controller into gadget mode")?;

	let (gadget, success) = run_gadget(&sn).await.context("failed to run gadget")?;
	if !success? {
		bail!("iAP2 accessory rejected us");
	}

	let switched = Instant::now();

	info!("starting host mode");
	gadget
		.unbind()
		.context("failed to disconnect gadget from the bus")?;
	force_mode(&dwc2, Mode::Host)
		.await
		.context("failed to force controller into host mode")?;
	debug!(
		"host mode reached {:?} after the role switch",
		switched.elapsed()
	);

	let head_unit = find_iap2_device(HEAD_UNIT_ATTEMPTS, HEAD_UNIT_INTERVAL)
		.await
		.context("failed to find the carplay head unit")?;
	info!(
		"found head unit {:?} after the role switch",
		switched.elapsed()
	);

	gadget.remove().context("failed to remove gadget")?;

	let transport = bulk::open(&head_unit.device, head_unit.iap2_interface, MAX_PACKET_LEN)
		.await
		.context("failed to open the iAP2 bulk transport")?;

	let link = Link::new(transport, Role::Device, LinkParams::usb_recommended());
	let mut session = ControlSession::new(link);
	let mut handshake = Handshake::with_policy(DeviceProfile::default(), DefaultPolicy);

	info!(
		"iAP2 over bulk with {:04x}:{:04x}; waiting for the accessory's detection sequence",
		head_unit.vendor_id, head_unit.product_id
	);
	let result = policy::drive(&mut session, &mut handshake, |h, params| {
		info!("iAP2 control session ready over {params:?}");
		let ncm = (
			head_unit.ncm_control_interface,
			head_unit.ncm_data_interface,
		);
		match h.carplay_interface() {
			// The nominated number must be the NCM control interface (Table 30-1).
			Some(nominated) if nominated == ncm.0 => {
				info!(
					"CarPlay rides on NCM control interface {nominated}, data {:?}",
					ncm.1
				)
			}
			Some(nominated) => warn!(
				"accessory nominated CarPlay interface {nominated} but its NCM interfaces \
				 are control {} / data {}",
				ncm.0, ncm.1
			),
			None => warn!(
				"accessory did not nominate a CarPlay interface in its \
				 USBHostTransportComponent"
			),
		}
	})
	.await;

	Err(result
		.expect_err("policy::drive only returns on failure")
		.into())
}
