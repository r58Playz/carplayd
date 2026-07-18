use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, info, warn};
use nusb::{Device, DeviceInfo, list_devices};

/// Snapshot the live host-side state right after a role switch: the forced USB role,
/// the USB buses/devices the kernel knows about, and the tail of the kernel log. This
/// is the only reliable way to tell whether `echo host` actually started the host
/// controller — look for a "connection id status change" line (the OTG interrupt that
/// `dwc2_force_mode` raises) followed by a root hub / a `new … device`. nusb does not
/// list root hubs, and the role file reverts to `none` once the cable is unplugged, so
/// neither is trustworthy after the fact.
pub async fn dump_host_state(role_path: &str) {
	match tokio::fs::read_to_string(role_path).await {
		Ok(s) => info!("usb role = {}", s.trim()),
		Err(e) => warn!("could not read {role_path}: {e}"),
	}

	match tokio::fs::read_dir("/sys/bus/usb/devices").await {
		Ok(mut rd) => {
			let mut names = Vec::new();
			while let Ok(Some(entry)) = rd.next_entry().await {
				names.push(entry.file_name().to_string_lossy().into_owned());
			}
			names.sort();
			info!("/sys/bus/usb/devices = {names:?}");
		}
		Err(e) => warn!("could not read /sys/bus/usb/devices: {e}"),
	}

	match tokio::process::Command::new("dmesg").output().await {
		Ok(out) => {
			let text = String::from_utf8_lossy(&out.stdout);
			let lines: Vec<&str> = text.lines().collect();
			let start = lines.len().saturating_sub(25);
			for line in &lines[start..] {
				info!("dmesg | {line}");
			}
		}
		Err(e) => warn!("could not run dmesg: {e}"),
	}
}

/// Dump every USB device the host controller currently sees, with full descriptors
/// (all configurations, alternate settings, and endpoints). Returns the count.
pub async fn dump_usb_devices() -> Result<usize> {
	let devices: Vec<DeviceInfo> = list_devices()
		.await
		.context("failed to list devices")?
		.collect();

	info!("=== {} USB device(s) ===", devices.len());
	for d in &devices {
		info!(
			"bus {} addr {}  {:04x}:{:04x}  bcdDevice {:04x}  dev-class {:02x}/{:02x}/{:02x}  mfr={:?} product={:?} serial={:?}",
			d.busnum(),
			d.device_address(),
			d.vendor_id(),
			d.product_id(),
			d.device_version(),
			d.class(),
			d.subclass(),
			d.protocol(),
			d.manufacturer_string(),
			d.product_string(),
			d.serial_number(),
		);
		// Quick interface view straight from enumeration metadata (no open needed).
		for i in d.interfaces() {
			info!(
				"    if{} class {:02x}/{:02x}/{:02x} string={:?}",
				i.interface_number(),
				i.class(),
				i.subclass(),
				i.protocol(),
				i.interface_string(),
			);
		}
		// Full descriptor tree (alt settings + endpoints) needs an open handle.
		match d.open().await {
			Ok(dev) => dump_device_descriptors(&dev),
			Err(e) => warn!("    (could not open for full descriptors: {e})"),
		}
	}

	Ok(devices.len())
}

fn dump_device_descriptors(dev: &Device) {
	for cfg in dev.configurations() {
		info!(
			"    config {} ({} interfaces)",
			cfg.configuration_value(),
			cfg.num_interfaces(),
		);
		for iface in cfg.interfaces() {
			for alt in iface.alt_settings() {
				info!(
					"      if{}.{} class {:02x}/{:02x}/{:02x} ({} endpoints)",
					alt.interface_number(),
					alt.alternate_setting(),
					alt.class(),
					alt.subclass(),
					alt.protocol(),
					alt.num_endpoints(),
				);
				for ep in alt.endpoints() {
					info!(
						"        ep {:#04x} {:?} max-packet {}",
						ep.address(),
						ep.transfer_type(),
						ep.max_packet_size(),
					);
				}
			}
		}
	}
}

/// True if a device exposes the USB Host Mode iAP2 interface (class FF/F0/00,
/// §60.4.2). The `'iAP Interface'` string is *not* required — it is often not
/// cached, and the class triple is the authoritative match.
fn iap2_interface(d: &DeviceInfo) -> Option<u8> {
	d.interfaces()
		.find(|i| i.class() == 0xFF && i.subclass() == 0xF0 && i.protocol() == 0x00)
		.map(|i| i.interface_number())
}

/// Poll for the CarPlay head unit's iAP2 interface, dumping the full device list
/// each round so you can watch it appear (or confirm nothing ever attaches).
pub async fn find_iap2_device() -> Result<Option<(Device, u8)>> {
	const ATTEMPTS: u32 = 40;
	const INTERVAL: Duration = Duration::from_millis(100);

	let mut last_count = usize::MAX;
	for attempt in 0..ATTEMPTS {
		let devices: Vec<DeviceInfo> = list_devices()
			.await
			.context("failed to list devices")?
			.collect();

        debug!("attempt {attempt}: {} USB device(s)", devices.len());

		// Dump in full whenever the device set changes, so the log isn't spammed
		// but every new attach is visible.
		if devices.len() != last_count {
			last_count = devices.len();
			info!("attempt {attempt}: {} USB device(s)", devices.len());
			let _ = dump_usb_devices().await;
		}

		if let Some((d, iface)) = devices.iter().find_map(|d| iap2_interface(d).map(|n| (d, n))) {
			info!("iAP2 interface found on {:04x}:{:04x} interface {iface} after {attempt} attempt(s)", d.vendor_id(), d.product_id());
			let dev = d.open().await.context("failed to open iAP2 device")?;
			return Ok(Some((dev, iface)));
		}

		tokio::time::sleep(INTERVAL).await;
	}

	warn!(
		"no iAP2 interface after {ATTEMPTS} attempts ({}s) — the head unit never attached as a USB device",
		ATTEMPTS as u64 * INTERVAL.as_millis() as u64 / 1000
	);
	Ok(None)
}
