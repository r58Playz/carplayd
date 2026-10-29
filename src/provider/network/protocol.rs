use crate::provider::{
	audio::{AudioRequest, AudioSpeech, AudioStatus},
	frame::FrameCrop,
};
use serde::{Deserialize, Serialize};

use crate::carplay::{
	input::{InputDevice, InputEvent},
	media::NowPlaying,
};

/// Control messages sent by carplayd, with an explicit top-level discriminator.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderSentMessage {
	/// Sent when first connecting to the control socket.
	Init {
		display_width: u32,
		display_height: u32,
		display_fps: u32,
		input_devices: Vec<InputDevice>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		audio: Option<AudioCapabilities>,
	},
	/// The client sends DisplayResponse on control, then connects to this port.
	DisplayRequest {
		port: u16,
	},
	AudioRequest {
		id: u64,
		port: u16,
	},
	AudioStatus {
		status: AudioStatus,
	},
	/// Forward input; no response is needed.
	InputEvent {
		device: InputDevice,
		events: Vec<InputEvent>,
	},
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderReceivedMessage {
	DisplayResponse {
		// Required even when null: the response is the display handshake.
		#[serde(deserialize_with = "Option::deserialize")]
		safe_area: Option<FrameCrop>,
	},
	NowPlayingUpdate {
		now_playing: Option<NowPlaying>,
	},
	AudioStart {
		request: AudioRequest,
	},
	AudioStop {
		id: u64,
	},
	AudioFlush {
		id: u64,
	},
	AudioDuck {
		id: u64,
		duration_ms: u32,
		volume: f64,
	},
	AudioUnduck {
		id: u64,
		duration_ms: u32,
	},
	AudioAppState {
		speech: AudioSpeech,
		phone_call: bool,
		turn_by_turn: bool,
	},
	AudioInputProgress {
		id: u64,
		consumed: u64,
	},
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AudioCapabilities {
	pub sample_types: [crate::provider::audio::SampleType; 2],
	pub min_sample_rate: u32,
	pub max_sample_rate: u32,
	pub max_channels: u8,
}
impl Default for AudioCapabilities {
	fn default() -> Self {
		Self {
			sample_types: [
				crate::provider::audio::SampleType::S16Le,
				crate::provider::audio::SampleType::F32Le,
			],
			min_sample_rate: 8000,
			max_sample_rate: 96000,
			max_channels: 2,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::carplay::input::InputKey;
	use serde_json::json;

	#[test]
	fn display_response_requires_an_explicit_nullable_crop() {
		for crop in [
			serde_json::Value::Null,
			json!({"left": 100, "top": 20, "width": 1080, "height": 678}),
		] {
			let value = json!({"type": "display_response", "safe_area": crop});
			let decoded: ProviderReceivedMessage = serde_json::from_value(value.clone()).unwrap();
			assert_eq!(serde_json::to_value(decoded).unwrap(), value);
		}
		for value in [
			json!({"type": "display_response"}),
			json!({"type": "display_response", "safe_area": {"left": -2}}),
		] {
			assert!(serde_json::from_value::<ProviderReceivedMessage>(value).is_err());
		}
	}

	#[test]
	fn messages_have_flat_payloads_and_round_trip_with_explicit_types() {
		let cases = [
			(
				ProviderSentMessage::Init {
					display_width: 800,
					display_height: 480,
					display_fps: 30,
					input_devices: vec![InputDevice::IapRemote],
					audio: None,
				},
				json!({"type": "init", "display_width": 800, "display_height": 480,
					"display_fps": 30, "input_devices": ["IapRemote"]}),
			),
			(
				ProviderSentMessage::DisplayRequest { port: 6768 },
				json!({"type": "display_request", "port": 6768}),
			),
			(
				ProviderSentMessage::InputEvent {
					device: InputDevice::IapRemote,
					events: vec![InputEvent::Key {
						key: InputKey::PlayPause,
						pressed: true,
					}],
				},
				json!({"type": "input_event", "device": "IapRemote",
					"events": [{"Key": {"key": "PlayPause", "pressed": true}}]}),
			),
		];
		for (message, expected) in cases {
			assert_eq!(serde_json::to_value(&message).unwrap(), expected);
			let decoded: ProviderSentMessage = serde_json::from_value(expected.clone()).unwrap();
			assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
		}
	}

	#[test]
	fn missing_or_unknown_message_types_are_rejected() {
		for message in [
			json!({"port": 6768}),
			json!({"type": "unknown", "port": 6768}),
		] {
			assert!(serde_json::from_value::<ProviderSentMessage>(message).is_err());
		}
	}

	#[test]
	fn now_playing_snapshots_and_missing_sources_round_trip() {
		for now_playing in [
			json!({"media_item": {"title": "Track", "artist": "Artist"},
				"status": "Paused", "elapsed_time": 12000}),
			serde_json::Value::Null,
		] {
			let expected = json!({"type": "now_playing_update", "now_playing": now_playing});
			let message: ProviderReceivedMessage = serde_json::from_value(expected).unwrap();
			let encoded = serde_json::to_value(message).unwrap();
			assert_eq!(encoded["type"], "now_playing_update");
			if now_playing.is_null() {
				assert!(encoded["now_playing"].is_null());
			} else {
				assert_eq!(encoded["now_playing"]["media_item"]["title"], "Track");
				assert_eq!(encoded["now_playing"]["status"], "Paused");
				assert_eq!(encoded["now_playing"]["elapsed_time"], 12000);
			}
			serde_json::from_value::<ProviderReceivedMessage>(encoded).unwrap();
		}
	}
}
