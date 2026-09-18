use std::{
	net::{IpAddr, Ipv6Addr},
	pin::pin,
};

use anyhow::{Context, Result, bail};
use futures::{FutureExt, StreamExt, future::select};
use log::debug;
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

use crate::carplay::{MacAddr, usb::head_unit::HeadUnitUsb};

#[derive(Clone, Copy, Debug)]
pub struct NcmLink {
	pub ip: Ipv6Addr,
	pub scope_id: u32,
	pub mac: MacAddr,
}

pub async fn get_ncm_device_name(head_unit: &HeadUnitUsb) -> Result<String> {
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

pub async fn configure_ncm(device: &str) -> Result<NcmLink> {
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

		debug!(
			"ncm link {} index {} flags {:?}",
			device, link.header.index, link.header.flags
		);

		let mac = link
			.attributes
			.iter()
			.find_map(|x| {
				if let LinkAttribute::Address(addr) = x {
					MacAddr::try_from(addr.as_slice()).ok()
				} else {
					None
				}
			})
			.context("no hardware address on the ncm link")?;

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
		dev_conf.accept_dad = 0;
		dev_conf.dad_transmits = 0;
		dev_conf.optimistic_dad = 1;
		dev_conf.use_optimistic = 1;

		let attr = LinkAttribute::AfSpecUnspec(vec![AfSpecUnspec::Inet6(vec![
			AfSpecInet6::AddrGenMode(In6AddrGenMode::Eui64),
			AfSpecInet6::DevConf(dev_conf),
		])]);

		// Two calls, not one. Bringing the link up is what makes the kernel derive the
		// link-local address, and it does that with whatever devconf is in force at that
		// moment — so the DAD settings have to already be committed. Folding `.up()` into
		// the same message races the address against the configuration that governs it.
		handle
			.link()
			.set(
				LinkUnspec::new_with_index(link.header.index)
					.append_extra_attribute(attr)
					.build(),
			)
			.execute()
			.await
			.context("failed to set link ipv6 attrs")?;
		debug!("ncm ipv6 attrs committed");

		handle
			.link()
			.set(LinkUnspec::new_with_index(link.header.index).up().build())
			.execute()
			.await
			.context("failed to bring link up")?;
		debug!("ncm link set up, waiting for a link-local address");

		while let Some((msg, _)) = events.next().await {
			if let NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewAddress(addr_msg)) =
				msg.payload
			{
				if addr_msg.header.index != link.header.index {
					continue;
				}

				debug!(
					"ncm address event: {:?}",
					addr_msg
						.attributes
						.iter()
						.filter(|x| matches!(
							x,
							AddressAttribute::Address(_) | AddressAttribute::Flags(_)
						))
						.collect::<Vec<_>>()
				);

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

				return Ok(NcmLink {
					ip: v6,
					scope_id: link.header.index,
					mac,
				});
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
