use anyhow::{Context, Result, anyhow};
use log::{debug, info, trace, warn};
use tokio::{sync::mpsc, task::JoinSet};
use usb_gadget::{
	Class, Config, Gadget, Id, RegGadget, Strings, UsbVersion, default_udc,
	function::custom::{
		CtrlReceiver, CtrlSender, Custom, CustomBuilder, CustomDesc, Endpoint, EndpointDirection,
		EndpointSender, Event, Interface, TransferType,
	},
};

use crate::iap2::{
	ControlSession, DeviceProfile, Error, Handshake, Link, LinkParams, Role, policy,
	transport::hid::{HidTransport, output_report_size, zeroed_input_report},
};

const D2H_VENDOR_TYPE: u8 = 0xc0;
const H2D_VENDOR_TYPE: u8 = 0x40;

const APPLE_GET_CAPABILITIES: u8 = 0x53;
const APPLE_USB_ROLE_SWITCH: u8 = 0x51;
/// `wValue` for a role switch into CarPlay (Table 60-4).
const APPLE_CAPABILITY_CARPLAY: u16 = 0x0001;

const D2H_STD_IFACE: u8 = 0x81;
const H2D_CLASS_IFACE: u8 = 0x21;
const D2H_CLASS_IFACE: u8 = 0xa1;

const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
const HID_DESCRIPTOR_TYPE: u8 = 0x21;
const HID_REPORT_DESC_TYPE: u8 = 0x22;
const HID_REQ_GET_REPORT: u8 = 0x01;
const HID_REQ_GET_IDLE: u8 = 0x02;
const HID_REQ_GET_PROTOCOL: u8 = 0x03;
const HID_REQ_SET_REPORT: u8 = 0x09;
const HID_REQ_SET_IDLE: u8 = 0x0a;
/// `wValue` high byte for an Input report (HID 1.11 §7.2.1).
const HID_REPORT_TYPE_INPUT: u8 = 0x01;

/// The `USBTransportIdentifier` we report before the role switch. It only has to be
/// something the accessory can compare against its wireless sessions, of which a wired
/// head unit has none.
const USB_TRANSPORT_IDENTIFIER: &str = "usb0";

/// iAP2-over-HID report descriptor, captured verbatim from a real iPhone
const HID_DESC_DATA: [u8; 7] = [0x11, 0x01, 0x00, 0x01, 0x22, 0xd0, 0x00];
const HID_DESC_FULL: [u8; 9] = [0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0xd0, 0x00];
#[rustfmt::skip]
const HID_REPORT_DESC: [u8; 208] = [
	0x06, 0x00, 0xff,       // Usage Page (Vendor-Defined 0xFF00)
	0x09, 0x01,             // Usage (0x01)
	0xa1, 0x01,             // Collection (Application)
	0x75, 0x08,             //   Report Size (8)
	0x26, 0x80, 0x00,       //   Logical Maximum (128)
	0x15, 0x00,             //   Logical Minimum (0)
	// Input reports (device→host, interrupt IN endpoint)
	0x09, 0x01, 0x85, 0x01, 0x95, 0x05,       0x82, 0x02, 0x01, // ID 1,  Count 5
	0x09, 0x01, 0x85, 0x02, 0x95, 0x09,       0x82, 0x02, 0x01, // ID 2,  Count 9
	0x09, 0x01, 0x85, 0x03, 0x95, 0x0d,       0x82, 0x02, 0x01, // ID 3,  Count 13
	0x09, 0x01, 0x85, 0x04, 0x95, 0x11,       0x82, 0x02, 0x01, // ID 4,  Count 17
	0x09, 0x01, 0x85, 0x05, 0x95, 0x19,       0x82, 0x02, 0x01, // ID 5,  Count 25
	0x09, 0x01, 0x85, 0x06, 0x95, 0x31,       0x82, 0x02, 0x01, // ID 6,  Count 49
	0x09, 0x01, 0x85, 0x07, 0x95, 0x5f,       0x82, 0x02, 0x01, // ID 7,  Count 95
	0x09, 0x01, 0x85, 0x08, 0x95, 0xc1,       0x82, 0x02, 0x01, // ID 8,  Count 193
	0x09, 0x01, 0x85, 0x09, 0x96, 0x01, 0x01, 0x82, 0x02, 0x01, // ID 9,  Count 257
	0x09, 0x01, 0x85, 0x0a, 0x96, 0x81, 0x01, 0x82, 0x02, 0x01, // ID 10, Count 385
	0x09, 0x01, 0x85, 0x0b, 0x96, 0x01, 0x02, 0x82, 0x02, 0x01, // ID 11, Count 513
	0x09, 0x01, 0x85, 0x0c, 0x96, 0xff, 0x02, 0x82, 0x02, 0x01, // ID 12, Count 767
	// Output reports (host→device, EP0 SET_REPORT)
	0x09, 0x01, 0x85, 0x0d, 0x95, 0x05,       0x92, 0x02, 0x01, // ID 13, Count 5
	0x09, 0x01, 0x85, 0x0e, 0x95, 0x09,       0x92, 0x02, 0x01, // ID 14, Count 9
	0x09, 0x01, 0x85, 0x0f, 0x95, 0x0d,       0x92, 0x02, 0x01, // ID 15, Count 13
	0x09, 0x01, 0x85, 0x10, 0x95, 0x11,       0x92, 0x02, 0x01, // ID 16, Count 17
	0x09, 0x01, 0x85, 0x11, 0x95, 0x19,       0x92, 0x02, 0x01, // ID 17, Count 25
	0x09, 0x01, 0x85, 0x12, 0x95, 0x31,       0x92, 0x02, 0x01, // ID 18, Count 49
	0x09, 0x01, 0x85, 0x13, 0x95, 0x5f,       0x92, 0x02, 0x01, // ID 19, Count 95
	0x09, 0x01, 0x85, 0x14, 0x95, 0xc1,       0x92, 0x02, 0x01, // ID 20, Count 193
	0x09, 0x01, 0x85, 0x15, 0x95, 0xff,       0x92, 0x02, 0x01, // ID 21, Count 255
	0xc0,                   // End Collection
];

pub fn udc_name() -> Result<String> {
	let udc = default_udc().context("failed to get UDC")?;
	udc.name()
		.to_os_string()
		.into_string()
		.map_err(|x| anyhow!("invalid udc name {x:?}"))
}

fn custom_builder() -> CustomBuilder {
	let mut builder = Custom::builder();
	builder.all_ctrl_recipient = true;
	builder
}

pub fn iphone_gadget(sn: &str) -> (Gadget, Vec<Custom>, EndpointSender) {
	let mut customs = Vec::new();

	let (_ep1_rx, ep1_dir) = EndpointDirection::host_to_device();
	let (_ep2_tx, ep2_dir) = EndpointDirection::device_to_host();
	let (custom1, handle1) = custom_builder()
		.with_interface(
			Interface::new(Class::new(0x06, 0x01, 0x01), "PTP")
				.with_endpoint(Endpoint::bulk(ep1_dir))
				.with_endpoint(Endpoint::bulk(ep2_dir)),
		)
		.build();
	customs.push(custom1);
	let mut config1 = Config::new("PTP").with_function(handle1);
	config1.max_power = 500;
	config1.self_powered = true;

	let (hid_tx, hid_dir) = EndpointDirection::device_to_host();
	let mut hid_ep = Endpoint::custom(hid_dir, TransferType::Interrupt);
	hid_ep.max_packet_size_hs = 64;
	hid_ep.interval = 1;
	let (custom2, handle2) = custom_builder()
		.with_interface(
			Interface::new(Class::new(0x03, 0x00, 0x00), "iAP HID")
				.with_custom_desc(CustomDesc::new(HID_DESCRIPTOR_TYPE, HID_DESC_DATA.to_vec()))
				.with_endpoint(hid_ep),
		)
		.build();
	customs.push(custom2);
	let mut config2 = Config::new("iPod USB Interface").with_function(handle2);
	config2.max_power = 500;
	config2.self_powered = true;

	let (_ep4_rx, ep4_dir) = EndpointDirection::host_to_device();
	let (_ep5_tx, ep5_dir) = EndpointDirection::device_to_host();
	let (_ep6_rx, ep6_dir) = EndpointDirection::host_to_device();
	let (_ep7_tx, ep7_dir) = EndpointDirection::device_to_host();
	let (custom3, handle3) = custom_builder()
		.with_interface(
			Interface::new(Class::new(0x06, 0x01, 0x01), "PTP")
				.with_endpoint(Endpoint::bulk(ep4_dir))
				.with_endpoint(Endpoint::bulk(ep5_dir)),
		)
		.with_interface(
			Interface::new(Class::new(0xFF, 0xFE, 0x02), "Apple USB Multiplexor")
				.with_endpoint(Endpoint::bulk(ep6_dir))
				.with_endpoint(Endpoint::bulk(ep7_dir)),
		)
		.build();
	customs.push(custom3);
	let mut config3 = Config::new("PTP + Apple Mobile Device").with_function(handle3);
	config3.max_power = 500;
	config3.self_powered = true;

	let (_ep8_rx, ep8_dir) = EndpointDirection::host_to_device();
	let (_ep9_tx, ep9_dir) = EndpointDirection::device_to_host();
	let (_ep10_rx, ep10_dir) = EndpointDirection::host_to_device();
	let (_ep11_tx, ep11_dir) = EndpointDirection::device_to_host();
	let (_ep12_rx, ep12_dir) = EndpointDirection::host_to_device();
	let (_ep13_tx, ep13_dir) = EndpointDirection::device_to_host();
	let (custom4, handle4) = custom_builder()
		.with_interface(
			Interface::new(Class::new(0x06, 0x01, 0x01), "PTP")
				.with_endpoint(Endpoint::bulk(ep8_dir))
				.with_endpoint(Endpoint::bulk(ep9_dir)),
		)
		.with_interface(
			Interface::new(Class::new(0xFF, 0xFE, 0x02), "Apple USB Multiplexor")
				.with_endpoint(Endpoint::bulk(ep10_dir))
				.with_endpoint(Endpoint::bulk(ep11_dir)),
		)
		.with_interface(
			Interface::new(Class::new(0xFF, 0xFD, 0x01), "AppleUSBEthernet")
				.with_endpoint(Endpoint::bulk(ep12_dir))
				.with_endpoint(Endpoint::bulk(ep13_dir)),
		)
		.build();
	customs.push(custom4);
	let mut config4 =
		Config::new("PTP + Apple Mobile Device + Apple USB Ethernet").with_function(handle4);
	config4.max_power = 500;
	config4.self_powered = true;

	let mut gadget = Gadget::new(
		Class::new(0x00, 0x00, 0x00),
		Id::new(0x05ac, 0x12a8),
		Strings::new("Apple Inc.", "iPhone", sn),
	)
	.with_config(config1)
	.with_config(config2)
	.with_config(config3)
	.with_config(config4);

	gadget.device_release = 0x1002;
	gadget.usb_version = UsbVersion::V20;
	gadget.max_packet_size0 = 64;

	(gadget, customs, hid_tx)
}

pub struct GadgetGuard(RegGadget);
impl GadgetGuard {
	pub fn unbind(&self) -> Result<(), std::io::Error> {
		self.0.bind(None)
	}

	pub fn remove(self) -> Result<(), std::io::Error> {
		self.0.remove()
	}
}

pub async fn run_gadget(sn: &str) -> Result<(GadgetGuard, Result<bool>)> {
	usb_gadget::remove_all().context("failed to remove usb gadgets")?;

	let udc = default_udc().context("failed to get default udc")?;
	let (gadget, customs, mut hid_tx) = iphone_gadget(sn);
	let reg = gadget.bind(&udc).context("failed to bind to UDC")?;

	debug!("gadget bound to {:?}", udc.name());

	let mut set = JoinSet::new();

	let (report_tx, report_rx) = mpsc::unbounded_channel::<Vec<u8>>();
	let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();

	set.spawn(async move {
		while let Some(report) = input_rx.recv().await {
			hid_tx
				.send_async(report.into())
				.await
				.context("failed to send HID input report")?;
		}
		debug!("HID input writer ended");
		anyhow::Ok(false)
	});

	set.spawn(async move {
		let transport = HidTransport::new(report_rx, input_tx);
		let link = Link::new(transport, Role::Device, LinkParams::usb_recommended());
		let mut session = ControlSession::new(link);
		let mut handshake = Handshake::new(DeviceProfile {
			usb_transport_identifier: Some(USB_TRANSPORT_IDENTIFIER.into()),
			..DeviceProfile::default()
		});

		match policy::drive(&mut session, &mut handshake, |h, params| {
			info!("iAP2 ready over HID: {params:?}");
			if let Some(iface) = h.carplay_interface() {
				info!("accessory nominated CarPlay interface {iface}");
			}
		})
		.await
		{
			Err(Error::Closed) => debug!("iAP2 over HID ended with the transport"),
			Err(err) => warn!("iAP2 over HID failed: {err}"),
			Ok(()) => {}
		}
		anyhow::Ok(false)
	});

	for (idx, mut custom) in customs.into_iter().enumerate() {
		let config = idx + 1;
		let report_tx = report_tx.clone();
		set.spawn(async move {
			'outer: loop {
				custom.wait_event().await?;
				while let Some(event) = custom.try_event()? {
					match event {
						Event::SetupDeviceToHost(req) => on_setup_in(config, req)?,
						Event::SetupHostToDevice(req) => {
							if on_setup_out(config, req, &report_tx)? {
								break 'outer anyhow::Ok(true);
							}
						}
						Event::Enable => debug!("gadget enabled: car selected config {config}"),
						Event::Disable => debug!("config {config} disabled"),
						Event::Suspend => debug!("config {config} suspended"),
						Event::Resume => debug!("config {config} resumed"),
						other => debug!("config {config} event {other:?}"),
					}
				}
			}
		});
	}
	drop(report_tx);

	let ret = match set
		.join_next()
		.await
		.context("no task found?")?
		.context("failed to join task")?
	{
		Ok(true) => Ok(true),
		Ok(false) => Err(anyhow!("gadget task ended before the role switch")),
		Err(err) => Err(err),
	};

	Ok((GadgetGuard(reg), ret))
}

/// The setup-packet fields we branch on, copied out so the request itself stays movable
/// (answering it consumes the [`CtrlSender`] / [`CtrlReceiver`]).
#[derive(Clone, Copy, Debug)]
struct Setup {
	request_type: u8,
	request: u8,
	value: u16,
}

impl Setup {
	fn of(req: &usb_gadget::function::custom::CtrlReq) -> Self {
		Self {
			request_type: req.request_type,
			request: req.request,
			value: req.value,
		}
	}
}

/// Answer a device-to-host control request.
///
/// Every path must either `send` or `halt`: dropping a [`CtrlSender`] stalls EP0, which is
/// how twelve unanswered `GET_REPORT`s used to turn into twelve silent EP0 halts.
fn on_setup_in(config: usize, req: CtrlSender<'_>) -> Result<()> {
	trace!("config {config} data request: {:?}", req.ctrl_req());
	let ctrl = Setup::of(req.ctrl_req());

	match (ctrl.request_type, ctrl.request) {
		(D2H_VENDOR_TYPE, APPLE_GET_CAPABILITIES) if req.len() == 4 => {
			debug!("gadget received apple get supported capabilities");
			// Little-endian wValue bitfield then wIndex (Tables 60-3, 60-4).
			req.send(&[0x01, 0x00, 0x00, 0x00])
				.context("failed to send carplay capability back")?;
		}
		(D2H_STD_IFACE, USB_REQ_GET_DESCRIPTOR) => {
			let desc_type = (ctrl.value >> 8) as u8;
			let want = req.len();
			match desc_type {
				HID_REPORT_DESC_TYPE => {
					let n = want.min(HID_REPORT_DESC.len());
					debug!("config {config} sending HID report descriptor ({n} bytes)");
					req.send(&HID_REPORT_DESC[..n])
						.context("failed to send HID report descriptor")?;
				}
				HID_DESCRIPTOR_TYPE => {
					let n = want.min(HID_DESC_FULL.len());
					debug!("config {config} sending HID descriptor");
					req.send(&HID_DESC_FULL[..n])
						.context("failed to send HID descriptor")?;
				}
				other => {
					trace!("config {config} stalling GET_DESCRIPTOR 0x{other:02x}");
					req.halt().context("failed to stall GET_DESCRIPTOR")?;
				}
			}
		}
		(D2H_CLASS_IFACE, HID_REQ_GET_REPORT) => {
			// The car polls every Input report id during enumeration. A real iPhone
			// answers; we have nothing queued, and an all-zero report is a well-formed
			// empty report set (LCB 0, no link packet), so zeros are the honest reply.
			// The data phase carries no report-id byte -- it is in wValue, exactly as for
			// SET_REPORT.
			let report_type = (ctrl.value >> 8) as u8;
			let report_id = (ctrl.value & 0xff) as u8;
			match zeroed_input_report(report_id, req.len())
				.filter(|_| report_type == HID_REPORT_TYPE_INPUT)
			{
				Some(data) => {
					debug!(
						"config {config} HID GET_REPORT id={report_id} -> {} zero bytes",
						data.len()
					);
					req.send(&data).context("failed to send HID input report")?;
				}
				None => {
					trace!(
						"config {config} stalling HID GET_REPORT type={report_type} id={report_id}"
					);
					req.halt().context("failed to stall HID GET_REPORT")?;
				}
			}
		}
		(D2H_CLASS_IFACE, HID_REQ_GET_IDLE | HID_REQ_GET_PROTOCOL) => {
			trace!("config {config} HID class request 0x{:02x}", ctrl.request);
			req.send(&[0x00])
				.context("failed to answer HID class request")?;
		}
		_ => {
			trace!("config {config} stalling unhandled data request: {ctrl:?}");
			req.halt().context("failed to stall control request")?;
		}
	}
	Ok(())
}

/// Answer a host-to-device control request. Returns `true` when the car asked for the USB
/// role switch, which ends the gadget phase.
fn on_setup_out(
	config: usize,
	req: CtrlReceiver<'_>,
	reports: &mpsc::UnboundedSender<Vec<u8>>,
) -> Result<bool> {
	trace!("config {config} data sent: {:?}", req.ctrl_req());
	let ctrl = Setup::of(req.ctrl_req());

	match (ctrl.request_type, ctrl.request) {
		(H2D_VENDOR_TYPE, APPLE_USB_ROLE_SWITCH) if req.is_empty() => {
			if ctrl.value != APPLE_CAPABILITY_CARPLAY {
				// Table 60-5 carries the capability being switched into in wValue. Anything
				// else is not a CarPlay switch, so note it rather than silently obeying.
				warn!(
					"role switch requested for wValue {:#06x}, not CarPlay",
					ctrl.value
				);
			}
			debug!("gadget received apple usb role switch");
			req.recv_all()
				.context("failed to ack role switch request")?;
			return Ok(true);
		}
		(H2D_VENDOR_TYPE, other) => {
			debug!("acking apple vendor request 0x{other:02x}");
			req.recv_all()
				.context("failed to ack apple vendor request")?;
		}
		(H2D_CLASS_IFACE, HID_REQ_SET_REPORT) => {
			let report_id = (ctrl.value & 0xff) as u8;
			let data = req.recv_all().context("failed to recv HID SET_REPORT")?;
			debug!(
				"config {config} HID SET_REPORT id={report_id} ({} bytes)",
				data.len()
			);
			if let Some(expected) = output_report_size(report_id)
				&& data.len() != expected
			{
				// Not fatal: the LCB says how much of the report is real. Worth knowing,
				// because it means the car is not using the descriptor we published.
				warn!(
					"HID SET_REPORT id={report_id} carried {} bytes, descriptor declares {expected}",
					data.len()
				);
			}
			reports
				.send(data)
				.context("failed to send to the iAP2 task")?;
		}
		(H2D_CLASS_IFACE, HID_REQ_SET_IDLE) => {
			req.recv_all().context("failed to ack HID SET_IDLE")?;
			trace!("config {config} HID SET_IDLE");
		}
		(H2D_CLASS_IFACE, other) => {
			trace!("config {config} unhandled HID class request 0x{other:02x}");
			req.halt().context("failed to stall HID class request")?;
		}
		_ => {
			trace!("config {config} stalling unhandled request: {ctrl:?}");
			req.halt().context("failed to stall control request")?;
		}
	}
	Ok(false)
}
