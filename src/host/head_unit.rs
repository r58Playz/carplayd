//! Finding the head unit after the USB role switch, when we are the host.
//!
//! Per §30.2.4.2 the accessory then presents a single configuration holding a
//! vendor-specific iAP2 interface plus a USB NCM control/data pair. We claim the iAP2
//! interface ourselves; the NCM pair belongs to the kernel's `cdc_ncm` driver, which is
//! what brings up the `usb0` netdev the CarPlay session runs over.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{debug, info};
use nusb::{Device, DeviceInfo, list_devices};

use crate::iap2::transport::bulk::{IAP2_INTERFACE, NCM_CONTROL_INTERFACE, NCM_DATA_INTERFACE};

/// The head unit, once enumerated.
#[derive(Clone)]
pub struct HeadUnit {
	pub device: Device,
	pub device_info: DeviceInfo,
	pub iap2_interface: u8,
	pub ncm_control_interface: u8,
	pub ncm_data_interface: u8,
}

/// The interface number matching a class/subclass/protocol triple.
fn interface_matching(d: &DeviceInfo, (class, subclass, protocol): (u8, u8, u8)) -> Option<u8> {
	d.interfaces()
		.find(|i| i.class() == class && i.subclass() == subclass && i.protocol() == protocol)
		.map(|i| i.interface_number())
}

pub async fn find_iap2_device(attempts: u32, interval: Duration) -> Result<HeadUnit> {
	for attempt in 0..attempts {
		let devices: Vec<DeviceInfo> = list_devices()
			.await
			.context("failed to list devices")?
			.collect();

		debug!("attempt {attempt}: {} USB device(s)", devices.len());

		let found = devices.into_iter().find_map(|d| {
			interface_matching(&d, IAP2_INTERFACE)
				.zip(interface_matching(&d, NCM_CONTROL_INTERFACE))
				.zip(interface_matching(&d, NCM_DATA_INTERFACE))
				.map(|((a, b), c)| (d, a, b, c))
		});

		if let Some((info, iap2_interface, ncm_control_interface, ncm_data_interface)) = found {
			info!(
				"iAP2 interface {iap2_interface} on {:04x}:{:04x} after {attempt} attempt(s)",
				info.vendor_id(),
				info.product_id(),
			);

			return Ok(HeadUnit {
				device: info.open().await.context("failed to open iAP2 device")?,
				device_info: info,
				iap2_interface,
				ncm_control_interface,
				ncm_data_interface,
			});
		}

		tokio::time::sleep(interval).await;
	}

	bail!(
		"no iAP2 interface ({:02x}/{:02x}/{:02x}) appeared after {attempts} attempt(s)",
		IAP2_INTERFACE.0,
		IAP2_INTERFACE.1,
		IAP2_INTERFACE.2,
	)
}
