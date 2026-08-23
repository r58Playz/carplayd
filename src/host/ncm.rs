use std::{net::{IpAddr, Ipv6Addr}, pin::pin};

use anyhow::{Context, Result, bail};
use futures::{FutureExt, StreamExt, future::select};
use rtnetlink::{
	LinkUnspec, MulticastGroup,
	packet_core::NetlinkPayload,
	packet_route::{
		RouteNetlinkMessage,
		address::{AddressAttribute, AddressFlags},
		link::{AfSpecInet6, AfSpecUnspec, In6AddrGenMode, LinkAttribute},
	},
};
use tokio::fs::read_dir;

use crate::host::head_unit::HeadUnit;

pub async fn get_ncm_device_name(head_unit: &HeadUnit) -> Result<String> {
	let active_config = head_unit
		.device
		.active_configuration()
		.context("failed to get active config on head unit")?
		.configuration_value();

	let dev_sysfs = head_unit.device_info.sysfs_path();
	let dev_name = dev_sysfs
		.file_name()
		.context("no filename in device sysfs")?
		.to_string_lossy();

	let iface_sysfs = dev_sysfs
		.parent()
		.context("no parent in device sysfs")?
		.join(format!(
			"{dev_name}:{active_config}.{}",
			head_unit.ncm_control_interface
		));

	let ncm_device = read_dir(iface_sysfs.join("net"))
		.await
		.context("failed to read device sysfs net dir")?
		.next_entry()
		.await
		.context("failed to get next net dir entry")?
		.context("no net dir entry")?;

	Ok(ncm_device.file_name().to_string_lossy().into_owned())
}

pub async fn configure_ncm(device: &str) -> Result<(Ipv6Addr, u32)> {
	let (conn, handle, mut events) =
		rtnetlink::new_multicast_connection(&[MulticastGroup::Ipv6Ifaddr])
			.context("failed to create rtnetlink conn")?;

	let fut = pin!(async move {
		let link = handle
			.link()
			.get()
			.match_name(device)
			.execute()
			.next()
			.await
			.context("no link found for device")?
			.context("failed to get link for device")?;

		let mut dev_conf = *link
			.attributes
			.iter()
			.find_map(|x| {
				if let LinkAttribute::AfSpecUnspec(x) = x {
					x.iter().find_map(|x| {
						if let AfSpecUnspec::Inet6(x) = x {
							x.iter().find_map(|x| {
								if let AfSpecInet6::DevConf(x) = x {
									Some(x)
								} else {
									None
								}
							})
						} else {
							None
						}
					})
				} else {
					None
				}
			})
			.context("no devconf struct found for device")?;

		dev_conf.accept_ra = 0;
		dev_conf.optimistic_dad = 1;

		let attr = LinkAttribute::AfSpecUnspec(vec![AfSpecUnspec::Inet6(vec![
			AfSpecInet6::AddrGenMode(In6AddrGenMode::Eui64),
			AfSpecInet6::DevConf(dev_conf),
		])]);

		handle
			.link()
			.set(
				LinkUnspec::new_with_index(link.header.index)
					.up()
					.append_extra_attribute(attr)
					.build(),
			)
			.execute()
			.await
			.context("failed to set link attrs")?;

		while let Some((msg, _)) = events.next().await {
			if let NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewAddress(addr_msg)) =
				msg.payload
			{
				if addr_msg.header.index != link.header.index {
					continue;
				}

				if addr_msg.attributes.iter().any(
					|x| matches!(x, AddressAttribute::Flags(f) if f.contains(AddressFlags::Tentative)),
				) {
					continue;
				}

				let Some(v6) = addr_msg.attributes.iter().find_map(|x| {
					if let AddressAttribute::Address(IpAddr::V6(x)) = x
						&& x.is_unicast_link_local()
					{
						Some(*x)
					} else {
						None
					}
				}) else {
					continue;
				};

				return Ok((v6, link.header.index));
			}
		}

		bail!("rtnetlink events ended before address appeared");
	});

	select(
		conn.map(|_| bail!("rtnetlink connection exited early")),
		fut,
	)
	.await
	.factor_first()
	.0
}
