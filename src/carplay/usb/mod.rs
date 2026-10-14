pub mod dwc2;
pub mod head_unit;
pub mod ncm;

use std::{
	collections::{HashMap, HashSet},
	time::Instant,
};

use anyhow::{Context, Result, anyhow, ensure};
use log::{error, info, warn};
use tokio::{
	select,
	sync::{mpsc, watch},
};

use crate::{
	carplay::{
		CARPLAY_DEVICE_NAME,
		input::{HidParser, InputDevice, InputEvent, InputReport, KeyMerger},
		media::NowPlaying,
		usb::head_unit::HeadUnitUsb,
	},
	iap2::{
		ControlSession, DeviceProfile, Handshake, Link, LinkParams, MAX_PACKET_LEN, Role,
		msg::{Message, Param, ids},
		policy::{self, Policy, default_reply},
		transport::bulk::{self, BulkTransport},
	},
	pairing::identity::PairIdentity,
	provider::{CarplayProviderConnectionProxy, CarplayProviderMedia, MediaEvent},
};

struct UsbPolicy {
	hid: HashMap<u16, HidParser>,
	keys: KeyMerger<u16>,
	hid_tx: mpsc::UnboundedSender<(InputDevice, InputReport)>,

	media_rx: mpsc::UnboundedReceiver<Box<dyn CarplayProviderMedia>>,
	media: Option<Box<dyn CarplayProviderMedia>>,
	/// params to send for both groups
	send_now_playing: Option<[HashSet<u16>; 2]>,
	last_now_playing: NowPlaying,
}
impl UsbPolicy {
	fn new(
		hid_tx: mpsc::UnboundedSender<(InputDevice, InputReport)>,
		media_rx: mpsc::UnboundedReceiver<Box<dyn CarplayProviderMedia>>,
	) -> Self {
		Self {
			hid: HashMap::new(),
			keys: KeyMerger::default(),
			hid_tx,

			media_rx,
			media: None,
			send_now_playing: None,
			last_now_playing: Default::default(),
		}
	}

	fn send_input(&self, time: Instant, events: Vec<InputEvent>) -> Result<()> {
		if !events.is_empty() {
			self.hid_tx
				.send((InputDevice::IapRemote, InputReport { time, events }))
				.context("failed to send merged iAP remote report")?;
		}
		Ok(())
	}

	fn now_playing_update(&mut self, update: NowPlaying) -> Vec<Message> {
		let Some(requested) = &self.send_now_playing else {
			return Vec::new();
		};
		let mut msg = Message::new(ids::ID_NOW_PLAYING_UPDATE);
		for group in update.encode_diff(&self.last_now_playing) {
			let Some(fields) = requested.get(usize::from(group.id)) else {
				continue;
			};
			let mut members = group.as_group().unwrap();
			members.retain(|member| fields.contains(&member.id));
			if !members.is_empty() {
				msg = msg.param(Param::group(group.id, &members));
			}
		}
		self.last_now_playing = update;
		if msg.params.is_empty() {
			Vec::new()
		} else {
			vec![msg]
		}
	}

	fn initial_now_playing(&mut self) -> Vec<Message> {
		// Every Start needs the complete requested state, including newly requested fields.
		self.last_now_playing = Default::default();
		let Some(media) = &mut self.media else {
			return Vec::new();
		};
		match media.get_now_playing() {
			Ok(Some(update)) => self.now_playing_update(update),
			Ok(None) => self.now_playing_update(NowPlaying::unavailable()),
			Err(err) => {
				warn!("failed to get initial now playing snapshot {err:?}");
				Vec::new()
			}
		}
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
					self.send_input(report.time, events)?;

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
					self.send_input(Instant::now(), events)?;

					anyhow::Ok(())
				})() {
					warn!("failed to process ID_STOP_HID {err:?}");
				}
				Vec::new()
			}
			ids::ID_START_NOW_PLAYING_UPDATES => {
				match (|| {
					let mut requested = [HashSet::new(), HashSet::new()];
					for (id, fields) in requested.iter_mut().enumerate() {
						if let Some(group) = msg.get(id as u16) {
							for member in group.as_group()? {
								// media queue transfers
								if id == 1 && member.id == 17 {
									continue;
								}
								fields.insert(member.id);
							}
						}
					}

					self.send_now_playing.replace(requested);

					anyhow::Ok(self.initial_now_playing())
				})() {
					Ok(replies) => replies,
					Err(err) => {
						warn!("failed to process ID_START_NOW_PLAYING_UPDATES {err:?}");
						Vec::new()
					}
				}
			}
			ids::ID_STOP_NOW_PLAYING_UPDATES => {
				self.send_now_playing.take();
				self.last_now_playing = Default::default();

				Vec::new()
			}
			_ => default_reply(msg, cx),
		}
	}

	fn on_reset(&mut self) {
		self.send_now_playing = None;
		self.last_now_playing = Default::default();
		self.hid.clear();
		let events = self.keys.flush();
		if let Err(err) = self.send_input(Instant::now(), events) {
			warn!("failed to release iAP remote keys {err:?}");
		}
	}

	async fn next_outbound(&mut self) -> Vec<Message> {
		if self.media.is_none() {
			if let Some(media) = self.media_rx.recv().await {
				self.media = Some(media);
				// Start can arrive before the provider. Send its cached snapshot before
				// waiting for changes, so paused media does not need to emit an event.
				if self.send_now_playing.is_some() {
					let initial = self.initial_now_playing();
					if !initial.is_empty() {
						return initial;
					}
				}
			} else {
				error!("failed to recv media");
				std::future::pending::<()>().await;
				unreachable!()
			}
		}
		let media = self.media.as_mut().unwrap();
		match media.next_now_playing_change().await {
			Ok(update) => match update {
				MediaEvent::NowPlayingChanged(update) => {
					self.now_playing_update(update.unwrap_or_else(NowPlaying::unavailable))
				}
			},
			Err(err) => {
				error!("failed to get now playing update {err:?}");
				std::future::pending::<()>().await;
				unreachable!()
			}
		}
	}
}

pub struct CarplayUsbSession {
	session: ControlSession<BulkTransport>,
	handshake: Handshake<UsbPolicy>,
	pub params: LinkParams,

	hid_rx: mpsc::UnboundedReceiver<(InputDevice, InputReport)>,
	media_tx: mpsc::UnboundedSender<Box<dyn CarplayProviderMedia>>,
	provider_rx: watch::Receiver<Option<CarplayProviderConnectionProxy>>,
}

impl CarplayUsbSession {
	pub async fn new(
		identity: &PairIdentity,
		head_unit: &HeadUnitUsb,
		provider_rx: watch::Receiver<Option<CarplayProviderConnectionProxy>>,
	) -> Result<Self> {
		let (hid_tx, hid_rx) = mpsc::unbounded_channel();
		let (media_tx, media_rx) = mpsc::unbounded_channel();

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
			UsbPolicy::new(hid_tx, media_rx),
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

			hid_rx,
			media_tx,
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

				self.media_tx
					.send(
						provider
							.get_media()
							.await
							.context("failed to get media provider")?,
					)
					.map_err(|_| anyhow!("failed to send media provider"))?;

				while let Some((device, report)) = self.hid_rx.recv().await {
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
			while let Ok((device, report)) = self.hid_rx.try_recv() {
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

	fn message(policy: &mut UsbPolicy, msg: Message) -> Vec<Message> {
		let profile = DeviceProfile::default();
		policy.on_message(
			&msg,
			policy::Context {
				profile: &profile,
				params: None,
				identification: None,
			},
		)
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
		let mut policy = UsbPolicy::new(tx, mpsc::unbounded_channel().1);
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
		let mut policy = UsbPolicy::new(tx, mpsc::unbounded_channel().1);
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
		let mut policy = UsbPolicy::new(tx, mpsc::unbounded_channel().1);
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
		let mut policy = UsbPolicy::new(tx, mpsc::unbounded_channel().1);
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

	struct CachedMedia {
		snapshot: NowPlaying,
		updates: mpsc::UnboundedReceiver<Option<NowPlaying>>,
	}

	#[async_trait::async_trait]
	impl CarplayProviderMedia for CachedMedia {
		async fn next_now_playing_change(&mut self) -> Result<MediaEvent> {
			let update = self.updates.recv().await.context("media updates closed")?;
			self.snapshot = update.clone().unwrap_or_else(NowPlaying::unavailable);
			Ok(MediaEvent::NowPlayingChanged(update))
		}

		fn get_now_playing(&mut self) -> Result<Option<NowPlaying>> {
			Ok(Some(self.snapshot.clone()))
		}
	}

	fn paused_snapshot(elapsed_time: u32) -> NowPlaying {
		serde_json::from_value(serde_json::json!({
			"media_item": {"title": "Track", "artist": "Artist"},
			"status": "Paused",
			"elapsed_time": elapsed_time,
			"set_elapsed_time_allowed": false,
		}))
		.unwrap()
	}

	fn subscribe(policy: &mut UsbPolicy, media: &[u16], playback: &[u16]) -> Vec<Message> {
		message(
			policy,
			Message::new(ids::ID_START_NOW_PLAYING_UPDATES)
				.param(Param::group(
					0,
					&media.iter().copied().map(Param::none).collect::<Vec<_>>(),
				))
				.param(Param::group(
					1,
					&playback
						.iter()
						.copied()
						.map(Param::none)
						.collect::<Vec<_>>(),
				)),
		)
	}

	fn expect_initial(replies: &[Message], elapsed_time: u32) {
		assert_eq!(replies.len(), 1);
		let msg = &replies[0];
		assert_eq!(msg.id, ids::ID_NOW_PLAYING_UPDATE);
		let media = msg.get(0).unwrap();
		assert_eq!(media.sub(1).unwrap().unwrap().as_utf8().unwrap(), "Track");
		let playback = msg.get(1).unwrap();
		assert_eq!(playback.sub(0).unwrap().unwrap().as_u8().unwrap(), 2);
		assert_eq!(
			playback.sub(1).unwrap().unwrap().as_u32().unwrap(),
			elapsed_time
		);
		assert!(!playback.sub(13).unwrap().unwrap().as_bool().unwrap());
	}

	#[tokio::test]
	async fn start_sends_cached_snapshot_and_restart_sends_newly_requested_fields() {
		let (hid_tx, _hid_rx) = mpsc::unbounded_channel();
		let (_media_tx, media_rx) = mpsc::unbounded_channel();
		let (updates_tx, updates_rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(hid_tx, media_rx);
		policy.media = Some(Box::new(CachedMedia {
			snapshot: paused_snapshot(12_000),
			updates: updates_rx,
		}));

		// No change event is needed to display a track that was already paused.
		let initial = subscribe(&mut policy, &[1], &[0, 1, 13]);
		expect_initial(&initial, 12_000);
		assert_eq!(initial[0].get(0).unwrap().as_group().unwrap().len(), 1);

		updates_tx.send(Some(paused_snapshot(13_000))).unwrap();
		let delta = tokio::time::timeout(std::time::Duration::from_secs(1), policy.next_outbound())
			.await
			.unwrap();
		assert_eq!(
			delta,
			vec![
				Message::new(ids::ID_NOW_PLAYING_UPDATE)
					.param(Param::group(1, &[Param::u32_(1, 13_000)]))
			]
		);

		let restarted = subscribe(&mut policy, &[1, 12], &[0, 1, 13]);
		expect_initial(&restarted, 13_000);
		assert_eq!(
			restarted[0]
				.get(0)
				.unwrap()
				.sub(12)
				.unwrap()
				.unwrap()
				.as_utf8()
				.unwrap(),
			"Artist"
		);
	}

	#[tokio::test]
	async fn provider_arriving_after_start_sends_cached_snapshot_without_a_change() {
		let (hid_tx, _hid_rx) = mpsc::unbounded_channel();
		let (media_tx, media_rx) = mpsc::unbounded_channel();
		let (_updates_tx, updates_rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(hid_tx, media_rx);
		assert!(subscribe(&mut policy, &[1], &[0, 1, 13]).is_empty());
		assert!(
			media_tx
				.send(Box::new(CachedMedia {
					snapshot: paused_snapshot(12_000),
					updates: updates_rx,
				}))
				.is_ok()
		);

		let initial =
			tokio::time::timeout(std::time::Duration::from_secs(1), policy.next_outbound())
				.await
				.unwrap();
		expect_initial(&initial, 12_000);
	}

	#[tokio::test]
	async fn start_reads_provider_acquired_by_a_cancelled_outbound_wait() {
		let (hid_tx, _hid_rx) = mpsc::unbounded_channel();
		let (media_tx, media_rx) = mpsc::unbounded_channel();
		let (_updates_tx, updates_rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(hid_tx, media_rx);
		assert!(
			media_tx
				.send(Box::new(CachedMedia {
					snapshot: paused_snapshot(12_000),
					updates: updates_rx,
				}))
				.is_ok()
		);

		{
			let mut outbound = Box::pin(policy.next_outbound());
			assert!(futures::poll!(outbound.as_mut()).is_pending());
		}
		expect_initial(&subscribe(&mut policy, &[1], &[0, 1, 13]), 12_000);
	}

	#[tokio::test]
	async fn missing_source_clears_cached_track_and_reset_requires_a_new_subscription() {
		let (hid_tx, _hid_rx) = mpsc::unbounded_channel();
		let (_media_tx, media_rx) = mpsc::unbounded_channel();
		let (updates_tx, updates_rx) = mpsc::unbounded_channel();
		let mut policy = UsbPolicy::new(hid_tx, media_rx);
		policy.media = Some(Box::new(CachedMedia {
			snapshot: paused_snapshot(12_000),
			updates: updates_rx,
		}));
		expect_initial(&subscribe(&mut policy, &[1, 12], &[0, 1, 13]), 12_000);
		updates_tx.send(None).unwrap();
		let cleared =
			tokio::time::timeout(std::time::Duration::from_secs(1), policy.next_outbound())
				.await
				.unwrap();
		assert_eq!(cleared.len(), 1);
		let media = cleared[0].get(0).unwrap();
		assert_eq!(media.sub(1).unwrap().unwrap().as_utf8().unwrap(), "");
		assert_eq!(media.sub(12).unwrap().unwrap().as_utf8().unwrap(), "");
		let playback = cleared[0].get(1).unwrap();
		assert_eq!(playback.sub(0).unwrap().unwrap().as_u8().unwrap(), 0);
		assert_eq!(playback.sub(1).unwrap().unwrap().as_u32().unwrap(), 0);

		policy.on_reset();
		assert!(policy.send_now_playing.is_none());
		assert!(
			policy
				.now_playing_update(paused_snapshot(12_000))
				.is_empty()
		);
	}
}
