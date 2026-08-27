//! `GET /info` (§30.3.2.4) — the mutual introduction.
//!
//! One round trip, both bodies binary plists. We send [`InfoRequest`], describing ourselves
//! and optionally narrowing what we want back; the accessory answers with [`AccessoryInfo`],
//! which is where every number the rest of the session needs comes from: the display
//! geometry and UUID for the screen stream, the audio formats and latencies for the audio
//! streams, the HID descriptors for input, and the accessory's initial claim on the shared
//! resources.
//!
//! A real head unit's response is around 1.5 kB. Almost every field is optional here even
//! where R29 marks it required, because a missing key must degrade one feature rather than
//! fail the session — the exceptions are the handful the spec requires *and* that we cannot
//! proceed without.
//!
//! §30.3.4.1 notes this is not a one-shot: the accessory must be ready to answer an `/info`
//! asking for an updated [`AccessoryInfo::modes`] at any point in the session.

use plist::Data;
use serde::{Deserialize, Serialize};

use crate::{carplay::net::proto::modes::ModeChange, carplay_enum};

/// `GET /info` request (Table 30-27).
///
/// **Only `qualifier` is specified.** Table 30-27 has exactly one row, and it is the only
/// key in this struct with defined semantics or any evidence behind it.
///
/// The device fields below are **[I]**, on two grounds and no more: §30.3.2.4 opens with
/// "the info message provides device information to the accessory", and a sentence later
/// says "the device may include any information it wants to provide to the accessory in the
/// request plist". No capture we have shows what iOS actually puts there — the probe in
/// `ncm-probe/session.log` is our own request, not an iPhone's — and the accessory receives
/// all of these again in `SETUP` a moment later regardless. The receiver's
/// `_requestReportIfIncompatibleSender` hints that it inspects sender identity somewhere,
/// but the pooled string table cannot say whether that happens on this path or on `SETUP`.
///
/// Practically this costs nothing to get wrong: every device field is optional and skipped
/// when absent, so [`InfoRequest::all`] serialises to an empty dictionary, which is exactly
/// the spec-minimal request. Fill them in only if a head unit turns out to want them.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoRequest {
	/// Which properties of Table 30-28 to return. Absent means all of them; **present but
	/// empty means none**, which is a trap worth remembering — use `None`, not
	/// `Some(vec![])`. Observed values include `"txtAirPlay"` and `"txtCarPlay"`. **[V]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub qualifier: Option<Vec<String>>,

	#[serde(default, rename = "deviceID", skip_serializing_if = "Option::is_none")]
	pub device_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub mac_address: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub model: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub os_build_version: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source_version: Option<String>,
	/// Must be `"1.0"` if present. **[I]**, and weaker than the rest: Table 30-24 defines
	/// `protovers` as an *accessory* Bonjour key, and the device states its own version in
	/// the `X-Apple-ProtocolVersion` header that `conn.rs` already sends on every request.
	/// A body key here is probably redundant.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub protocol_version: Option<String>,
}

impl InfoRequest {
	/// Ask for everything the accessory has.
	pub fn all() -> Self {
		Self::default()
	}

	/// Ask for a subset, by Table 30-28 key name.
	pub fn qualified(properties: &[&str]) -> Self {
		Self {
			qualifier: Some(properties.iter().map(|p| (*p).to_owned()).collect()),
			..Self::default()
		}
	}
}

/// `GET /info` response (Table 30-28).
///
/// The keys R29 marks required are still `Option` here. Head units omit things — the probed
/// VW sends no `fv`, the Kenwood does — and the useful failure is "no oem icon" rather than
/// "no CarPlay". Read the required ones with the accessors below, which name what is
/// missing.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessoryInfo {
	// --- identity ---
	/// Globally unique ID for the accessory, normally its primary MAC.
	#[serde(default, rename = "deviceID", skip_serializing_if = "Option::is_none")]
	pub device_id: Option<String>,
	/// Must be `"CarPlay"`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	/// Globally unique model name identifying the vehicle's acoustic environment.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub model: Option<String>,
	/// User-visible manufacturer name.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub manufacturer: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub firmware_revision: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub hardware_revision: Option<String>,
	/// e.g. `"Darwin 13.0.0 x86_64"`.
	#[serde(default, rename = "OSInfo", skip_serializing_if = "Option::is_none")]
	pub os_info: Option<String>,
	/// `"1.0"` if present.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub protocol_version: Option<String>,
	/// Apple-assigned CarPlay source version, `x.y.z`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source_version: Option<String>,
	/// Opaque to accessories; mirrors the Bonjour `features` key (Table 30-24).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub features: Option<i64>,
	/// Mirrors the Bonjour `flags` key. See [`status_flags`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub status_flags: Option<i64>,

	// --- what we need to drive the session ---
	/// Display information. Required, and the source of the screen stream's `uuid` and of
	/// the geometry that goes into the `VideoConfig` frame.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub displays: Option<Vec<Display>>,
	/// Supported audio formats. The first two entries must be `type` 100 and 101 with
	/// `audioType` `"compatibility"` and PCM-only formats.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub audio_formats: Option<Vec<AudioFormat>>,
	/// Fixed audio latencies inside the accessory. Later entries override earlier ones
	/// where both apply — last one wins.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub audio_latencies: Option<Vec<AudioLatency>>,
	/// HID devices, with their USB HID descriptors.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub hid_devices: Option<Vec<HidDevice>>,
	/// BCP-47 codes for the accessory's character recogniser.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub hid_languages: Option<Vec<String>>,
	/// The accessory's initial claim on the shared resources (§30.3.4.1). The device treats
	/// this as a resource transfer *to* the accessory and only then decides whether to take
	/// anything back. Observed as `{reasonStr: "InitMode", resources: [...]}`. **[V]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub modes: Option<ModeChange>,

	// --- capability flags ---
	/// Supports the UDP session idle keepalive.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub keep_alive_low_power: Option<bool>,
	/// Supports statistics in the keepalive body. Debug only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub keep_alive_send_stats_as_body: Option<bool>,
	/// See [`extended_feature`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub extended_features: Option<Vec<String>>,
	/// Whether certain UI elements are currently limited.
	#[serde(default, rename = "limitedUI", skip_serializing_if = "Option::is_none")]
	pub limited_ui: Option<bool>,
	/// Which elements limited UI affects. See [`limited_ui_element`].
	#[serde(
		default,
		rename = "limitedUIElements",
		skip_serializing_if = "Option::is_none"
	)]
	pub limited_ui_elements: Option<Vec<String>>,
	/// Dark outside. An accessory that does not support night mode must return an
	/// unsupported error rather than a value, so the device can fall back to time of day.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub night_mode: Option<bool>,
	/// True for a right-hand-drive vehicle.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub right_hand_drive: Option<bool>,
	/// Bluetooth module MACs, colon separated. Required if the accessory has Bluetooth, and
	/// a precondition for us sending `disableBluetooth`.
	#[serde(
		default,
		rename = "bluetoothIDs",
		skip_serializing_if = "Option::is_none"
	)]
	pub bluetooth_ids: Option<Vec<String>>,

	// --- branding ---
	/// Whether the OEM icon appears on the home screen.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub oem_icon_visible: Option<bool>,
	/// Label under the OEM icon.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub oem_icon_label: Option<String>,
	/// Legacy single 104x104 PNG, for devices before iOS 9.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub oem_icon: Option<Data>,
	/// Modern multi-size icon set.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub oem_icons: Option<Vec<OemIcon>>,

	/// Initial state of vehicle features.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub vehicle_information: Option<VehicleInformation>,
}

impl AccessoryInfo {
	/// The first display, which is the one CarPlay drives.
	pub fn primary_display(&self) -> Option<&Display> {
		self.displays.as_ref()?.first()
	}

	/// Whether the accessory declared an extended feature (Table 30-35).
	pub fn has_extended_feature(&self, feature: &str) -> bool {
		self.extended_features
			.as_ref()
			.is_some_and(|features| features.iter().any(|f| f == feature))
	}
}

/// One entry of `audioFormats` (Table 30-29).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioFormat {
	pub r#type: super::setup::StreamType,
	/// One of [`super::setup::audio_type`].
	pub audio_type: String,
	/// Combination of [`super::setup::audio_format`]. Optional when the stream type has no
	/// input; must be a subset of the output formats when present.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub audio_input_formats: Option<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub audio_output_formats: Option<u64>,
}

/// One entry of `audioLatencies` (Table 30-31).
///
/// `sr`, `ss` and `ch` narrow an entry to one format; leaving them out makes it the default
/// for the whole stream type and audio type.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioLatency {
	pub r#type: super::setup::StreamType,
	pub audio_type: String,
	/// Samples per second.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub sr: Option<u32>,
	/// Bits per sample.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ss: Option<u32>,
	/// Channels.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ch: Option<u32>,
	/// Required for duplex streams.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub input_latency_micros: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub output_latency_micros: Option<i64>,
}

/// One entry of `displays` (Table 30-32).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Display {
	/// Echo this into the screen stream's `SETUP` descriptor. **[I]**
	pub uuid: String,
	/// Must be non-zero. This is the width the encoder and the `VideoConfig` frame use.
	pub width_pixels: u32,
	/// Must be non-zero.
	pub height_pixels: u32,
	/// Physical width in millimetres. Must be non-zero.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub width_physical: Option<f64>,
	/// Physical height in millimetres. Must be non-zero.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub height_physical: Option<f64>,
	/// Combination of [`display_feature`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub features: Option<i64>,
	/// Which input device the UI should be laid out for.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub primary_input_device: Option<PrimaryInputDevice>,
	/// Frame rate ceiling. Absent means the device picks; captures run at 30 and 60.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub max_fps: Option<u32>,
	/// Raw EDID, if the accessory has one.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub edid: Option<Data>,
}

carplay_enum! {
	/// `primaryInputDevice` (Table 30-34). Drives the whole shape of the UI iOS renders.
	PrimaryInputDevice {
		Touchscreen = 0x01,
		Touchpad = 0x02,
		Knob = 0x03,
	}
}

/// `features` bits of a [`Display`] (Table 30-33).
///
/// A bitmask; set every input method the accessory really supports. Claiming
/// [`HIGH_FIDELITY_TOUCH`] commits the accessory to under 140 ms from touch to the frame
/// that reflects it, which is a latency budget for our encoder, not just theirs.
pub mod display_feature {
	pub const KNOB: i64 = 0x02;
	pub const LOW_FIDELITY_TOUCH: i64 = 0x04;
	pub const HIGH_FIDELITY_TOUCH: i64 = 0x08;
	pub const TOUCHPAD: i64 = 0x10;
}

/// `statusFlags` bits, shared with the Bonjour `flags` key (Table 30-25).
pub mod status_flags {
	pub const PROBLEM_DETECTED: i64 = 0x01;
	/// Set means the accessory is not configured and must not be connected to.
	pub const NOT_CONFIGURED: i64 = 0x02;
	pub const AUDIO_CABLE_ATTACHED: i64 = 0x04;
}

/// `extendedFeatures` string values (Table 30-35).
pub mod extended_feature {
	/// The accessory understands the `url` key of a device-sent `requestUI`.
	pub const ENHANCED_REQUEST_CAR_UI: &str = "enhancedRequestCarUI";
	/// The accessory wants `vocoderInfo` on the telephony stream. Required for wireless.
	pub const VOCODER_INFO: &str = "vocoderInfo";
}

/// `limitedUIElements` string values (Table 30-36).
pub mod limited_ui_element {
	pub const SOFT_KEYBOARD: &str = "softKeyboard";
	pub const SOFT_PHONE_KEYPAD: &str = "softPhoneKeypad";
	pub const NON_MUSIC_LISTS: &str = "nonMusicLists";
	pub const MUSIC_LISTS: &str = "musicLists";
	pub const JAPAN_MAPS: &str = "japanMaps";
}

/// One entry of `oemIcons` (Table 30-37).
///
/// Prerendered icons must be supplied at 120, 180 and 256 px; mask icons at 104, 156 and
/// 256 px.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OemIcon {
	pub image_data: Data,
	pub width_pixels: u32,
	pub height_pixels: u32,
	/// True for a full-bleed icon, false for a mask.
	pub prerendered: bool,
}

/// One entry of `hidDevices` (Table 30-66).
///
/// The descriptor is a USB HID report descriptor; parsing it is what turns the opaque
/// `hidReport` blobs of a `hidSendReport` command into coordinates. The probed VW head unit
/// declares report ID + X:u16 + Y:u16 with logical maxima 800 and 480. **[V]**
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidDevice {
	/// Unique within this session; quoted back in every `hidSendReport`.
	pub uuid: String,
	/// The [`Display::uuid`] this device drives, or a null UUID if it drives none.
	#[serde(rename = "displayUUID")]
	pub display_uuid: String,
	/// USB-formatted HID report descriptor.
	pub hid_descriptor: Data,
	/// USB-style HID country code.
	pub hid_country_code: u32,
	/// Must be non-zero.
	#[serde(rename = "hidProductID")]
	pub hid_product_id: u32,
	/// Must be non-zero.
	#[serde(rename = "hidVendorID")]
	pub hid_vendor_id: u32,
	/// User-friendly name.
	pub name: String,
}

/// `vehicleInformation` (Table 30-38).
///
/// Also the payload of the accessory's `updateVehicleInformation` command, so the same
/// struct serves both.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct VehicleInformation {
	/// Presence of the key means the vehicle supports Japanese Electronic Toll Collection
	/// and the ETC symbol appears in the CarPlay UI.
	#[serde(
		default,
		rename = "ElectronicTollCollection",
		skip_serializing_if = "Option::is_none"
	)]
	pub electronic_toll_collection: Option<ActiveFlag>,
	/// Presence of the key means the vehicle feeds native route guidance into its driver
	/// assistance systems. Requires Destination Information support.
	#[serde(
		default,
		rename = "NavigationAidedDriving",
		skip_serializing_if = "Option::is_none"
	)]
	pub navigation_aided_driving: Option<ActiveFlag>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub user_preferences: Option<UserPreferences>,
}

/// Tables 30-39 and 30-40, which have identical shapes.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ActiveFlag {
	pub active: bool,
}

/// `userPreferences` (Table 30-41).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPreferences {
	/// Keyed by the touchpad's [`HidDevice::uuid`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub touchpad_settings: Option<std::collections::HashMap<String, TouchpadSettings>>,
}

/// `touchpadSettings` (Table 30-42).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TouchpadSettings {
	/// 0.0 slowest to 1.0 fastest; 0.5 is the default.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub touchpad_sensitivity: Option<f64>,
	/// Combination of [`haptic_feedback_type`]. Absent means no haptics.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub supported_haptic_feedback_types: Option<i64>,
}

/// `hapticFeedbackType` bits (Table 30-43).
pub mod haptic_feedback_type {
	/// The accessory's most common haptic.
	pub const DEFAULT: i64 = 0x1;
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::carplay::net::proto::to_binary;

	/// Table 30-27 defines one key. The default request must therefore put nothing on the
	/// wire — an empty dictionary, which the accessory reads as "return everything".
	///
	/// This is the guard on the inferred device fields above: as long as they stay `Option`
	/// and skipped, being wrong about them costs nothing.
	#[test]
	fn the_default_request_is_an_empty_dictionary() {
		let value = plist::to_value(&InfoRequest::all()).unwrap();

		assert_eq!(
			value.as_dictionary().expect("a dictionary").len(),
			0,
			"no key may appear that the spec did not ask for"
		);
	}

	/// The trap in Table 30-27: a missing `qualifier` means "all properties", but a *present
	/// and empty* one means "no properties at all".
	#[test]
	fn an_empty_qualifier_is_not_the_same_as_no_qualifier() {
		let none = plist::to_value(&InfoRequest::all()).unwrap();
		assert!(none.as_dictionary().unwrap().get("qualifier").is_none());

		let empty = plist::to_value(&InfoRequest::qualified(&[])).unwrap();
		assert_eq!(
			empty.as_dictionary().unwrap()["qualifier"]
				.as_array()
				.unwrap()
				.len(),
			0
		);
	}

	#[test]
	fn a_qualified_request_carries_only_the_qualifier() {
		let value = plist::to_value(&InfoRequest::qualified(&["model", "displays"])).unwrap();
		let dict = value.as_dictionary().unwrap();

		assert_eq!(dict.len(), 1);
		assert_eq!(
			dict["qualifier"]
				.as_array()
				.unwrap()
				.iter()
				.map(|v| v.as_string().unwrap())
				.collect::<Vec<_>>(),
			["model", "displays"]
		);
	}
}
