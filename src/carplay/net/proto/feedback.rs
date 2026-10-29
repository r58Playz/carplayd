//! `POST /feedback` (§30.3.2.6) — media clock sync, and the control-channel keepalive.
//!
//! The device posts, the accessory answers with its timing. Both bodies may be empty when
//! neither side has anything to report, which is what makes this usable as the keepalive:
//! §30.3.2.9 says that while a screen or audio stream is active, feedback messages *are*
//! the liveness signal on the control channel.
//!
//! Everything in the response is about audio. Each entry pairs a media timestamp with the
//! wall clock reading at the same instant, which lets us compute the accessory's real
//! consumption rate and produce audio at exactly that rate rather than resampling. A screen
//! stream reports no feedback here — its keepalive rides the screen data connection instead.

use serde::{Deserialize, Serialize};

use crate::carplay::net::proto::setup::StreamType;

/// The body we post. R29 defines no keys for this direction, so it is normally empty.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackRequest {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streams: Option<Vec<FeedbackStream>>,
}

/// The accessory's answer (Table 30-56).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackResponse {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streams: Option<Vec<FeedbackStream>>,
}

/// One stream's timing report (Table 30-57).
///
/// The table marks everything but `sr` required, but real head units have been seen sending
/// just `type` and `sr`, with no connection ID and no clock pair. Only `type` is enforced.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackStream {
	pub r#type: StreamType,
	#[serde(
		default,
		rename = "streamConnectionID",
		skip_serializing_if = "Option::is_none"
	)]
	pub stream_connection_id: Option<i64>,
	/// Media timestamp at the same instant as `timestamp`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub sample_time: Option<f64>,
	/// Synchronised wall clock — our clock, as the accessory has estimated it — at the same
	/// instant as `sample_time`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timestamp: Option<f64>,
	/// The same instant on the accessory's own undisciplined clock.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timestamp_raw_ns: Option<i64>,
	/// Estimated consumption rate in samples per second, derived from the pair above.
	/// Required for audio streams.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub sr: Option<f64>,
}

#[cfg(test)]
mod tests {
	use plist::Value;

	use super::*;
	use crate::carplay::net::proto::from_value;

	/// What a real head unit answered: just `type` and `sr`, no connection ID or clock pair.
	#[test]
	fn a_rate_only_response_parses() {
		let mut stream = plist::Dictionary::new();
		stream.insert("type".into(), Value::Integer(100.into()));
		stream.insert("sr".into(), Value::Real(48000.0));

		let mut dict = plist::Dictionary::new();
		dict.insert(
			"streams".into(),
			Value::Array(vec![Value::Dictionary(stream)]),
		);

		let response: FeedbackResponse = from_value(&Value::Dictionary(dict)).unwrap();
		let streams = response.streams.unwrap();

		assert_eq!(streams[0].r#type, StreamType::MainAudio);
		assert_eq!(streams[0].sr, Some(48000.0));
		assert_eq!(streams[0].stream_connection_id, None);
		assert_eq!(streams[0].timestamp, None);
	}
}
