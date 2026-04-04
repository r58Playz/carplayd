use anyhow::{Context, Result};
use nusb::{Device, list_devices};

mod iap2;

pub use iap2::UsbIap2;

pub async fn find_iap2_device() -> Result<Option<(Device, u8)>> {
	let device = list_devices()
		.await
		.context("failed to get device list")?
		.find_map(|x| {
			let iface = x
				.interfaces()
				.find(|x| {
					x.class() == 0xFF
						&& x.subclass() == 0xF0
						&& x.protocol() == 0x00
						&& x.interface_string().is_some_and(|x| x == "iAP Interface")
				})
				.map(|x| x.interface_number());
			iface.map(|y| (x, y))
		});

	if let Some((dev, iface)) = device {
		Ok(Some((
			dev.open().await.context("failed to open device")?,
			iface,
		)))
	} else {
		Ok(None)
	}
}
