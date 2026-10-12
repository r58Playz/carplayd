pub mod dwc2;
pub mod head_unit;
pub mod ncm;

use std::{collections::HashMap, time::Instant};

use anyhow::{Context, Result, ensure};
use log::{info, warn};
use tokio::{
	select,
	sync::{mpsc, watch},
};

use crate::{
	carplay::{
		CARPLAY_DEVICE_NAME,
		input::{HidParser, InputDevice, InputEvent, InputReport, KeyMerger},
		usb::head_unit::HeadUnitUsb,
	},
	iap2::{
		ControlSession, DeviceProfile, Handshake, Link, LinkParams, MAX_PACKET_LEN, Role,
		msg::{Message, ids},
		policy::{self, Policy, default_reply},
		transport::bulk::{self, BulkTransport},
	},
	pairing::identity::PairIdentity,
	provider::CarplayProviderConnectionProxy,
};

struct UsbPolicy {
	hid: HashMap<u16, HidParser>,
	keys: KeyMerger<u16>,
	tx: mpsc::UnboundedSender<(InputDevice, InputReport)>,
}
impl UsbPolicy {
	fn new(tx: mpsc::UnboundedSender<(InputDevice, InputReport)>) -> Self {
		Self {
			hid: HashMap::new(),
			keys: KeyMerger::default(),
			tx,
		}
	}

	fn send(&self, time: Instant, events: Vec<InputEvent>) -> Result<()> {
		if !events.is_empty() {
			self.tx
				.send((InputDevice::IapRemote, InputReport { time, events }))
				.context("failed to send merged iAP remote report")?;
		}
		Ok(())
	}
}

impl Policy for UsbPolicy {
	fn on_message(&mut self, msg: &Message, cx: policy::Context<'_>) -> Vec<Message> {
		match msg.id {
			ids::ID_START_HID => {
				if let Err(err) = (|| {
					let id = msg
						.get(0)
						.context("no id")?
						.as_u16()
						.context("invalid component ID")?;

					let desc = &msg.get(4).context("no report descriptor")?.data;
					let hid = HidParser::new(desc).context("failed to parse report descriptor")?;

					ensure!(!self.hid.contains_key(&id), "duplicate ID_START_HID");

					self.hid.insert(id, hid);

					anyhow::Ok(())
				})() {
					warn!("failed to process ID_START_HID {err:?}");
				}
				Vec::new()
			}
			ids::ID_ACCESSORY_HID_REPORT => {
				if let Err(err) = (|| {
					let id = msg
						.get(0)
						.context("no id")?
						.as_u16()
						.context("invalid component ID")?;

					let report_bytes = &msg.get(1).context("no HID report")?.data;
					let report = self
						.hid
						.get_mut(&id)
						.context("ID_ACCESSORY_HID_REPORT without ID_START_HID")?
						.parse(Instant::now(), report_bytes)
						.context("failed to parse hid report")?;

					// R29 §57.1 permits only media playback controls over iAP2
					// during CarPlay. Touch, knob and Siri input use the network.
					let changes = report.events.into_iter().filter_map(|event| match event {
						InputEvent::Key { key, pressed }
							if InputDevice::IapRemote.supported_keys().contains(&key) =>
						{
							Some((key, pressed))
						}
						_ => None,
					});
					let events = self.keys.apply(id, changes);
					self.send(report.time, events)?;

					anyhow::Ok(())
				})() {
					warn!("failed to process ID_ACCESSORY_HID_REPORT {err:?}");
				}
				Vec::new()
			}
			ids::ID_STOP_HID => {
				if let Err(err) = (|| {
					let id = msg
						.get(0)
						.context("no id")?
						.as_u16()
						.context("invalid component ID")?;

					self.hid
						.remove(&id)
						.context("ID_STOP_HID without ID_START_HID")?;
					let events = self.keys.remove(&id);
					self.send(Instant::now(), events)?;

					anyhow::Ok(())
				})() {
					warn!("failed to process ID_STOP_HID {err:?}");
				}
				Vec::new()
			}
			_ => default_reply(msg, cx),
		}
	}

	fn on_reset(&mut self) {
		self.hid.clear();
		let events = self.keys.flush();
		if let Err(err) = self.send(Instant::now(), events) {
			warn!("failed to release iAP remote keys {err:?}");
		}
	}
}

pub struct CarplayUsbSession {
	session: ControlSession<BulkTransport>,
	handshake: Handshake<UsbPolicy>,
	pub params: LinkParams,

	rx: mpsc::UnboundedReceiver<(InputDevice, InputReport)>,
	provider_rx: watch::Receiver<Option<CarplayProviderConnectionProxy>>,
}

impl CarplayUsbSession {
	pub async fn new(
		identity: &PairIdentity,
		head_unit: &HeadUnitUsb,
		provider_rx: watch::Receiver<Option<CarplayProviderConnectionProxy>>,
	) -> Result<Self> {
		let (tx, rx) = mpsc::unbounded_channel();

		let transport = bulk::open(&head_unit.device, head_unit.iap2_interface, MAX_PACKET_LEN)
			.await
			.context("failed to open the iAP2 bulk transport")?;

		let link = Link::new(transport, Role::Device, LinkParams::usb_recommended());
		let mut session = ControlSession::new(link);
		let mut handshake = Handshake::with_policy(
			DeviceProfile {
				name: CARPLAY_DEVICE_NAME.to_string(),
				device_uuid: identity.pairing_id().to_string(),
				..Default::default()
			},
			UsbPolicy::new(tx),
		);

		let params = policy::drive_until_ready(&mut session, &mut handshake)
			.await
			.context("iAP2 handshake failed")?;

		info!("iAP2 control session ready over {params:?}");

		let control = head_unit.ncm_control_interface;
		let data = head_unit.ncm_data_interface;
		match handshake.carplay_interface() {
			// The nominated number must be the NCM control interface (Table 30-1).
			Some(nominated) if nominated == control => {
				info!("accessory nominated {nominated} (matches control), data {data}")
			}
			Some(nominated) => warn!(
				"accessory nominated {nominated} but its NCM interfaces are control {control} / data {data}",
			),
			None => warn!("accessory did not nominate a NCM interface"),
		}

		Ok(Self {
			session,
			handshake,
			params,

			rx,
			provider_rx,
		})
	}

	pub async fn run(mut self) -> Result<()> {
		let result = {
			let provider_task = async {
				let provider = self
					.provider_rx
					.wait_for(Option::is_some)
					.await
					.context("failed to wait for provider")?
					.clone()
					.unwrap();

				while let Some((device, report)) = self.rx.recv().await {
					provider
						.process_input(device, report)
						.await
						.context("failed to process iap remote report")?;
				}

				anyhow::Ok(())
			};
			select! {
				x = provider_task => x.context("provider forwarder ended"),
				x = policy::drive(&mut self.session, &mut self.handshake) => x.context("iAP2 session ended")
			}
		};
		// A reset can win the select before the forwarder reads the queued
		// releases. Drain those reports before ending this session.
		self.handshake.on_reset();
		let provider = self.provider_rx.borrow().clone();
		if let Some(provider) = provider {
			while let Ok((device, report)) = self.rx.try_recv() {
				if let Err(err) = provider.process_input(device, report).await {
					warn!("failed to forward final iAP remote report {err:?}");
					break;
				}
			}
		}
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{carplay::input::InputKey, iap2::msg::Param};

	const PLAY: &[u8] = &[
		0x05, 0x0c, 0x09, 1, 0xa1, 1, 0x09, 0xb0, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0x81, 2,
		0x75, 7, 0x81, 1, 0xc0,
	];

	fn message(policy: &mut UsbPolicy, msg: Message) {
		let profile = DeviceProfile::default();
		policy.on_message(
			&msg,
			policy::Context {
				profile: &profile,
				params: None,
				identification: None,
			},
		);
	}

	fn start(policy: &mut UsbPolicy, id: u16, descriptor: &[u8]) {
		message(
			policy,
			Message::new(ids::ID_START_HID)
				.param(Param::u16_(0, id))
				.param(Param::u16_(1, 1))
				.param(Param::u16_(2, 1))
				.param(Param::blob(4, descriptor.to_vec())),
		);
	}

	fn report(policy: &mut UsbPolicy, id: u16, bytes: &[u8]) {
		message(
			policy,
			Message::new(ids::ID_ACCESSORY_HID_REPORT)
				.param(Param::u16_(0, id))
				.param(Param::blob(1, bytes.to_vec())),
		);
	}

	fn stop(policy: &mut UsbPolicy, id: u16) {
		message(
			policy,
			Message::new(ids::ID_STOP_HID).param(Param::u16_(0, id)),
		);
	}

	fn expect_key(rx: &mut mpsc::UnboundedReceiver<(InputDevice, InputReport)>, pressed: bool) {
		let (device, report) = rx.try_recv().unwrap();
		assert!(matches!(device, InputDevice::IapRemote));
		assert_eq!(
			report.events,
			[InputEvent::Key {
				key: InputKey::Play,
				pressed
			}]
		);
	}

	#[test]
	fn late_components_merge_keys_and_stop_preserves_other_owners() {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(tx);
		start(&mut policy, 10, PLAY);
		report(&mut policy, 10, &[1]);
		expect_key(&mut rx, true);
		// A late component reuses the same unnumbered layout independently.
		start(&mut policy, 20, PLAY);
		report(&mut policy, 20, &[1]);
		assert!(rx.try_recv().is_err());
		stop(&mut policy, 10);
		assert!(rx.try_recv().is_err());
		report(&mut policy, 20, &[0]);
		expect_key(&mut rx, false);
		report(&mut policy, 20, &[1]);
		expect_key(&mut rx, true);
		stop(&mut policy, 20);
		expect_key(&mut rx, false);
		assert!(policy.hid.is_empty());
	}

	#[test]
	fn reset_releases_once_and_allows_components_to_start_again() {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(tx);
		start(&mut policy, 10, PLAY);
		start(&mut policy, 20, PLAY);
		report(&mut policy, 10, &[1]);
		report(&mut policy, 20, &[1]);
		expect_key(&mut rx, true);
		policy.on_reset();
		expect_key(&mut rx, false);
		policy.on_reset();
		assert!(rx.try_recv().is_err());
		start(&mut policy, 10, PLAY);
		report(&mut policy, 10, &[1]);
		expect_key(&mut rx, true);
	}

	#[test]
	fn duplicate_start_and_malformed_ids_do_not_replace_held_state() {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(tx);
		start(&mut policy, 10, PLAY);
		report(&mut policy, 10, &[1]);
		expect_key(&mut rx, true);
		start(&mut policy, 10, PLAY);
		message(
			&mut policy,
			Message::new(ids::ID_STOP_HID).param(Param::blob(0, vec![0, 10, 0])),
		);
		assert!(rx.try_recv().is_err());
		report(&mut policy, 10, &[0]);
		expect_key(&mut rx, false);
	}

	#[test]
	fn only_media_playback_keys_are_forwarded_as_iap_remote() {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(tx);
		let descriptor = [
			0x05, 0x0c, 0x09, 1, 0xa1, 1, 0x09, 0xb0, 0x0a, 0x23, 2, 0x15, 0, 0x25, 1, 0x75, 1,
			0x95, 2, 0x81, 2, 0x75, 6, 0x95, 1, 0x81, 1, 0xc0,
		];
		start(&mut policy, 10, &descriptor);
		report(&mut policy, 10, &[3]);
		expect_key(&mut rx, true);
		assert!(rx.try_recv().is_err());
		report(&mut policy, 10, &[2]);
		expect_key(&mut rx, false);
		report(&mut policy, 10, &[0]);
		assert!(rx.try_recv().is_err());
	}
}
