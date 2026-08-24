use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use log::{LevelFilter, debug, info, warn};
use mdns_sd::{IfKind, ServiceDaemon};
use tokio::{
	sync::mpsc,
	task::{JoinSet, spawn_blocking},
	time::timeout,
};

use crate::{
	carplay::{
		ctrl_server::CarplayCtrlServer,
		head_unit_discover::find_head_unit_server,
		usb::{
			dwc2::{Dwc2, Mode},
			head_unit::{HeadUnitUsb, find_iap2_device},
			ncm::{configure_ncm, get_ncm_device_name},
		},
	},
	gadget::{run_gadget, udc_name},
	iap2::{
		ControlSession, DefaultPolicy, DeviceProfile, Handshake, Link, LinkParams, MAX_PACKET_LEN,
		Role, policy, transport::bulk,
	},
};

mod carplay;
mod gadget;
mod iap2;
mod pairing;

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

async fn run_usb_iap2(head_unit: HeadUnitUsb) -> Result<()> {
	let transport = bulk::open(&head_unit.device, head_unit.iap2_interface, MAX_PACKET_LEN)
		.await
		.context("failed to open the iAP2 bulk transport")?;

	let link = Link::new(transport, Role::Device, LinkParams::usb_recommended());
	let mut session = ControlSession::new(link);
	let mut handshake = Handshake::with_policy(DeviceProfile::default(), DefaultPolicy);

	let result = policy::drive(&mut session, &mut handshake, |h, params| {
		info!("iAP2 control session ready over {params:?}");
		let control = head_unit.ncm_control_interface;
		let data = head_unit.ncm_data_interface;
		match h.carplay_interface() {
			// The nominated number must be the NCM control interface (Table 30-1).
			Some(nominated) if nominated == control => {
				info!("accessory nominated {nominated} (matches control), data {data}")
			}
			Some(nominated) => warn!(
				"accessory nominated {nominated} but its NCM interfaces are control {control} / data {data}",
			),
			None => warn!("accessory did not nominate a NCM interface"),
		}
	})
	.await;

	Err(result
		.expect_err("policy::drive only returns on failure")
		.into())
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

	let mut set = JoinSet::new();

	let mdns = ServiceDaemon::new().context("failed to create mdns server")?;
	mdns.disable_interface(IfKind::All)
		.context("failed to blacklist all interfaces")?;

	let head_unit = run_usb_handshake(&sn)
		.await
		.context("failed to run usb handshake")?;

	let net_iface = get_ncm_device_name(&head_unit)
		.await
		.context("failed to get ncm device name")?;
	let (ip, scope_id) = configure_ncm(&net_iface)
		.await
		.context("failed to configure ncm device")?;

	mdns.enable_interface(&net_iface)
		.context("failed to enable ncm device for mdns")?;

	let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();
	set.spawn(
		CarplayCtrlServer {
			conn_tx,
			mdns: mdns.clone(),
			ip,
			scope_id,
			port: 6767,
		}
		.run(),
	);
	set.spawn(run_usb_iap2(head_unit.clone()));

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

	let head_unit_server = find_head_unit_server(mdns, target_mac, scope_id)
		.await
		.context("failed to find head unit server")?;

	set.join_all().await;

	Ok(())
}
