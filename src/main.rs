use std::time::Duration;

use anyhow::{Context, Result};
use log::{LevelFilter, debug, error, info, trace};
use tokio::task::spawn_blocking;

use crate::{
	gadget::{run_gadget, udc_name},
	host::find_iap2_device,
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

	let controller_name = spawn_blocking(udc_name)
		.await?
		.context("failed to get controller name")?;

	debug!("got controller {controller_name}");

	let role_switch_path = format!("/sys/class/usb_role/{controller_name}-role-switch/role");

	info!("starting gadget mode");

	tokio::fs::write(&role_switch_path, "device")
		.await
		.context("failed to force controller into gadget mode")?;

	let success = run_gadget(&sn).await.context("failed to run gadget")?;
	if !success {
		error!("iAP2 accessory rejected us");
		return Ok(());
	}

	info!("starting host mode");
	tokio::fs::write(&role_switch_path, "host")
		.await
		.context("failed to force controller into host mode")?;

	// The car errors ~2s after the role switch, so this is the only window to see
	// whether `echo host` actually started the host controller. Look in the dmesg
	// tail for "connection id status change" (the OTG interrupt) + a device attach.
	tokio::time::sleep(Duration::from_millis(150)).await;
	host::dump_host_state(&role_switch_path).await;

	let iap2_device = find_iap2_device()
		.await
		.context("failed to get carplay head unit")?;
	let Some((iap2_device, iap2_interface_id)) = iap2_device else {
		error!("carplay head unit not found");
		return Ok(());
	};

	info!("carplay head unit found {iap2_device:?} {iap2_interface_id:?}");

	Ok(())
}
