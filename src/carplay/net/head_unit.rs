use std::net::SocketAddrV6;

use anyhow::{Context, Result, anyhow, bail};
use log::debug;
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};

use crate::carplay::MacAddr;

pub struct HeadUnitServer {
	pub mac: MacAddr,
	pub addr: SocketAddrV6,
	pub pairing_identity: String,
	pub model: String,
}

fn parse_head_unit_service(
	info: &ResolvedService,
	target_mac: Option<MacAddr>,
	scope_id: u32,
) -> Result<HeadUnitServer> {
	let flags = usize::from_str_radix(
		info.txt_properties
			.get_property_val_str("flags")
			.context("no flags property in carplay")?
			.trim_start_matches("0x"),
		16,
	)
	.context("invalid flags property")?;

	if flags & 0b010 != 0 {
		bail!("head unit not configured according to flags");
	}

	let device_id = info
		.txt_properties
		.get_property_val_str("deviceid")
		.context("no device id txt property in carplay")?;

	let mac = MacAddr::try_from(
		device_id
			.splitn(6, ":")
			.map(|x| u8::from_str_radix(x, 16))
			.collect::<std::result::Result<Vec<_>, _>>()
			.context("invalid mac address")?,
	)
	.map_err(|_| anyhow!("invalid mac address"))?;

	if target_mac.is_some_and(|x| mac != x) {
		bail!("head unit mac does not match");
	}

	let ip = info
		.addresses
		.iter()
		.find_map(|x| {
			if let ScopedIp::V6(x) = x
				&& x.scope_id().index == scope_id
			{
				Some(*x.addr())
			} else {
				None
			}
		})
		.context("no address found with ncm scope id")?;
	let addr = SocketAddrV6::new(ip, info.port, 0, scope_id);

	let pairing_identity = info
		.txt_properties
		.get_property_val_str("pi")
		.unwrap_or(device_id)
		.to_string();

	let model = info
		.txt_properties
		.get_property_val_str("model")
		.context("no model txt property in carplay")?
		.to_string();

	Ok(HeadUnitServer {
		mac,
		addr,
		pairing_identity,
		model,
	})
}

pub async fn find_head_unit_server(
	mdns: ServiceDaemon,
	target_mac: Option<MacAddr>,
	scope_id: u32,
) -> Result<HeadUnitServer> {
	let browse = mdns
		.browse("_airplay._tcp.local.")
		.context("failed to browse for head unit")?;

	loop {
		if let ServiceEvent::ServiceResolved(info) = browse
			.recv_async()
			.await
			.context("failed to get browse event")?
		{
			match parse_head_unit_service(&info, target_mac, scope_id) {
				Ok(x) => break Ok(x),
				Err(err) => {
					debug!("head unit service {info:?} rejected: {err:?}")
				}
			}
		}
	}
}
