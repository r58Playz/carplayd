use anyhow::{Context, Result};
use log::{debug, trace};
use usb_gadget::{
	Class, Config, Gadget, Id, Strings, default_udc,
	function::custom::{Custom, Event, Interface},
};

const D2H_VENDOR_TYPE: u8 = 0xc0;
const H2D_VENDOR_TYPE: u8 = 0x40;

const APPLE_GET_CAPABILITIES: u8 = 0x53;
const APPLE_USB_ROLE_SWITCH: u8 = 0x51;

pub fn run_gadget(sn: &str) -> Result<bool> {
	usb_gadget::remove_all().context("failed to remove usb gadgets")?;

	let (mut custom, handle) = Custom::builder()
		.with_interface(Interface::new(
			Class::vendor_specific(0xff, 0xff),
			"CarPlay",
		))
		.build();

	let udc = default_udc().context("failed to get UDC")?;

	let mut config = Config::new("CarPlay").with_function(handle);
	config.max_power = 1000; // ma
	let reg = Gadget::new(
		Class::vendor_specific(0xff, 0xff),
		Id::new(0x05ac, 0x12a8), // iPhone 8 Plus
		Strings::new("Apple Inc.", "iPhone", sn),
	)
	.with_config(config)
	.bind(&udc)
	.context("failed to bind to UDC")?;

	debug!("gadget bound to {:?}", udc.name());

	loop {
		match custom.event().context("failed to receive gadget event")? {
			Event::SetupHostToDevice(req) => {
				trace!("gadget data sent: {:?}", req);
				if req.ctrl_req().request_type == H2D_VENDOR_TYPE
					&& req.ctrl_req().request == APPLE_USB_ROLE_SWITCH
					&& req.len() == 0
				{
					debug!("gadget received apple usb role switch");
					break Ok(true);
				}
			}
			Event::SetupDeviceToHost(req) => {
				trace!("gadget data request: {:?}", req);
				if req.ctrl_req().request_type == D2H_VENDOR_TYPE
					&& req.ctrl_req().request == APPLE_GET_CAPABILITIES
					&& req.len() == 4
				{
					debug!("gadget received apple get supported capabilities");

					req.send(&[0x01, 0x00, 0x00, 0x00])
						.context("failed to send carplay capability back")?; // carplay
				}
			}
			Event::Enable => {
				debug!("gadget enabled");
			}
			Event::Disable => {
				debug!("gadget disabled");
				break Ok(false);
			}
			Event::Suspend => {
				debug!("gadget suspended");
			}
			Event::Resume => {
				debug!("gadget resumed");
			}
			_ => {}
		}
	}
}
