use std::time::Duration;

use anyhow::{Context, Result};
use log::{LevelFilter, debug, error, info, trace};
use tokio::task::spawn_blocking;

use crate::{
	gadget::{run_gadget, udc_name},
	host::{UsbIap2, find_iap2_device},
};

mod gadget;
mod host;
mod iap2;

#[tokio::main]
async fn main() -> Result<()> {
	env_logger::builder()
		.filter_level(LevelFilter::Off)
		.filter_module("carplayd", LevelFilter::Trace)
		.parse_default_env()
		.init();

	let sn = std::env::args().nth(1).context("no serial provided")?;

	let controller_name = spawn_blocking(|| udc_name())
		.await?
		.context("failed to get controller name")?;

	debug!("got controller {controller_name}");


	info!("starting gadget mode");

	let success = spawn_blocking(move || run_gadget(&sn))
		.await?
		.context("failed to run gadget")?;
	if !success {
		error!("iAP2 accessory rejected us");
		return Ok(());
	}

	debug!("resetting dwc2 for {controller_name}");
	tokio::time::sleep(Duration::from_millis(100)).await;
	tokio::fs::write("/sys/bus/platform/drivers/dwc2/unbind", &controller_name)
		.await
		.context("failed to unbind dwc2")?;
	trace!("unbound dwc2 from {controller_name}");
	tokio::time::sleep(Duration::from_millis(100)).await;
	tokio::fs::write("/sys/bus/platform/drivers/dwc2/bind", &controller_name)
		.await
		.context("failed to bind dwc2")?;
	trace!("bound dwc2 to {controller_name}");
	tokio::time::sleep(Duration::from_millis(100)).await;


	info!("starting host mode");

	let iap2_device = find_iap2_device()
		.await
		.context("failed to get carplay head unit")?;
	let Some((iap2_device, iap2_interface_id)) = iap2_device else {
		error!("carplay head unit not found");
		return Ok(());
	};
	let iap2_interface = iap2_device
		.claim_interface(iap2_interface_id)
		.await
		.context("failed to claim iap2 interface")?;

	debug!(
		"found iap2 device {:?} with iap2 interface number {iap2_interface_id}",
		iap2_device.device_descriptor()
	);

	info!("starting iap2");

	let mut iap2 = UsbIap2::new(iap2_interface).await.context("failed to bringup iap2")?;
	iap2.until_carplay_ready().await.context("failed to poll iap2 until carplay ready")?;

	info!("iap2 carplay ready");

	Ok(())
}
