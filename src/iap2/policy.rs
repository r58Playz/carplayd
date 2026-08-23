//! The device half of the iAP2 control session: what to send, and when.
//!
//! We impersonate an iPhone, so we drive authentication and identification (§19.1, §19.2)
//! and the accessory answers. Authentication is *accept-only* by design: the head unit
//! holds the MFi coprocessor and we are nominally the verifier, so we take its certificate
//! and challenge response on trust and send `AuthenticationSucceeded` regardless. That is
//! the whole premise of this project.
//!
//! That mechanical part lives in [`Handshake`], a synchronous state machine — messages
//! in, messages out — so it is testable without a car. Everything above it is a
//! [`Policy`]: which features to answer, and what to report. Every trait method has a
//! default, so the pre-role-switch gadget can use [`DefaultPolicy`] unchanged while the
//! host side supplies an implementation holding whatever state it bridges from.
//!
//! ```ignore
//! struct AndroidAuto {
//!     now_playing: watch::Receiver<NowPlaying>,
//! }
//!
//! impl Policy for AndroidAuto {
//!     fn on_message(&mut self, msg: &Message, cx: Context<'_>) -> Vec<Message> {
//!         match msg.id {
//!             ids::ID_START_NOW_PLAYING_UPDATES => vec![self.now_playing_update()],
//!             // Everything we do not special-case keeps the stock behaviour.
//!             _ => policy::default_reply(msg, cx),
//!         }
//!     }
//!
//!     // Push an update when the Android Auto side changes, unprompted.
//!     async fn next_outbound(&mut self) -> Vec<Message> {
//!         self.now_playing.changed().await.ok();
//!         vec![self.now_playing_update()]
//!     }
//! }
//! ```
//!
//! `drive` wires a [`Handshake`] to a [`ControlSession`].

use std::{
	future::Future,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use log::{debug, info, warn};

use crate::iap2::{
	error::Error,
	msg::{Message, Param, ids},
	packet::Frame,
	session::{ControlEvent, ControlSession},
	sync::LinkParams,
};

/// Parameter identifiers of `IdentificationInformation` (Table 97-9). Only the ones we
/// read are named; the rest are logged by number.
pub mod ident {
	pub const NAME: u16 = 0;
	pub const MODEL_IDENTIFIER: u16 = 1;
	pub const MANUFACTURER: u16 = 2;
	pub const SERIAL_NUMBER: u16 = 3;
	pub const FIRMWARE_VERSION: u16 = 4;
	pub const HARDWARE_VERSION: u16 = 5;
	pub const MESSAGES_SENT_BY_ACCESSORY: u16 = 6;
	pub const MESSAGES_RECEIVED_FROM_DEVICE: u16 = 7;
	pub const CURRENT_LANGUAGE: u16 = 12;
	pub const USB_HOST_TRANSPORT_COMPONENT: u16 = 16;
	pub const VEHICLE_INFORMATION_COMPONENT: u16 = 20;

	/// Sub-parameters of `USBHostTransportComponent` (Table 97-15).
	pub mod usb_host {
		pub const TRANSPORT_COMPONENT_IDENTIFIER: u16 = 0;
		pub const TRANSPORT_COMPONENT_NAME: u16 = 1;
		pub const SUPPORTS_IAP2_CONNECTION: u16 = 2;
		pub const CARPLAY_INTERFACE_NUMBER: u16 = 3;
	}
}

/// Parameter identifiers of the messages we send.
mod param {
	/// `RequestAuthenticationChallengeResponse` (§97.1.3).
	pub const AUTHENTICATION_CHALLENGE: u16 = 0;
	/// `PowerUpdate` (§97.15.2).
	pub const MAXIMUM_CURRENT_DRAWN_FROM_ACCESSORY: u16 = 0;
	pub const DEVICE_BATTERY_WILL_CHARGE: u16 = 1;
	/// `DeviceInformationUpdate` (§97.8.1).
	pub const DEVICE_NAME: u16 = 0;
	/// `DeviceLanguageUpdate` (§97.8.2).
	pub const DEVICE_LANGUAGE: u16 = 0;
	/// `DeviceTimeUpdate` (§97.8.3).
	pub const SECONDS_SINCE_REFERENCE_DATE: u16 = 0;
	pub const TIME_ZONE_OFFSET_MINUTES: u16 = 1;
	pub const DAYLIGHT_SAVINGS_OFFSET_MINUTES: u16 = 2;
	/// `WirelessCarPlayUpdate` (§97.8.5).
	pub const WIRELESS_CARPLAY_STATUS: u16 = 0;
	/// `DeviceTransportIdentifierNotification` (§97.8.6).
	pub const USB_TRANSPORT_IDENTIFIER: u16 = 1;
}

/// `WirelessCarPlayUpdateStatus` (Table 97-92).
const WIRELESS_CARPLAY_UNAVAILABLE: u8 = 0;

/// Authentication must finish inside 15 s of starting, under control session 2 (§19.1).
const AUTH_BUDGET: Duration = Duration::from_secs(15);

/// What we tell the accessory about ourselves.
#[derive(Clone, Debug)]
pub struct DeviceProfile {
	/// `DeviceName`, as shown in Settings.
	pub name: String,
	/// `DeviceLanguage`, an ISO 639-1 code.
	pub language: String,
	/// Current we draw from the accessory, in mA. Matches the gadget's `MaxPower`.
	pub max_current_ma: u16,
	/// The `USBTransportIdentifier` for `DeviceTransportIdentifierNotification`.
	///
	/// Only meaningful before the role switch, where it lets the accessory rule out an
	/// active wireless CarPlay session (Figure 30-2). Leave it `None` over the bulk
	/// transport, where the question has already been settled.
	pub usb_transport_identifier: Option<String>,
	/// The challenge we ask the accessory's coprocessor to sign. Never verified.
	pub challenge: Bytes,
}

impl Default for DeviceProfile {
	fn default() -> Self {
		Self {
			name: "iPhone".into(),
			language: "en".into(),
			max_current_ma: 500,
			usb_transport_identifier: None,
			challenge: Bytes::from_static(&[0xA5; 20]),
		}
	}
}

/// How far the handshake has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
	/// Waiting for the link.
	Idle,
	/// `RequestAuthenticationCertificate` sent.
	Authenticating,
	/// `AuthenticationSucceeded` sent.
	Authenticated,
	/// `IdentificationAccepted` sent and authentication done.
	Ready,
}

/// Everything the handshake has learned, for a [`Policy`] to consult.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
	/// What we report about ourselves.
	pub profile: &'a DeviceProfile,
	/// The negotiated link parameters, once the link is up.
	pub params: Option<&'a LinkParams>,
	/// The accessory's `IdentificationInformation`, once it has arrived. This is the
	/// authoritative statement of which messages the head unit sends and expects.
	pub identification: Option<&'a Message>,
}

impl Context<'_> {
	/// The CarPlay (NCM) interface number the accessory nominated, if it did.
	pub fn carplay_interface(&self) -> Option<u8> {
		self.identification?
			.get(ident::USB_HOST_TRANSPORT_COMPONENT)?
			.sub(ident::usb_host::CARPLAY_INTERFACE_NUMBER)
			.ok()??
			.as_u8()
			.ok()
	}

	/// Whether the accessory said it will send this message (Table 97-9, parameter 6).
	/// Absent identification means "unknown", reported as `true`.
	pub fn accessory_sends(&self, id: u16) -> bool {
		self.declares(ident::MESSAGES_SENT_BY_ACCESSORY, id)
	}

	/// Whether the accessory said it expects to receive this message (parameter 7).
	pub fn accessory_expects(&self, id: u16) -> bool {
		self.declares(ident::MESSAGES_RECEIVED_FROM_DEVICE, id)
	}

	fn declares(&self, param_id: u16, id: u16) -> bool {
		let Some(list) = self.identification.and_then(|m| m.get(param_id)) else {
			return true;
		};
		list.data
			.as_chunks::<2>()
			.0
			.iter()
			.any(|c| u16::from_be_bytes(*c) == id)
	}
}

/// What the device answers beyond the mechanical handshake.
///
/// Every method has a default, so an implementation can be empty ([`DefaultPolicy`]) or
/// override exactly the features it supports. The defaults are also exposed as free
/// functions ([`default_reply`], [`device_notifications`]) so an override can fall back to
/// them for everything it does not handle.
pub trait Policy {
	/// Answer one accessory message that the handshake did not consume.
	///
	/// The default answers the §97.15 power feature and stays silent otherwise, which is
	/// legal: §56.3.2 makes a `Start*` a request rather than a command, so an unanswered
	/// feature simply stays dark.
	fn on_message(&mut self, msg: &Message, cx: Context<'_>) -> Vec<Message> {
		default_reply(msg, cx)
	}

	/// Messages to send once authentication and identification are both complete.
	///
	/// The default sends the unsolicited §97.8 device notifications.
	fn on_ready(&mut self, cx: Context<'_>) -> Vec<Message> {
		device_notifications(cx)
	}

	/// The accessory's identification, the moment it arrives — before [`Self::on_ready`],
	/// and before authentication has necessarily finished under control session 2.
	fn on_identification(&mut self, _msg: &Message, _cx: Context<'_>) {}

	/// The link went down. Any state mirrored from the accessory is now stale.
	fn on_reset(&mut self) {}

	/// Wait for something to report to the accessory unprompted.
	///
	/// `drive` races this against the inbound stream, so the future **must be
	/// cancel-safe** — it is dropped and recreated every time a message arrives. Channel
	/// receivers (`watch::Receiver::changed`, `mpsc::Receiver::recv`) are; a
	/// partially-consumed stream generally is not. The default never resolves, which is
	/// right for a policy that only ever answers.
	fn next_outbound(&mut self) -> impl Future<Output = Vec<Message>> + Send {
		std::future::pending()
	}
}

/// A policy that does only what any device must: answer the power feature and send the
/// §97.8 notifications. Enough for the pre-role-switch gadget, and the baseline the host
/// side builds on.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultPolicy;

impl Policy for DefaultPolicy {}

/// The default answer to an accessory message.
///
/// Call this from a [`Policy::on_message`] override for the messages it does not handle.
pub fn default_reply(msg: &Message, cx: Context<'_>) -> Vec<Message> {
	match msg.id {
		ids::ID_START_POWER_UPDATES => vec![power_update(cx)],
		// Add cases here as the head unit's MessagesReceivedFromDevice list demands them.
		_ => Vec::new(),
	}
}

/// `PowerUpdate` (§97.15.2), from the profile's declared draw.
pub fn power_update(cx: Context<'_>) -> Message {
	Message::new(ids::ID_POWER_UPDATE)
		.param(Param::u16_(
			param::MAXIMUM_CURRENT_DRAWN_FROM_ACCESSORY,
			cx.profile.max_current_ma,
		))
		.param(Param::bool(param::DEVICE_BATTERY_WILL_CHARGE, false))
}

/// The unsolicited notifications a device sends once identification is accepted (§97.8).
/// None of these are gated by a `Start*` request.
pub fn device_notifications(cx: Context<'_>) -> Vec<Message> {
	let mut out = vec![
		Message::new(ids::ID_DEVICE_INFORMATION_UPDATE)
			.param(Param::utf8(param::DEVICE_NAME, &cx.profile.name)),
		Message::new(ids::ID_DEVICE_LANGUAGE_UPDATE)
			.param(Param::utf8(param::DEVICE_LANGUAGE, &cx.profile.language)),
		device_time_update(),
		// We are wired-only, and saying so is what stops the head unit waiting for a
		// wireless handoff (Table 97-92).
		Message::new(ids::ID_WIRELESS_CAR_PLAY_UPDATE).param(Param::u8_(
			param::WIRELESS_CARPLAY_STATUS,
			WIRELESS_CARPLAY_UNAVAILABLE,
		)),
	];
	if let Some(id) = &cx.profile.usb_transport_identifier {
		out.push(
			Message::new(ids::ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION)
				.param(Param::utf8(param::USB_TRANSPORT_IDENTIFIER, id)),
		);
	}
	out
}

/// `DeviceTimeUpdate` (§97.8.3) from the system clock. The reference date is the Unix
/// epoch, and we run in UTC, so both offsets are zero.
pub fn device_time_update() -> Message {
	let secs = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |d| d.as_secs());
	Message::new(ids::ID_DEVICE_TIME_UPDATE)
		.param(Param::u64_(param::SECONDS_SINCE_REFERENCE_DATE, secs))
		.param(Param::i16_(param::TIME_ZONE_OFFSET_MINUTES, 0))
		.param(Param::i8_(param::DAYLIGHT_SAVINGS_OFFSET_MINUTES, 0))
}

/// The device-side control session: the §19.1/§19.2 handshake, plus a [`Policy`] for
/// everything after it.
pub struct Handshake<P = DefaultPolicy> {
	profile: DeviceProfile,
	policy: P,
	stage: Stage,
	authenticated: bool,
	identified: bool,
	auth_started: Option<Instant>,
	identification: Option<Message>,
	params: Option<LinkParams>,
}

impl Handshake<DefaultPolicy> {
	/// A handshake with the stock policy.
	pub fn new(profile: DeviceProfile) -> Self {
		Self::with_policy(profile, DefaultPolicy)
	}
}

impl<P: Policy> Handshake<P> {
	pub fn with_policy(profile: DeviceProfile, policy: P) -> Self {
		Self {
			profile,
			policy,
			stage: Stage::Idle,
			authenticated: false,
			identified: false,
			auth_started: None,
			identification: None,
			params: None,
		}
	}

	pub fn stage(&self) -> Stage {
		self.stage
	}

	pub fn ready(&self) -> bool {
		self.stage == Stage::Ready
	}

	pub fn policy(&self) -> &P {
		&self.policy
	}

	pub fn policy_mut(&mut self) -> &mut P {
		&mut self.policy
	}

	pub fn profile(&self) -> &DeviceProfile {
		&self.profile
	}

	/// The accessory's `IdentificationInformation`, once it has arrived.
	pub fn identification(&self) -> Option<&Message> {
		self.identification.as_ref()
	}

	/// The CarPlay (NCM) interface number the accessory nominated, if it did.
	pub fn carplay_interface(&self) -> Option<u8> {
		self.context().carplay_interface()
	}

	/// What a [`Policy`] sees.
	pub fn context(&self) -> Context<'_> {
		Context {
			profile: &self.profile,
			params: self.params.as_ref(),
			identification: self.identification.as_ref(),
		}
	}

	/// Authentication is device-initiated (§19.1), so the link coming up is our cue.
	///
	/// A second call means the link re-synchronized (§56.2.7.3) and the accessory will
	/// redo the whole exchange, so the handshake starts over.
	pub fn on_established(&mut self, params: LinkParams, now: Instant) -> Vec<Message> {
		self.params = Some(params);
		self.stage = Stage::Authenticating;
		self.authenticated = false;
		self.identified = false;
		self.identification = None;
		self.auth_started = Some(now);
		vec![Message::new(ids::ID_REQUEST_AUTHENTICATION_CERTIFICATE)]
	}

	/// The link went down.
	pub fn on_reset(&mut self) {
		self.stage = Stage::Idle;
		self.authenticated = false;
		self.identified = false;
		self.identification = None;
		self.policy.on_reset();
	}

	/// When authentication must be finished by, while it is still outstanding (§19.1).
	pub fn auth_deadline(&self) -> Option<Instant> {
		if self.authenticated {
			return None;
		}
		self.auth_started.map(|started| started + AUTH_BUDGET)
	}

	/// Fail the handshake if the accessory has run out of time (§19.1).
	pub fn check_deadline(&self, now: Instant) -> Result<(), Error> {
		if let Some(deadline) = self.auth_deadline()
			&& now > deadline
		{
			return Err(Error::Timeout("accessory authentication"));
		}
		Ok(())
	}

	/// Respond to one message from the accessory.
	pub fn on_message(&mut self, msg: &Message) -> Vec<Message> {
		match msg.id {
			// ── §97.1 authentication ──
			ids::ID_AUTHENTICATION_CERTIFICATE => {
				debug!(
					"accessory certificate ({} params); accepting on trust",
					msg.params.len()
				);
				vec![
					Message::new(ids::ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE).param(
						Param::blob(
							param::AUTHENTICATION_CHALLENGE,
							self.profile.challenge.clone(),
						),
					),
				]
			}
			ids::ID_AUTHENTICATION_RESPONSE => {
				self.authenticated = true;
				self.stage = Stage::Authenticated;
				info!("accessory authenticated (challenge response accepted unverified)");
				let mut out = vec![Message::new(ids::ID_AUTHENTICATION_SUCCEEDED)];
				// Under control session 2 identification may already have happened, in
				// which case asking again would be redundant (§56.3.2).
				if self.identified {
					out.extend(self.become_ready());
				} else {
					out.push(Message::new(ids::ID_START_IDENTIFICATION));
				}
				out
			}
			// Informational under control session 2; nothing to answer.
			ids::ID_ACCESSORY_AUTHENTICATION_SERIAL_NUMBER => Vec::new(),

			// ── §97.2 identification ──
			ids::ID_IDENTIFICATION_INFORMATION | ids::ID_IDENTIFICATION_INFORMATION_UPDATE => {
				self.identified = true;
				self.identification = Some(msg.clone());
				let Self {
					policy,
					profile,
					identification,
					params,
					..
				} = self;
				policy.on_identification(
					msg,
					Context {
						profile,
						params: params.as_ref(),
						identification: identification.as_ref(),
					},
				);
				let mut out = vec![Message::new(ids::ID_IDENTIFICATION_ACCEPTED)];
				if self.authenticated {
					out.extend(self.become_ready());
				}
				out
			}
			ids::ID_CANCEL_IDENTIFICATION => {
				warn!("accessory cancelled identification; asking again");
				self.identified = false;
				self.identification = None;
				vec![Message::new(ids::ID_START_IDENTIFICATION)]
			}

			// Everything else is a feature, which is the policy's business.
			_ => {
				let Self {
					policy,
					profile,
					identification,
					params,
					..
				} = self;
				policy.on_message(
					msg,
					Context {
						profile,
						params: params.as_ref(),
						identification: identification.as_ref(),
					},
				)
			}
		}
	}

	fn become_ready(&mut self) -> Vec<Message> {
		self.stage = Stage::Ready;
		let Self {
			policy,
			profile,
			identification,
			params,
			..
		} = self;
		policy.on_ready(Context {
			profile,
			params: params.as_ref(),
			identification: identification.as_ref(),
		})
	}
}

/// Log an `IdentificationInformation` message in full.
///
/// `MessagesSentByAccessory` and `MessagesReceivedFromDevice` are the interesting part:
/// they are the head unit's own statement of which §97 messages a session needs.
pub fn log_identification(msg: &Message) {
	let text = |id: u16| {
		msg.get(id)
			.and_then(|p| p.as_utf8().ok())
			.unwrap_or("(absent)")
			.to_string()
	};
	info!(
		"accessory identification: {} / {} by {} (serial {}, fw {}, hw {}, language {})",
		text(ident::NAME),
		text(ident::MODEL_IDENTIFIER),
		text(ident::MANUFACTURER),
		text(ident::SERIAL_NUMBER),
		text(ident::FIRMWARE_VERSION),
		text(ident::HARDWARE_VERSION),
		text(ident::CURRENT_LANGUAGE),
	);

	for (param_id, label) in [
		(ident::MESSAGES_SENT_BY_ACCESSORY, "sends"),
		(ident::MESSAGES_RECEIVED_FROM_DEVICE, "expects"),
	] {
		let Some(p) = msg.get(param_id) else { continue };
		let names: Vec<String> = p
			.data
			.as_chunks::<2>()
			.0
			.iter()
			.map(|c| {
				let id = u16::from_be_bytes(*c);
				match ids::name(id) {
					Some(name) => name.to_string(),
					None => format!("unknown({id:#06x})"),
				}
			})
			.collect();
		info!(
			"accessory {label} {} message(s): {}",
			names.len(),
			names.join(", ")
		);
	}

	if let Some(usb) = msg.get(ident::USB_HOST_TRANSPORT_COMPONENT) {
		match usb.as_group() {
			Ok(sub) => info!("USBHostTransportComponent: {sub:?}"),
			Err(err) => warn!("USBHostTransportComponent is not a group: {err}"),
		}
	}
	if let Some(vehicle) = msg.get(ident::VEHICLE_INFORMATION_COMPONENT) {
		match vehicle.as_group() {
			Ok(sub) => info!("VehicleInformationComponent: {sub:?}"),
			Err(err) => warn!("VehicleInformationComponent is not a group: {err}"),
		}
	}

	// Anything we did not name above, so nothing goes unrecorded.
	let known = [
		ident::NAME,
		ident::MODEL_IDENTIFIER,
		ident::MANUFACTURER,
		ident::SERIAL_NUMBER,
		ident::FIRMWARE_VERSION,
		ident::HARDWARE_VERSION,
		ident::MESSAGES_SENT_BY_ACCESSORY,
		ident::MESSAGES_RECEIVED_FROM_DEVICE,
		ident::CURRENT_LANGUAGE,
		ident::USB_HOST_TRANSPORT_COMPONENT,
		ident::VEHICLE_INFORMATION_COMPONENT,
	];
	for p in msg.params.iter().filter(|p| !known.contains(&p.id)) {
		debug!("identification {p:?}");
	}
}

/// Run the handshake to completion, then keep answering the accessory.
///
/// Returns when the transport ends or the link resets. `on_ready` fires each time the
/// handshake completes, with the handshake and the negotiated parameters.
pub async fn drive<T, P, F>(
	session: &mut ControlSession<T>,
	handshake: &mut Handshake<P>,
	mut on_ready: F,
) -> Result<(), Error>
where
	T: futures::Stream<Item = Result<Frame, Error>> + futures::Sink<Frame, Error = Error> + Unpin,
	P: Policy,
	F: FnMut(&Handshake<P>, &LinkParams),
{
	let mut announced = false;

	loop {
		// Three things can wake us: the accessory, the policy wanting to report something
		// unprompted, or the authentication budget running out while the accessory says
		// nothing at all. The borrow of `handshake` has to end before the branch bodies
		// touch it, so the select only classifies the wake-up.
		let wake = {
			let deadline = handshake.auth_deadline();
			let policy = handshake.policy_mut();
			tokio::select! {
				event = session.next() => Wake::Event(event),
				outbound = policy.next_outbound() => Wake::Outbound(outbound),
				() = deadline_elapsed(deadline) => Wake::AuthDeadline,
			}
		};

		let replies = match wake {
			Wake::AuthDeadline => {
				handshake.check_deadline(Instant::now())?;
				continue;
			}
			Wake::Outbound(msgs) => msgs,
			Wake::Event(None) => break,
			Wake::Event(Some(event)) => match event? {
				ControlEvent::Established(params) => {
					announced = false;
					handshake.on_established(params, Instant::now())
				}
				ControlEvent::Message(msg) => {
					debug!("rx {msg:?}");
					if msg.id == ids::ID_IDENTIFICATION_INFORMATION
						|| msg.id == ids::ID_IDENTIFICATION_INFORMATION_UPDATE
					{
						log_identification(&msg);
					}
					if msg.id == ids::ID_AUTHENTICATION_FAILED {
						// The accessory decides this, not us; there is no recovery.
						return Err(Error::Timeout("accessory authentication"));
					}
					handshake.on_message(&msg)
				}
				ControlEvent::OtherSession { session: id, len } => {
					debug!("ignoring {len} bytes on session {id}");
					Vec::new()
				}
				ControlEvent::Reset => {
					handshake.on_reset();
					return Err(Error::Reset);
				}
				ControlEvent::Sleep => {
					info!("accessory reported sleep");
					Vec::new()
				}
			},
		};

		for reply in replies {
			session.send(reply).await?;
		}

		if handshake.ready()
			&& !announced
			&& let Some(params) = handshake.params.clone()
		{
			announced = true;
			on_ready(handshake, &params);
		}
	}

	Err(Error::Closed)
}

/// What woke [`drive`].
enum Wake {
	Event(Option<Result<ControlEvent, Error>>),
	Outbound(Vec<Message>),
	AuthDeadline,
}

/// Resolves at `deadline`, or never if there is none.
async fn deadline_elapsed(deadline: Option<Instant>) {
	match deadline {
		Some(at) => tokio::time::sleep_until(at.into()).await,
		None => std::future::pending().await,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn run<P: Policy>(handshake: &mut Handshake<P>, id: u16) -> Vec<u16> {
		handshake
			.on_message(&Message::new(id))
			.into_iter()
			.map(|m| m.id)
			.collect()
	}

	fn establish<P: Policy>(handshake: &mut Handshake<P>) -> Vec<u16> {
		handshake
			.on_established(LinkParams::usb_recommended(), Instant::now())
			.into_iter()
			.map(|m| m.id)
			.collect()
	}

	#[test]
	fn drives_the_handshake_in_order() {
		let mut p = Handshake::new(DeviceProfile::default());
		assert_eq!(p.stage(), Stage::Idle);

		assert_eq!(
			establish(&mut p),
			vec![ids::ID_REQUEST_AUTHENTICATION_CERTIFICATE]
		);
		assert_eq!(p.stage(), Stage::Authenticating);

		assert_eq!(
			run(&mut p, ids::ID_AUTHENTICATION_CERTIFICATE),
			vec![ids::ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE]
		);
		assert_eq!(
			run(&mut p, ids::ID_AUTHENTICATION_RESPONSE),
			vec![
				ids::ID_AUTHENTICATION_SUCCEEDED,
				ids::ID_START_IDENTIFICATION
			]
		);
		assert_eq!(p.stage(), Stage::Authenticated);

		let after_ident = run(&mut p, ids::ID_IDENTIFICATION_INFORMATION);
		assert_eq!(after_ident[0], ids::ID_IDENTIFICATION_ACCEPTED);
		assert!(after_ident.contains(&ids::ID_DEVICE_INFORMATION_UPDATE));
		assert!(after_ident.contains(&ids::ID_DEVICE_LANGUAGE_UPDATE));
		assert!(after_ident.contains(&ids::ID_DEVICE_TIME_UPDATE));
		assert!(after_ident.contains(&ids::ID_WIRELESS_CAR_PLAY_UPDATE));
		assert!(p.ready());
	}

	#[test]
	fn tolerates_identification_before_authentication() {
		// Control session 2 lets the accessory identify first (§56.3.2).
		let mut p = Handshake::new(DeviceProfile::default());
		establish(&mut p);
		assert_eq!(
			run(&mut p, ids::ID_IDENTIFICATION_INFORMATION),
			vec![ids::ID_IDENTIFICATION_ACCEPTED]
		);
		assert!(!p.ready());

		run(&mut p, ids::ID_AUTHENTICATION_CERTIFICATE);
		let out = run(&mut p, ids::ID_AUTHENTICATION_RESPONSE);
		// Identification already happened, so we must not ask for it again.
		assert!(!out.contains(&ids::ID_START_IDENTIFICATION));
		assert_eq!(out[0], ids::ID_AUTHENTICATION_SUCCEEDED);
		assert!(p.ready());
	}

	#[test]
	fn re_asks_after_cancelled_identification() {
		let mut p = Handshake::new(DeviceProfile::default());
		establish(&mut p);
		run(&mut p, ids::ID_AUTHENTICATION_CERTIFICATE);
		run(&mut p, ids::ID_AUTHENTICATION_RESPONSE);
		run(&mut p, ids::ID_IDENTIFICATION_INFORMATION);
		assert_eq!(
			run(&mut p, ids::ID_CANCEL_IDENTIFICATION),
			vec![ids::ID_START_IDENTIFICATION]
		);
	}

	#[test]
	fn answers_power_updates_with_real_values() {
		let mut p = Handshake::new(DeviceProfile {
			max_current_ma: 500,
			..Default::default()
		});
		let out = p.on_message(&Message::new(ids::ID_START_POWER_UPDATES));
		assert_eq!(out.len(), 1);
		assert_eq!(out[0].id, ids::ID_POWER_UPDATE);
		assert_eq!(
			out[0]
				.get(param::MAXIMUM_CURRENT_DRAWN_FROM_ACCESSORY)
				.unwrap()
				.as_u16()
				.unwrap(),
			500
		);
		assert!(
			p.on_message(&Message::new(ids::ID_STOP_POWER_UPDATES))
				.is_empty()
		);
	}

	#[test]
	fn stays_silent_on_features_we_do_not_implement() {
		let mut p = Handshake::new(DeviceProfile::default());
		for id in [
			ids::ID_START_NOW_PLAYING_UPDATES,
			ids::ID_START_CALL_STATE_UPDATES,
			ids::ID_START_ROUTE_GUIDANCE_UPDATES,
			ids::ID_START_HID,
		] {
			assert!(p.on_message(&Message::new(id)).is_empty(), "{id:#06x}");
		}
	}

	#[test]
	fn transport_identifier_is_opt_in() {
		let mut p = Handshake::new(DeviceProfile::default());
		establish(&mut p);
		run(&mut p, ids::ID_AUTHENTICATION_CERTIFICATE);
		run(&mut p, ids::ID_AUTHENTICATION_RESPONSE);
		let out = run(&mut p, ids::ID_IDENTIFICATION_INFORMATION);
		assert!(!out.contains(&ids::ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION));

		let mut p = Handshake::new(DeviceProfile {
			usb_transport_identifier: Some("usb0".into()),
			..Default::default()
		});
		establish(&mut p);
		run(&mut p, ids::ID_AUTHENTICATION_CERTIFICATE);
		run(&mut p, ids::ID_AUTHENTICATION_RESPONSE);
		let out = run(&mut p, ids::ID_IDENTIFICATION_INFORMATION);
		assert!(out.contains(&ids::ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION));
	}

	#[test]
	fn authentication_has_a_deadline() {
		let mut p = Handshake::new(DeviceProfile::default());
		let start = Instant::now();
		p.on_established(LinkParams::usb_recommended(), start);
		assert!(p.check_deadline(start + Duration::from_secs(14)).is_ok());
		assert!(p.check_deadline(start + Duration::from_secs(16)).is_err());
		// Once authenticated the budget no longer applies.
		run(&mut p, ids::ID_AUTHENTICATION_CERTIFICATE);
		run(&mut p, ids::ID_AUTHENTICATION_RESPONSE);
		assert!(p.check_deadline(start + Duration::from_secs(60)).is_ok());
	}

	#[test]
	fn reads_the_carplay_interface_number() {
		let mut p = Handshake::new(DeviceProfile::default());
		let ident = Message::new(ids::ID_IDENTIFICATION_INFORMATION).param(Param::group(
			ident::USB_HOST_TRANSPORT_COMPONENT,
			&[
				Param::u16_(ident::usb_host::TRANSPORT_COMPONENT_IDENTIFIER, 1),
				Param::utf8(ident::usb_host::TRANSPORT_COMPONENT_NAME, "iAP Interface"),
				Param::none(ident::usb_host::SUPPORTS_IAP2_CONNECTION),
				Param::u8_(ident::usb_host::CARPLAY_INTERFACE_NUMBER, 2),
			],
		));
		p.on_message(&ident);
		assert_eq!(p.carplay_interface(), Some(2));
		log_identification(&ident);
	}

	/// The whole stack against a scripted accessory: two `Link`s over a duplex pipe with
	/// the same codec the bulk transport uses, a real `ControlSession` on each side, and
	/// `drive` running the device half.
	#[tokio::test]
	async fn end_to_end_handshake_against_a_scripted_accessory() {
		use bytes::Bytes;
		use tokio_util::codec::Framed;

		use crate::iap2::{
			link::{Link, Role},
			packet::LinkCodec,
			session::ControlSession,
			sync::LinkParams,
		};

		// Run with `RUST_LOG=carplayd=trace cargo test end_to_end -- --nocapture` to watch
		// the exchange the way it will look against a real head unit.
		let _ = env_logger::builder().is_test(true).try_init();

		let (device_io, accessory_io) = tokio::io::duplex(16 * 1024);
		let proposal = LinkParams::usb_recommended();

		let mut device = ControlSession::new(Link::new(
			Framed::new(device_io, LinkCodec::default()),
			Role::Device,
			proposal.clone(),
		));
		let mut accessory = ControlSession::new(Link::new(
			Framed::new(accessory_io, LinkCodec::default()),
			Role::Accessory,
			proposal,
		));

		// The accessory: answers the handshake, then asks for power updates.
		let head_unit = async move {
			let mut seen = Vec::new();
			while let Some(event) = accessory.next().await {
				let replies = match event.unwrap() {
					ControlEvent::Message(msg) => {
						seen.push(msg.id);
						match msg.id {
							ids::ID_REQUEST_AUTHENTICATION_CERTIFICATE => vec![
								Message::new(ids::ID_AUTHENTICATION_CERTIFICATE)
									.param(Param::blob(0, Bytes::from_static(b"pretend cert"))),
							],
							ids::ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE => {
								// The challenge is ours; a real head unit signs it.
								assert_eq!(msg.get(0).unwrap().data.len(), 20);
								vec![
									Message::new(ids::ID_AUTHENTICATION_RESPONSE)
										.param(Param::blob(0, Bytes::from_static(b"signed"))),
								]
							}
							ids::ID_START_IDENTIFICATION => vec![identification()],
							ids::ID_IDENTIFICATION_ACCEPTED => {
								vec![Message::new(ids::ID_START_POWER_UPDATES)]
							}
							ids::ID_POWER_UPDATE => break,
							_ => Vec::new(),
						}
					}
					_ => Vec::new(),
				};
				for reply in replies {
					accessory.send(reply).await.unwrap();
				}
			}
			seen
		};

		let mut handshake = Handshake::new(DeviceProfile::default());
		let phone = async {
			let mut ready_params = None;
			let result = drive(&mut device, &mut handshake, |_, params| {
				ready_params = Some(params.clone());
			})
			.await;
			// `drive` only returns on failure; the accessory hanging up is the expected one.
			assert!(
				matches!(result, Err(Error::Closed) | Err(Error::Transport(_))),
				"{result:?}"
			);
			ready_params
		};

		let (seen, ready) = tokio::time::timeout(
			Duration::from_secs(5),
			futures::future::join(head_unit, phone),
		)
		.await
		.expect("handshake timed out");

		// The accessory saw the full device-driven sequence, in order.
		assert_eq!(
			seen,
			vec![
				ids::ID_REQUEST_AUTHENTICATION_CERTIFICATE,
				ids::ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
				ids::ID_AUTHENTICATION_SUCCEEDED,
				ids::ID_START_IDENTIFICATION,
				ids::ID_IDENTIFICATION_ACCEPTED,
				// The five-message burst that follows identification must all get through
				// without stalling on the send window.
				ids::ID_DEVICE_INFORMATION_UPDATE,
				ids::ID_DEVICE_LANGUAGE_UPDATE,
				ids::ID_DEVICE_TIME_UPDATE,
				ids::ID_WIRELESS_CAR_PLAY_UPDATE,
				ids::ID_POWER_UPDATE,
			]
		);
		assert!(ready.is_some(), "the ready callback should have fired");
		assert_eq!(ready.unwrap().control_session_version(), Some(2));
		assert_eq!(handshake.stage(), Stage::Ready);
		assert_eq!(handshake.carplay_interface(), Some(2));
		assert_eq!(
			handshake
				.identification()
				.unwrap()
				.get(ident::NAME)
				.unwrap()
				.as_utf8()
				.unwrap(),
			"Test Head Unit"
		);
	}

	/// A plausible `IdentificationInformation`, with the parameters CarPlay requires.
	fn identification() -> Message {
		let messages =
			|ids: &[u16]| -> Vec<u8> { ids.iter().flat_map(|i| i.to_be_bytes()).collect() };
		Message::new(ids::ID_IDENTIFICATION_INFORMATION)
			.param(Param::utf8(ident::NAME, "Test Head Unit"))
			.param(Param::utf8(ident::MODEL_IDENTIFIER, "15CY MOP"))
			.param(Param::utf8(ident::MANUFACTURER, "Test Motors"))
			.param(Param::utf8(ident::SERIAL_NUMBER, "SN0001"))
			.param(Param::utf8(ident::FIRMWARE_VERSION, "1.0.8"))
			.param(Param::utf8(ident::HARDWARE_VERSION, "1.0"))
			.param(Param::blob(
				ident::MESSAGES_SENT_BY_ACCESSORY,
				messages(&[
					ids::ID_START_POWER_UPDATES,
					ids::ID_START_NOW_PLAYING_UPDATES,
				]),
			))
			.param(Param::blob(
				ident::MESSAGES_RECEIVED_FROM_DEVICE,
				messages(&[ids::ID_POWER_UPDATE, ids::ID_DEVICE_TIME_UPDATE, 0xFFFF]),
			))
			.param(Param::utf8(ident::CURRENT_LANGUAGE, "en"))
			.param(Param::group(
				ident::USB_HOST_TRANSPORT_COMPONENT,
				&[
					Param::u16_(ident::usb_host::TRANSPORT_COMPONENT_IDENTIFIER, 1),
					Param::utf8(ident::usb_host::TRANSPORT_COMPONENT_NAME, "iAP Interface"),
					Param::none(ident::usb_host::SUPPORTS_IAP2_CONNECTION),
					Param::u8_(ident::usb_host::CARPLAY_INTERFACE_NUMBER, 2),
				],
			))
	}

	/// The trait's intended use: a policy holding state from another subsystem, answering
	/// the one feature it supports and pushing an update when that state changes.
	#[tokio::test]
	async fn a_stateful_policy_answers_and_pushes() {
		use bytes::Bytes;
		use tokio::sync::watch;
		use tokio_util::codec::Framed;

		use crate::iap2::{
			link::{Link, Role},
			packet::LinkCodec,
			session::ControlSession,
		};

		/// `MediaItem` sub-parameters (Table 97-129).
		const MEDIA_ITEM_TITLE: u16 = 1;
		/// `NowPlayingUpdate` parameter 0 is a `MediaItemAttributes` group (§97.13.2).
		const MEDIA_ITEM_ATTRIBUTES: u16 = 0;

		/// Stands in for the Android Auto side: whatever is playing over there.
		struct Bridge {
			now_playing: watch::Receiver<String>,
			/// The accessory only gets updates after it asks for them (§97.13.1).
			subscribed: bool,
			identified: bool,
		}

		impl Bridge {
			fn now_playing_update(&self) -> Message {
				Message::new(ids::ID_NOW_PLAYING_UPDATE).param(Param::group(
					MEDIA_ITEM_ATTRIBUTES,
					&[Param::utf8(MEDIA_ITEM_TITLE, &self.now_playing.borrow())],
				))
			}
		}

		impl Policy for Bridge {
			fn on_message(&mut self, msg: &Message, cx: Context<'_>) -> Vec<Message> {
				match msg.id {
					ids::ID_START_NOW_PLAYING_UPDATES => {
						self.subscribed = true;
						vec![self.now_playing_update()]
					}
					ids::ID_STOP_NOW_PLAYING_UPDATES => {
						self.subscribed = false;
						Vec::new()
					}
					// Everything we do not special-case keeps the stock behaviour, so the
					// power feature still works without repeating it here.
					_ => default_reply(msg, cx),
				}
			}

			fn on_identification(&mut self, _msg: &Message, _cx: Context<'_>) {
				self.identified = true;
			}

			// `watch::Receiver::changed` is cancel-safe, which `drive` requires.
			async fn next_outbound(&mut self) -> Vec<Message> {
				if self.now_playing.changed().await.is_err() {
					// The bridge is gone; never wake again rather than spinning.
					std::future::pending::<()>().await;
				}
				if self.subscribed {
					vec![self.now_playing_update()]
				} else {
					Vec::new()
				}
			}
		}

		let (track_tx, track_rx) = watch::channel("Track One".to_string());
		let (device_io, accessory_io) = tokio::io::duplex(16 * 1024);
		let proposal = LinkParams::usb_recommended();

		let mut device = ControlSession::new(Link::new(
			Framed::new(device_io, LinkCodec::default()),
			Role::Device,
			proposal.clone(),
		));
		let mut accessory = ControlSession::new(Link::new(
			Framed::new(accessory_io, LinkCodec::default()),
			Role::Accessory,
			proposal,
		));

		let head_unit = async move {
			let mut titles = Vec::new();
			let mut asked = false;
			while let Some(event) = accessory.next().await {
				let replies = match event.unwrap() {
					ControlEvent::Message(msg) => match msg.id {
						ids::ID_REQUEST_AUTHENTICATION_CERTIFICATE => vec![
							Message::new(ids::ID_AUTHENTICATION_CERTIFICATE)
								.param(Param::blob(0, Bytes::from_static(b"cert"))),
						],
						ids::ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE => vec![
							Message::new(ids::ID_AUTHENTICATION_RESPONSE)
								.param(Param::blob(0, Bytes::from_static(b"signed"))),
						],
						ids::ID_START_IDENTIFICATION => vec![identification()],
						ids::ID_IDENTIFICATION_ACCEPTED => {
							asked = true;
							// Ask for both features: one the policy overrides, one it
							// leaves to the default.
							vec![
								Message::new(ids::ID_START_NOW_PLAYING_UPDATES),
								Message::new(ids::ID_START_POWER_UPDATES),
							]
						}
						ids::ID_NOW_PLAYING_UPDATE => {
							let title = msg
								.get(MEDIA_ITEM_ATTRIBUTES)
								.unwrap()
								.sub(MEDIA_ITEM_TITLE)
								.unwrap()
								.unwrap()
								.as_utf8()
								.unwrap()
								.to_string();
							titles.push(title);
							if titles.len() == 1 {
								// Now make the bridge's state change behind our back.
								track_tx.send("Track Two".to_string()).unwrap();
							} else {
								break;
							}
							Vec::new()
						}
						_ => Vec::new(),
					},
					_ => Vec::new(),
				};
				for reply in replies {
					accessory.send(reply).await.unwrap();
				}
			}
			assert!(asked);
			titles
		};

		let mut handshake = Handshake::with_policy(
			DeviceProfile::default(),
			Bridge {
				now_playing: track_rx,
				subscribed: false,
				identified: false,
			},
		);
		let phone = async {
			let result = drive(&mut device, &mut handshake, |_, _| {}).await;
			assert!(
				matches!(result, Err(Error::Closed) | Err(Error::Transport(_))),
				"{result:?}"
			);
		};

		let (titles, ()) = tokio::time::timeout(
			Duration::from_secs(5),
			futures::future::join(head_unit, phone),
		)
		.await
		.expect("stateful policy exchange timed out");

		// The first came from answering StartNowPlayingUpdates; the second arrived
		// unprompted, from next_outbound.
		assert_eq!(titles, vec!["Track One", "Track Two"]);
		assert!(handshake.policy().identified);
		assert!(handshake.policy().subscribed);
	}
}
