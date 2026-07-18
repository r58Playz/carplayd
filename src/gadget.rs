use anyhow::{Context, Result, anyhow};
use log::{debug, trace};
use tokio::{sync::mpsc, task::JoinSet};
use usb_gadget::{
	Class, Config, Gadget, Id, Strings, UsbVersion, default_udc,
	function::custom::{
		Custom, CustomBuilder, CustomDesc, Endpoint, EndpointDirection, EndpointSender, Event,
		Interface, TransferType,
	},
};

use crate::iap2::{Link, msg};

const D2H_VENDOR_TYPE: u8 = 0xc0;
const H2D_VENDOR_TYPE: u8 = 0x40;

const APPLE_GET_CAPABILITIES: u8 = 0x53;
const APPLE_USB_ROLE_SWITCH: u8 = 0x51;

// ── iAP2-over-HID (USB Device Mode) control-request plumbing (spec §60.5.5) ──
// Request-type bytes for the HID class/standard interface requests the car issues
// while the Pi is still the USB *device*.
const D2H_STD_IFACE: u8 = 0x81; // dir in  | standard | interface
const H2D_CLASS_IFACE: u8 = 0x21; // dir out | class    | interface

const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
const HID_DESCRIPTOR_TYPE: u8 = 0x21; // GET_DESCRIPTOR wValue high byte / CustomDesc type
const HID_REPORT_DESC_TYPE: u8 = 0x22; // GET_DESCRIPTOR wValue high byte for the report descriptor
const HID_REQ_SET_REPORT: u8 = 0x09;
const HID_REQ_SET_IDLE: u8 = 0x0a;

/// HID class descriptor payload (the 7 bytes after `bLength`/`bDescriptorType`):
/// bcdHID 1.11, no country code, one report descriptor of 208 bytes. Injected into
/// the interface with [`CustomDesc`] so it lands between the interface and endpoint
/// descriptors, and served verbatim for a `GET_DESCRIPTOR(0x21)`.
const HID_DESC_DATA: [u8; 7] = [0x11, 0x01, 0x00, 0x01, 0x22, 0xd0, 0x00];
/// Full 9-byte HID class descriptor (`bLength`, `bDescriptorType`, then [`HID_DESC_DATA`]).
const HID_DESC_FULL: [u8; 9] = [0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0xd0, 0x00];

/// iAP2-over-HID report descriptor, captured verbatim from a real iPhone (208 bytes,
/// matching `wDescriptorLength 208`). Input reports 1..=12 (5..=767 B payload) carry
/// phone→car iAP data over the interrupt IN endpoint; Output reports 13..=21
/// (5..=255 B) carry car→phone iAP data via EP0 `SET_REPORT`. Each report payload is
/// `[Link Control Byte][iAP packet data]`, zero-padded to the report size (§60.5.6).
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

/// Returns a Gadget that mimics the iPhone USB descriptor dump plus the four
/// `Custom` functions (one per configuration) whose control channels must be
/// driven. The car selects one configuration, and only that configuration's
/// function receives the vendor setup requests — so all four are returned and
/// polled together in `run_gadget`.
pub fn iphone_gadget(sn: &str) -> (Gadget, Vec<Custom>, EndpointSender) {
	let mut customs = Vec::new();

	// ── Config 1: "PTP" ───────────────────────────────────────────────────────
	// Interface 0: Still Image Capture, class 6/1/1, bulk OUT + bulk IN.
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

	// ── Config 2: "iPod USB Interface" — iAP2-over-HID transport ─────────────
	let (hid_tx, hid_dir) = EndpointDirection::device_to_host();
	let mut hid_ep = Endpoint::custom(hid_dir, TransferType::Interrupt);
	hid_ep.max_packet_size_hs = 64; // matches the real iPhone's 0x83 interrupt IN
	hid_ep.interval = 1; // 125 µs at high speed, as the iPhone advertises
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

	// ── Config 3: "PTP + Apple Mobile Device" ────────────────────────────────
	// Interface 0: PTP.  Interface 1: Apple USB Multiplexor (usbmux).
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

	// ── Config 4: "PTP + Apple Mobile Device + Apple USB Ethernet" ───────────
	// Interface 0: PTP.  Interface 1: Apple USB Multiplexor.  Interface 2: NCM.
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

	// ── Gadget ────────────────────────────────────────────────────────────────
	let mut gadget = Gadget::new(
		Class::new(0x00, 0x00, 0x00), // per §60.4 the device class must be 0x00
		Id::new(0x05ac, 0x12a8),
		Strings::new("Apple Inc.", "iPhone", sn),
	)
	.with_config(config1) // bConfigurationValue 1
	.with_config(config2) // bConfigurationValue 2
	.with_config(config3) // bConfigurationValue 3
	.with_config(config4); // bConfigurationValue 4

	gadget.device_release = 0x1002; // bcdDevice 16.02
	gadget.usb_version = UsbVersion::V20; // bcdUSB 2.00
	gadget.max_packet_size0 = 64;

	(gadget, customs, hid_tx)
}

pub async fn run_gadget(sn: &str) -> Result<bool> {
	usb_gadget::remove_all().context("failed to remove usb gadgets")?;

	let udc = default_udc().context("failed to get default udc")?;
	let (gadget, customs, mut hid_tx) = iphone_gadget(sn);
	let reg = gadget.bind(&udc).context("failed to bind to UDC")?;

	debug!("gadget bound to {:?}", udc.name());

	let mut set = JoinSet::new();

	let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

	set.spawn(async move {
		let mut link = Link::new();
		while let Some(bytes) = rx.recv().await {
			let out = link.receive(&bytes);
			for report in out.reports {
				hid_tx
					.send_async(report.into())
					.await
					.context("failed to send HID input report")?;
			}
			if out.established {
				debug!(
					"iAP2 link established (control session {:?})",
					link.control_session()
				);
				for r in link.send_control(msg::REQUEST_AUTH_CERTIFICATE.to_vec()) {
					hid_tx.send_async(r.into()).await?;
				}
			}
			for (session, payload) in out.payloads {
				debug!(
					"iAP2 session {session} payload ({} bytes): {:02x?}",
					payload.len(),
					payload
				);

				let (next, optional_next) = match msg::message_id(&payload) {
					Some(msg::ID_AUTHENTICATION_CERTIFICATE) => {
						(msg::request_auth_challenge_response(&msg::CHALLENGE), None)
					}
					Some(msg::ID_AUTHENTICATION_RESPONSE) => (
						msg::AUTHENTICATION_SUCCEEDED.to_vec(),
						Some(msg::START_IDENTIFICATION.to_vec()),
					),
					Some(msg::ID_IDENTIFICATION_INFORMATION) => (
						msg::IDENTIFICATION_ACCEPTED.to_vec(),
						Some(msg::device_transport_identifier("usb0")),
					),
					_ => continue,
				};

				for r in link.send_control(next) {
					hid_tx.send_async(r.into()).await?;
				}
				if let Some(next) = optional_next {
					for r in link.send_control(next) {
						hid_tx.send_async(r.into()).await?;
					}
				}
			}
		}
		debug!("hid task ended");
		anyhow::Ok(false)
	});

	for (idx, mut custom) in customs.into_iter().enumerate() {
		let config = idx + 1;
		let tx = tx.clone();
		set.spawn(async move {
			'outer: loop {
				custom.wait_event().await?;
				while let Some(event) = custom.try_event()? {
					match event {
						Event::SetupDeviceToHost(req) => {
							trace!("config {config} data request: {:?}", req);
							if req.ctrl_req().request_type == D2H_VENDOR_TYPE
								&& req.ctrl_req().request == APPLE_GET_CAPABILITIES
								&& req.len() == 4
							{
								debug!("gadget received apple get supported capabilities");
								req.send(&[0x01, 0x00, 0x00, 0x00]) // carplay
									.context("failed to send carplay capability back")?;
							} else if req.ctrl_req().request_type == D2H_STD_IFACE
								&& req.ctrl_req().request == USB_REQ_GET_DESCRIPTOR
							{
								let desc_type = (req.ctrl_req().value >> 8) as u8;
								let want = req.len();
								match desc_type {
									HID_REPORT_DESC_TYPE => {
										let n = want.min(HID_REPORT_DESC.len());
										debug!(
											"config {config} sending HID report descriptor ({n} bytes)"
										);
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
										trace!(
											"config {config} unhandled GET_DESCRIPTOR 0x{other:02x}"
										)
									}
								}
							}
						}
						Event::SetupHostToDevice(req) => {
							trace!("config {config} data sent: {:?}", req);
							if req.ctrl_req().request_type == H2D_VENDOR_TYPE
								&& req.ctrl_req().request == APPLE_USB_ROLE_SWITCH
								&& req.is_empty()
							{
								debug!("gadget received apple usb role switch");
								req.recv_all()
									.context("failed to ack role switch request")?;
								break 'outer anyhow::Ok(true);
							} else if req.ctrl_req().request_type == H2D_VENDOR_TYPE {
								debug!(
									"acking apple vendor request 0x{:02x}",
									req.ctrl_req().request
								);
								let _ = req.recv_all();
							} else if req.ctrl_req().request_type == H2D_CLASS_IFACE {
								match req.ctrl_req().request {
									HID_REQ_SET_REPORT => {
										let report_id = (req.ctrl_req().value & 0xff) as u8;
										let data = req
											.recv_all()
											.context("failed to recv HID SET_REPORT")?;
										debug!(
											"config {config} HID SET_REPORT id={report_id} ({} bytes): {:02x?}",
											data.len(),
											data
										);
										tx.send(data).context("failed to send to hid task")?;
									}
									HID_REQ_SET_IDLE => {
										let _ = req.recv_all(); // no data phase; ACK
										trace!("config {config} HID SET_IDLE");
									}
									other => {
										trace!(
											"config {config} unhandled HID class request 0x{other:02x}"
										);
										let _ = req.recv_all();
									}
								}
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

	let ret = match set
		.join_next()
		.await
		.context("no task found?")?
		.context("failed to join task")?
	{
		Ok(true) => Ok(true),
		Ok(false) => Err(anyhow!("hid task failed")),
		Err(err) => Err(err),
	};
	reg.remove().context("failed to remove gadget")?;
	set.abort_all();
	ret
}
