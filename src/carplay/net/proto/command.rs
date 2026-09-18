//! `POST /command` (§30.3.7) — everything that is neither setup nor media.
//!
//! Both sides send commands, on different sockets. The device sends its commands on the
//! **control** connection, the one we opened to the accessory's Bonjour port. The accessory
//! sends its commands on the **event** connection, the one we connect out to after the
//! initial `SETUP` and then serve. Same URL, same body format, opposite directions — which
//! is why [`DeviceCommand`] and [`AccessoryCommand`] are separate types rather than one
//! enum with a `Sender` field.
//!
//! Every body is a dictionary with a `type` string naming the command; the remaining keys
//! are its parameters. Apple's own example in §30.3.7 is flat:
//!
//! ```text
//! { type: "hidSendReport", uuid: "cbb6…", hidReport: <00112233 44> }
//! ```
//!
//! but `modesChanged` nests its parameters one level down under `params`, verified against
//! a real capture. There is no rule; it is per command. Each variant below encodes whatever
//! that command actually does, and [`AccessoryCommand::from_value`] accepts either shape
//! where the evidence is ambiguous.
//!
//! Decoding deliberately does not go through a `#[serde(tag = "type")]` enum. Serde's
//! internally-tagged representation buffers the body through its own `Content` type, and
//! `plist::Data` survives that only by luck — `hidSendReport` and `iAPSendMessage` both
//! carry data. Reading the tag first and then deserialising the dictionary directly keeps
//! the plist deserialiser on the fast path where `Data` is a real data node.

use anyhow::{Context, Result, bail};
use plist::{Data, Dictionary, Value};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
	carplay::net::proto::{
		info::VehicleInformation,
		modes::{ModeChange, Modes},
	},
	carplay_enum,
};

/// The key naming the command in every `/command` body.
pub const TYPE_KEY: &str = "type";

carplay_enum! {
	/// `siriAction` (Table 30-72).
	///
	/// A short press on a shared speech button sends `Prewarm` then `ButtonUp` and hands
	/// off to the accessory's own recogniser; a long press adds a `ButtonDown` once the
	/// timeout — at most 600 ms — elapses.
	SiriAction {
		/// Start warming Siri up. No resources are taken yet.
		Prewarm = 1,
		ButtonDown = 2,
		ButtonUp = 3,
	}
}

carplay_enum! {
	/// `hidInputMode` (Table 30-85). Tells a touchpad what the user is about to do so it
	/// can bias its gesture recognition.
	HidInputMode {
		Default = 0,
		Character = 1,
		Scrolling = 2,
		ScrollingWithCharacters = 3,
		DialPad = 4,
	}
}

/// A command **we** send to the accessory, on the control connection.
#[derive(Clone, Debug)]
pub enum DeviceCommand {
	/// Ramp the accessory's own audio down, e.g. before a navigation prompt (Table 30-73).
	DuckAudio { duration_ms: u32, volume: f64 },
	/// Ramp it back up (Table 30-74).
	UnduckAudio { duration_ms: u32 },
	/// Drop Bluetooth HFP/A2DP/AVRCP to this device for the life of the session (Table
	/// 30-75). Only legal once the accessory has published `bluetoothIDs`.
	DisableBluetooth { device_id: String },
	/// Announce the new owner of each resource (Table 30-80). Nests under `params`. **[V]**
	ModesChanged(Modes),
	/// Bias a HID's gesture recognition (Table 30-84).
	HidSetInputMode { uuid: String, mode: HidInputMode },
	/// Ask for the accessory's own UI (Table 30-88). `url` is only understood when the
	/// accessory declared `enhancedRequestCarUI`; `Some("oem:back")` returns to the last
	/// native screen, `None` shows a screen that can re-enter CarPlay.
	RequestUi { url: Option<String> },
	/// Discard unplayed audio.
	FlushAudio,
	/// Fire a touchpad haptic, within 140 ms (Table 30-93).
	PerformHapticFeedback { uuid: String, feedback_type: i64 },
	/// Tunnel an iAP2 message (Table 30-92). Wireless sessions only — over USB, iAP2 has
	/// its own bulk endpoints and this must not be used.
	IapSendMessage { data: Vec<u8> },
}

impl DeviceCommand {
	/// The `type` string this command carries.
	pub fn type_name(&self) -> &'static str {
		match self {
			Self::DuckAudio { .. } => "duckAudio",
			Self::UnduckAudio { .. } => "unduckAudio",
			Self::DisableBluetooth { .. } => "disableBluetooth",
			Self::ModesChanged(_) => "modesChanged",
			Self::HidSetInputMode { .. } => "hidSetInputMode",
			Self::RequestUi { .. } => "requestUI",
			Self::FlushAudio => "flushAudio",
			Self::PerformHapticFeedback { .. } => "performHapticFeedback",
			Self::IapSendMessage { .. } => "iAPSendMessage",
		}
	}

	/// Build the plist body.
	pub fn to_value(&self) -> Result<Value> {
		let mut body = match self {
			Self::DuckAudio {
				duration_ms,
				volume,
			} => {
				let mut d = Dictionary::new();
				d.insert("durationMs".into(), Value::Integer((*duration_ms).into()));
				d.insert("volume".into(), Value::Real(*volume));
				d
			}
			Self::UnduckAudio { duration_ms } => {
				let mut d = Dictionary::new();
				d.insert("durationMs".into(), Value::Integer((*duration_ms).into()));
				d
			}
			Self::DisableBluetooth { device_id } => {
				let mut d = Dictionary::new();
				d.insert("deviceID".into(), Value::String(device_id.clone()));
				d
			}
			Self::ModesChanged(modes) => {
				let mut d = Dictionary::new();
				d.insert("params".into(), to_dictionary_value(modes)?);
				d
			}
			Self::HidSetInputMode { uuid, mode } => {
				let mut d = Dictionary::new();
				d.insert("uuid".into(), Value::String(uuid.clone()));
				d.insert(
					"hidInputMode".into(),
					Value::Integer(i64::from(*mode).into()),
				);
				d
			}
			Self::RequestUi { url } => {
				let mut d = Dictionary::new();
				if let Some(url) = url {
					d.insert("url".into(), Value::String(url.clone()));
				}
				d
			}
			Self::FlushAudio => Dictionary::new(),
			Self::PerformHapticFeedback {
				uuid,
				feedback_type,
			} => {
				let mut d = Dictionary::new();
				d.insert("uuid".into(), Value::String(uuid.clone()));
				d.insert(
					"hapticFeedbackType".into(),
					Value::Integer((*feedback_type).into()),
				);
				d
			}
			Self::IapSendMessage { data } => {
				let mut d = Dictionary::new();
				d.insert("data".into(), Value::Data(data.clone()));
				d
			}
		};

		body.insert(TYPE_KEY.into(), Value::String(self.type_name().into()));
		Ok(Value::Dictionary(body))
	}

	/// Build the plist body, already encoded for a request.
	pub fn to_binary(&self) -> Result<Vec<u8>> {
		super::to_binary(&self.to_value()?)
	}
}

/// A command the **accessory** sends us, on the event connection.
#[derive(Clone, Debug)]
pub enum AccessoryCommand {
	/// Ask for the screen or main audio (Table 30-76). Answer with a
	/// [`ChangeModesResponse`]; a non-zero `status` is a refusal, and the accessory must
	/// wait for the answer before acting except in a safety emergency.
	ChangeModes(ModeChange),
	/// Send a key frame now. Recovery only — a head unit that sends this periodically is
	/// misbehaving, and it is worth logging if one does.
	ForceKeyFrame,
	/// A HID event (Table 30-83).
	HidSendReport(HidSendReport),
	/// Invoke Siri (Table 30-86). Must follow a real button press.
	RequestSiri { action: SiriAction },
	/// Ask for the CarPlay UI, optionally a specific app (Table 30-87). Legal `url` values
	/// are `maps:`, `mobilephone:`, `music:`, `nowplaying:` and `tel:`. Never sent on
	/// connection.
	RequestUi { url: Option<String> },
	/// It is dark outside (Table 30-89).
	SetNightMode { night_mode: bool },
	/// Limit the UI elements named in `limitedUIElements` (Table 30-90).
	SetLimitedUi { limited_ui: bool },
	/// New vehicle feature state (Table 30-91).
	UpdateVehicleInformation {
		vehicle_information: VehicleInformation,
	},
	/// Tunnelled iAP2 message (Table 30-92). Wireless only.
	IapSendMessage { data: Vec<u8> },
	/// Something not in R29. Kept whole so it can be logged rather than guessed at.
	Unknown { r#type: String, body: Value },
}

impl AccessoryCommand {
	/// Parse one `/command` body.
	pub fn from_value(value: Value) -> Result<Self> {
		let dict = value
			.into_dictionary()
			.context("a /command body must be a dictionary")?;

		let Some(Value::String(name)) = dict.get(TYPE_KEY) else {
			bail!("a /command body must carry a string `type`");
		};
		let name = name.clone();

		Ok(match name.as_str() {
			"changeModes" => Self::ChangeModes(unwrap_params(&dict)?),
			"forceKeyFrame" => Self::ForceKeyFrame,
			"hidSendReport" => Self::HidSendReport(from_dictionary(&dict)?),
			"requestSiri" => Self::RequestSiri {
				action: integer(&dict, "siriAction")
					.context("requestSiri without a siriAction")?
					.into(),
			},
			"requestUI" => Self::RequestUi {
				url: string(&dict, "url"),
			},
			"setNightMode" => Self::SetNightMode {
				night_mode: boolean(&dict, "nightMode")
					.context("setNightMode without a nightMode")?,
			},
			"setLimitedUI" => Self::SetLimitedUi {
				limited_ui: boolean(&dict, "limitedUI")
					.context("setLimitedUI without a limitedUI")?,
			},
			"updateVehicleInformation" => Self::UpdateVehicleInformation {
				vehicle_information: dict
					.get("vehicleInformation")
					.map(plist::from_value)
					.transpose()
					.context("failed to parse vehicleInformation")?
					.context("updateVehicleInformation without a vehicleInformation")?,
			},
			"iAPSendMessage" => Self::IapSendMessage {
				data: match dict.get("data") {
					Some(Value::Data(data)) => data.clone(),
					_ => bail!("iAPSendMessage without data"),
				},
			},
			_ => Self::Unknown {
				r#type: name,
				body: Value::Dictionary(dict),
			},
		})
	}

	/// Parse one `/command` body straight off the wire.
	pub fn from_binary(bytes: &[u8]) -> Result<Self> {
		Self::from_value(super::from_binary(bytes)?)
	}
}

/// `hidSendReport` parameters (Table 30-83).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidSendReport {
	/// Matches a [`super::info::HidDevice::uuid`] from `/info`.
	///
	/// Some head units use a small integer for this identifier. Keep this in lockstep with
	/// `HidDevice::uuid`, or the device can advertise successfully and then fail as soon as
	/// it sends the first touch report.
	#[serde(deserialize_with = "super::flexible::string")]
	pub uuid: String,
	/// A USB HID report, to be read against that device's descriptor. A VW head unit sends
	/// five bytes: report ID, then X and Y as little-endian `u16` -- USB HID is always
	/// little-endian, unlike everything else on this wire. **[V]**
	pub hid_report: Data,
	/// NTP timestamp on our clock when the event happened. Absent on some head units, in
	/// which case treat arrival as the event time.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timestamp: Option<f64>,
}

/// The answer to a `changeModes` (Table 30-79).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeModesResponse {
	/// Zero on success; any other value means the request was refused.
	pub status: i64,
	/// The resulting modes, present only on success.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub params: Option<Modes>,
}

impl ChangeModesResponse {
	pub fn accepted(modes: Modes) -> Self {
		Self {
			status: 0,
			params: Some(modes),
		}
	}

	pub fn refused(status: i64) -> Self {
		Self {
			status,
			params: None,
		}
	}
}

/// Serialise a payload and require that it came out as a dictionary.
fn to_dictionary_value<T: Serialize>(value: &T) -> Result<Value> {
	let value = plist::to_value(value).context("failed to encode a command payload")?;
	if value.as_dictionary().is_none() {
		bail!("a command payload must encode as a dictionary");
	}
	Ok(value)
}

fn from_dictionary<T: DeserializeOwned>(dict: &Dictionary) -> Result<T> {
	plist::from_value(&Value::Dictionary(dict.clone())).context("failed to parse a command payload")
}

/// Read a payload that may be nested under `params` or splatted into the body.
///
/// `modesChanged` nests and Apple's own `hidSendReport` example does not, so for the
/// commands where we have no capture to settle it, accept both.
fn unwrap_params<T: DeserializeOwned>(dict: &Dictionary) -> Result<T> {
	match dict.get("params") {
		Some(params) => plist::from_value(params).context("failed to parse command params"),
		None => from_dictionary(dict),
	}
}

fn integer(dict: &Dictionary, key: &str) -> Option<i64> {
	dict.get(key)?.as_signed_integer()
}

fn string(dict: &Dictionary, key: &str) -> Option<String> {
	Some(dict.get(key)?.as_string()?.to_owned())
}

/// Accessory commands carry booleans the same loose way `/info` does — `0` and `1` as
/// integers — so accept either. See [`super::flexible`].
fn boolean(dict: &Dictionary, key: &str) -> Option<bool> {
	let value = dict.get(key)?;
	value.as_boolean().or_else(|| {
		let n = value.as_signed_integer()?;
		Some(n != 0)
	})
}
