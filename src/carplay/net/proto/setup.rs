//! `SETUP` (§30.3.2.5) — the only message that creates ports.
//!
//! There are two rounds, and they use different request types:
//!
//! 1. The **initial** setup ([`SetupRequest`], Table 30-44) is sent once, right after
//!    `/auth-setup`, and carries the device's identity plus the UDP port our NTP server is
//!    already bound to. The accessory answers with [`SetupResponse`] (Table 30-45): the
//!    TCP port we connect out to for the event channel, and its own timing port.
//! 2. Every **later** setup ([`StreamSetupRequest`], Table 30-46) carries only `streams`,
//!    and is what actually opens the screen or an audio stream. The accessory answers with
//!    the port it is now listening on, and *the device* makes the connection (§30.3.2, step
//!    5) — for both input and output streams.
//!
//! The initial request may carry `streams` too, but no real iPhone capture does; they all
//! set up control first and the screen a second later, after `modesChanged`.

use serde::{Deserialize, Serialize};

use crate::carplay_enum;

carplay_enum! {
	/// Stream type (Table 30-53). The value doubles as the RTP payload type.
	StreamType {
		Invalid = 0,
		/// Low-latency full-duplex audio. UDP.
		MainAudio = 100,
		/// Low-latency output-only audio: UI sounds, alerts, navigation prompts. UDP.
		AltAudio = 101,
		/// High-latency output-only audio: entertainment. UDP.
		MainHighAudio = 102,
		/// H.264 screen output. TCP over NCM.
		Screen = 110,
	}
}

impl Default for StreamType {
	fn default() -> Self {
		Self::Invalid
	}
}

/// `audioType` string values (Table 30-54).
///
/// A string rather than an enum because the first two `/info` entries must use
/// `"compatibility"`, which R29 does not list but every shipping head unit sends. **[V]**
pub mod audio_type {
	/// Unspecified. Main and alternate audio.
	pub const DEFAULT: &str = "default";
	/// Ringtones and alarms. Main audio.
	pub const ALERT: &str = "alert";
	/// Music playback. Main audio and main high audio.
	pub const MEDIA: &str = "media";
	/// Telephony. Main audio.
	pub const TELEPHONY: &str = "telephony";
	/// Siri and voice control. Main audio.
	pub const SPEECH_RECOGNITION: &str = "speechRecognition";
	/// Undocumented, but required as the first two `/info` `audioFormats` entries so that
	/// iOS versions predating the `audioType` key still work. **[V]**
	pub const COMPATIBILITY: &str = "compatibility";
}

/// `audioFormat` bitfield values (Table 30-30).
///
/// PCM formats are for CarPlay over USB; the compressed formats are wireless only, and
/// AAC-ELD and OPUS must never be offered together.
pub mod audio_format {
	pub const PCM_8000_16_MONO: u64 = 0x0000_0004;
	pub const PCM_8000_16_STEREO: u64 = 0x0000_0008;
	pub const PCM_16000_16_MONO: u64 = 0x0000_0010;
	pub const PCM_16000_16_STEREO: u64 = 0x0000_0020;
	pub const PCM_24000_16_MONO: u64 = 0x0000_0040;
	pub const PCM_24000_16_STEREO: u64 = 0x0000_0080;
	pub const PCM_32000_16_MONO: u64 = 0x0000_0100;
	pub const PCM_32000_16_STEREO: u64 = 0x0000_0200;
	pub const PCM_44100_16_MONO: u64 = 0x0000_0400;
	pub const PCM_44100_16_STEREO: u64 = 0x0000_0800;
	pub const PCM_48000_16_MONO: u64 = 0x0000_4000;
	pub const PCM_48000_16_STEREO: u64 = 0x0000_8000;
	pub const AAC_LC_44100_STEREO: u64 = 0x0040_0000;
	pub const AAC_LC_48000_STEREO: u64 = 0x0080_0000;
	pub const AAC_ELD_44100_STEREO: u64 = 0x0100_0000;
	pub const AAC_ELD_48000_STEREO: u64 = 0x0200_0000;
	pub const AAC_ELD_16000_MONO: u64 = 0x0400_0000;
	pub const AAC_ELD_24000_MONO: u64 = 0x0800_0000;
	pub const OPUS_16000_MONO: u64 = 0x1000_0000;
	pub const OPUS_24000_MONO: u64 = 0x2000_0000;
	pub const OPUS_48000_MONO: u64 = 0x4000_0000;
	pub const AAC_ELD_44100_MONO: u64 = 0x8000_0000;
	pub const AAC_ELD_48000_MONO: u64 = 0x1_0000_0000;
}

/// The initial `SETUP` request (Table 30-44).
///
/// Every field the spec marks required is non-optional here. The two that are not in R29 at
/// all — `features` and `keepAliveLowPower` — come from real iPhone captures. **[V]**
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupRequest {
	/// Globally unique device ID: a lower-case colon-separated MAC, `"4c:74:bf:21:3f:0d"`.
	#[serde(rename = "deviceID")]
	pub device_id: String,
	/// MAC of the interface carrying this session — the NCM link. Real iPhones send this
	/// upper-case while `deviceID` is lower-case. **[V]**
	pub mac_address: String,
	/// e.g. `"iPhone9,1"`.
	pub model: String,
	/// The user-visible device name.
	pub name: String,
	/// e.g. `"18F72"`.
	pub os_build_version: String,
	/// UUID of this session, upper-case with hyphens.
	#[serde(rename = "sessionUUID")]
	pub session_uuid: String,
	/// CarPlay source version, e.g. `"545.22.21"` (iOS 14.6) or `"600.8.1"` (iOS 15.2).
	pub source_version: String,
	/// The UDP port **we** are listening on for time sync. Bind the NTP server first.
	pub timing_port: u16,
	/// Optional on the initial setup, and absent in every capture.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streams: Option<Vec<StreamRequest>>,
	/// Not in R29. Observed verbatim as
	/// `["uiContext", "viewAreas", "cornerMasks", "focusTransfer"]`. **[V]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub features: Option<Vec<String>>,
	/// Whether we support the UDP idle keepalive (§30.3.2.9).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[serde(deserialize_with = "crate::carplay::net::proto::flexible::option_boolean")]
	pub keep_alive_low_power: Option<bool>,
}

/// The initial `SETUP` response (Table 30-45).
///
/// `keepAlivePort` is marked required by the spec but is absent from every capture, so it
/// is optional here.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupResponse {
	/// TCP port to connect **out** to for the event channel, on which we then serve the
	/// accessory's requests.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub event_port: Option<u16>,
	/// UDP port for the session idle keepalive.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub keep_alive_port: Option<u16>,
	/// The *accessory's* timing port. Table 30-45 describes this as "port the device is
	/// listening on", which contradicts Table 30-44; the capture agrees with the accessory
	/// reporting its own, so the spec text looks like a copy-paste error. **[V]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timing_port: Option<u16>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streams: Option<Vec<StreamResponse>>,
}

/// A `SETUP` that only opens streams (Table 30-46).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSetupRequest {
	pub streams: Vec<StreamRequest>,
}

/// The response to a [`StreamSetupRequest`] (Table 30-47).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSetupResponse {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streams: Option<Vec<StreamResponse>>,
	/// Sometimes present alongside `streams` even on a stream-only setup. **[V]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timing_port: Option<u16>,
}

/// One stream descriptor in a request (Tables 30-48 and 30-51).
///
/// The audio and screen descriptors share `type` and `streamConnectionID` and are otherwise
/// disjoint, so this is one struct with optional halves rather than an enum — which is also
/// how they sit in the plist, as a heterogeneous array. Prefer [`StreamRequest::screen`]
/// and [`StreamRequest::main_audio`] over building one by hand.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamRequest {
	pub r#type: StreamType,
	/// Salts this stream's media keys. See [`StreamRequest::connection_id`] before you
	/// touch this field.
	#[serde(rename = "streamConnectionID")]
	pub stream_connection_id: i64,

	// --- screen (Table 30-51) ---
	/// Desired screen latency. Absent on iOS 14.6, `75` on iOS 15.2. **[V]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub latency_ms: Option<u32>,
	/// Not in R29: the `uuid` of the display this stream targets, echoed from the
	/// accessory's `/info` `displays[]`. **[I]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub uuid: Option<String>,
	/// Not in R29: the `supportsHighAccuracyTimestamps` feature. Real iPhones send five
	/// entries named `SubSu`, `BePxT`, `AfPxT`, `BefEn`, `EmEnc`. **[V]**
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timestamp_info: Option<Vec<TimestampInfo>>,

	// --- audio (Table 30-48) ---
	/// Combination of [`audio_format`] values.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub audio_format: Option<u64>,
	/// Sizes the accessory's jitter buffer. The accessory must not modify it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub audio_latency_ms: Option<u32>,
	/// One of [`audio_type`]. Main audio only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub audio_type: Option<String>,
	/// Debug only: loop output back as input.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[serde(deserialize_with = "crate::carplay::net::proto::flexible::option_boolean")]
	pub audio_loopback: Option<bool>,
	/// Enable audio input. Main audio only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[serde(deserialize_with = "crate::carplay::net::proto::flexible::option_boolean")]
	pub input: Option<bool>,
	/// UDP port we listen on for RTCP. Main high audio only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub control_port: Option<u16>,
	/// UDP port we listen on for data. Required when audio input is enabled.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data_port: Option<u16>,
	/// Samples per frame; a stereo pair counts as one sample. Main high audio only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub spf: Option<u32>,
	/// Native sample rate of the telephony output stream. Only when the accessory declared
	/// the `vocoderInfo` extended feature.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub vocoder_info: Option<VocoderInfo>,
}

/// One stream named in a `TEARDOWN` body.
///
/// Only the type is needed: §30.3.2 step 9 has the device "end one or more streams on the
/// accessory", and there is at most one stream of each type in a session.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamTeardown {
	pub r#type: StreamType,
}

/// `TEARDOWN` (§30.3.2, steps 9 and 10).
///
/// With a `streams` array it ends those streams and leaves the session up; with no body at
/// all it ends the whole session. Skipping it does not merely leak: the accessory keeps the
/// stream it set up, and the *next* screen `SETUP` — when the user comes back to CarPlay —
/// is answered `400 Bad Request`. Closing our own TCP socket tells it nothing.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeardownRequest {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streams: Option<Vec<StreamTeardown>>,
}

impl TeardownRequest {
	/// End the screen stream, leaving the session running.
	pub fn screen() -> Self {
		Self {
			streams: Some(vec![StreamTeardown {
				r#type: StreamType::Screen,
			}]),
		}
	}
}

impl StreamRequest {
	/// A screen stream (Table 30-51).
	pub fn screen(stream_connection_id: i64, latency_ms: u32) -> Self {
		Self {
			r#type: StreamType::Screen,
			stream_connection_id,
			latency_ms: Some(latency_ms),
			..Default::default()
		}
	}

	/// A main audio stream (Table 30-48).
	pub fn main_audio(
		stream_connection_id: i64,
		audio_format: u64,
		audio_latency_ms: u32,
		audio_type: &str,
	) -> Self {
		Self {
			r#type: StreamType::MainAudio,
			stream_connection_id,
			audio_format: Some(audio_format),
			audio_latency_ms: Some(audio_latency_ms),
			audio_type: Some(audio_type.to_owned()),
			..Default::default()
		}
	}

	/// The connection ID as the key derivation wants it.
	///
	/// The salt is built with `asprintf("%s%llu", "DataStream-Salt", id)` — **unsigned** —
	/// while iOS puts a signed value in the plist that is very often negative. The cast
	/// below is the whole fix: reinterpret the bits, do not clamp or negate. Feed the
	/// result to `SessionKeys::data_stream`. **[V]**
	pub fn connection_id(&self) -> u64 {
		self.stream_connection_id as u64
	}
}

/// One stream descriptor in a response (Tables 30-50 and 30-52).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamResponse {
	pub r#type: StreamType,
	/// The port the accessory is now listening on. TCP for the screen, UDP for audio.
	/// **We** connect to it, in both directions.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data_port: Option<u16>,
	/// UDP port the accessory listens on for RTCP. Main high audio only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub control_port: Option<u16>,
	/// Echoed by some head units, absent on others.
	#[serde(
		default,
		rename = "streamConnectionID",
		skip_serializing_if = "Option::is_none"
	)]
	pub stream_connection_id: Option<i64>,
	/// Media timestamp sampled at the same instant as `timestamp`. Audio only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub sample_time: Option<f64>,
	/// Synchronised wall clock at the same instant as `sample_time`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timestamp: Option<f64>,
	/// The same instant on the accessory's raw, undisciplined clock.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timestamp_raw_ns: Option<i64>,
}

/// One entry of the undocumented `timestampInfo` array. **[V]**
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimestampInfo {
	pub name: String,
}

impl TimestampInfo {
	/// The five names a real iPhone sends, in order. **[V]**
	pub fn defaults() -> Vec<Self> {
		["SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc"]
			.into_iter()
			.map(|name| Self {
				name: name.to_owned(),
			})
			.collect()
	}
}

/// Telephony vocoder information (Table 30-49).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VocoderInfo {
	/// Native sample rate of the telephony output stream.
	pub sample_rate: u32,
}
