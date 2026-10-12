use serde::{Deserialize, Serialize};

use crate::carplay::input::{InputDevice, InputEvent};

/// Control messages sent by carplayd, with an explicit top-level discriminator.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderMessage {
	/// Sent when first connecting to the control socket.
	Init {
		display_width: u32,
		display_height: u32,
		display_fps: u32,
		input_devices: Vec<InputDevice>,
	},
	/// Request a screen stream; the client responds by connecting to this port.
	DisplayRequest { port: u16 },
	/// Forward input; no response is needed.
	InputEvent {
		device: InputDevice,
		events: Vec<InputEvent>,
	},
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::carplay::input::InputKey;
	use serde_json::json;

	#[test]
	fn messages_have_flat_payloads_and_round_trip_with_explicit_types() {
		let cases = [
			(
				ProviderMessage::Init {
					display_width: 800,
					display_height: 480,
					display_fps: 30,
					input_devices: vec![InputDevice::IapRemote],
				},
				json!({"type": "init", "display_width": 800, "display_height": 480,
					"display_fps": 30, "input_devices": ["IapRemote"]}),
			),
			(
				ProviderMessage::DisplayRequest { port: 6768 },
				json!({"type": "display_request", "port": 6768}),
			),
			(
				ProviderMessage::InputEvent {
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
			let decoded: ProviderMessage = serde_json::from_value(expected.clone()).unwrap();
			assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
		}
	}

	#[test]
	fn missing_or_unknown_message_types_are_rejected() {
		for message in [
			json!({"port": 6768}),
			json!({"type": "unknown", "port": 6768}),
		] {
			assert!(serde_json::from_value::<ProviderMessage>(message).is_err());
		}
	}
}
