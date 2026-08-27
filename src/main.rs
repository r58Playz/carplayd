use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use log::{LevelFilter, debug, info, warn};
use mdns_sd::{IfKind, ServiceDaemon};
use tokio::{
	fs,
	sync::mpsc,
	task::{JoinSet, spawn_blocking},
	time::timeout,
};

use crate::{
	carplay::{
		CarplaySession,
		ctrl_server::CarplayCtrlServer,
		derive_device_id, format_mac_lower,
		net::head_unit::find_head_unit_server,
		usb::{
			dwc2::{Dwc2, Mode},
			head_unit::{HeadUnitUsb, find_iap2_device},
			ncm::{configure_ncm, get_ncm_device_name},
		},
	},
	gadget::{run_gadget, udc_name},
	pairing::identity::{PairIdentity, PairingStore},
};

mod carplay;
mod gadget;
mod iap2;
mod pairing;
mod provider;

const HEAD_UNIT_ATTEMPTS: u32 = 100;
const HEAD_UNIT_INTERVAL: Duration = Duration::from_millis(10);

async fn force_mode(dwc2: &Arc<Dwc2>, mode: Mode) -> Result<()> {
	let dwc2 = Arc::clone(dwc2);
	spawn_blocking(move || dwc2.force_mode(mode)).await?
}

async fn run_usb_handshake(sn: &str) -> Result<HeadUnitUsb> {
	let controller_name = spawn_blocking(udc_name)
		.await?
		.context("failed to get controller name")?;
	debug!("got controller {controller_name}");

	let dwc2 = Arc::new(Dwc2::open(&controller_name).context("failed to open dwc2 registers")?);

	info!("starting gadget mode");
	force_mode(&dwc2, Mode::Device)
		.await
		.context("failed to force controller into gadget mode")?;

	let (gadget, success) = run_gadget(sn).await.context("failed to run gadget")?;
	if !success? {
		bail!("iAP2 accessory rejected us");
	}

	info!("starting host mode");
	gadget
		.unbind()
		.context("failed to disconnect gadget from the bus")?;
	force_mode(&dwc2, Mode::Host)
		.await
		.context("failed to force controller into host mode")?;
	debug!("host mode reached");

	let head_unit = find_iap2_device(HEAD_UNIT_ATTEMPTS, HEAD_UNIT_INTERVAL)
		.await
		.context("failed to find the carplay head unit")?;
	info!("found head unit");

	gadget.remove().context("failed to remove gadget")?;

	Ok(head_unit)
}

#[tokio::main]
async fn main() -> Result<()> {
	env_logger::builder()
		.filter_level(LevelFilter::Warn)
		.filter_module("carplayd", LevelFilter::Trace)
		.format_timestamp_millis()
		.parse_default_env()
		.init();

	let dir = PathBuf::from(std::env::args().nth(1).context("no config dir provided")?);

	let mut set = JoinSet::new();
	let sn = fs::read_to_string(dir.join("serial_number"))
		.await
		.context("failed to read sn from config dir")?;
	let sn = sn.trim();
	let device_id = derive_device_id(sn);
	info!("device id {}", format_mac_lower(device_id));
	let mut identity = PairIdentity::load_or_generate(&dir.join("identity"))
		.await
		.context("failed to load identity")?;
	let mut pairing = PairingStore::load(&dir.join("pairing"))
		.await
		.context("failed to load pairing store")?;

	let mdns = ServiceDaemon::new().context("failed to create mdns server")?;
	mdns.disable_interface(IfKind::All)
		.context("failed to blacklist all interfaces")?;

	let head_unit_usb = run_usb_handshake(sn)
		.await
		.context("failed to run usb handshake")?;

	let net_iface = get_ncm_device_name(&head_unit_usb)
		.await
		.context("failed to get ncm device name")?;
	let link = configure_ncm(&net_iface)
		.await
		.context("failed to configure ncm device")?;
	debug!(
		"ncm link up: {} scope {} mac {}",
		link.ip,
		link.scope_id,
		format_mac_lower(link.mac)
	);

	mdns.enable_interface(&net_iface)
		.context("failed to enable ncm device for mdns")?;

	let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();
	set.spawn(
		CarplayCtrlServer {
			conn_tx,
			device_id,
			mdns: mdns.clone(),
			ip: link.ip,
			scope_id: link.scope_id,
			port: 6767,
		}
		.run(),
	);

	let target_mac = timeout(Duration::from_millis(500), conn_rx.recv())
		.await
		.ok()
		.flatten();
	set.spawn(async move {
		while let Some(addr) = conn_rx.recv().await {
			warn!("got carplay-ctrl connect addr late {addr:?}");
		}
		Ok(())
	});
	if target_mac.is_none() {
		warn!("carplay-ctrl timed out");
	}

	let head_unit_net = find_head_unit_server(mdns, target_mac, link.scope_id)
		.await
		.context("failed to find head unit server")?;

	let (carplay_session, usb, net) = CarplaySession::new(
		&mut set,
		&mut identity,
		&mut pairing,
		device_id,
		link,
		head_unit_usb,
		head_unit_net,
	)
	.await
	.context("failed to handshake carplay session")?;

	info!("carplay session ready");

	set.spawn(usb.run());
	set.spawn(net.run());
	set.spawn(carplay_session.run());

	while let Some(ret) = set.join_next().await {
		ret.context("failed to join")?
			.context("failed to drive carplay task")?
	}

	Ok(())
}
