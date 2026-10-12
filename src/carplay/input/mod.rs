use std::{
	collections::{BTreeSet, HashMap},
	hash::Hash,
	time::Instant,
};

mod hid;
pub use hid::HidParser;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum InputKey {
	/// 0x0C 0xB0 Play
	Play,
	/// 0x0C 0xB1 Pause
	Pause,
	/// 0x0C 0xB5 Scan Next Track
	NextTrack,
	/// 0x0C 0xB6 Scan Previous Track
	PreviousTrack,
	/// 0x0C 0xB9 Random Play
	Shuffle,
	/// 0x0C 0xBC Repeat
	Repeat,
	/// 0x0C 0xBE Tracking Normal
	TrackingNormal,
	/// 0x0C 0xCA Tracking Increment
	TrackingIncrement,
	/// 0x0C 0xCB Tracking Decrement
	TrackingDecrement,
	/// 0x0C 0xCD Play/Pause
	PlayPause,
	/// 0x0C 0xE2 Mute
	Mute,
	/// 0x0C 0xE9 Volume Increment
	VolumeUp,
	/// 0x0C 0xEA Volume Decrement
	VolumeDown,
	/// 0x0C 0x25B Promote
	Promote,
	/// 0x0C 0x25C Demote
	Demote,
	/// 0x0C 0x262 Add to Cart
	AddToCart,
	/// 0x0C 0x223 AC Home
	Home,
	/// 0x0C 0x224 AC Back
	Back,
	/// 0x0C 0x29E AC Navigation Guidance
	NavGuidance,
	/// 0x07 0x2A Keyboard Delete
	Backspace,
	/// 0x0B 0x20 Hook Switch
	HookSwitch,
	/// 0x0B 0x21 Flash
	Flash,
	/// 0x0B 0x26 Drop
	Drop,
	/// 0x0B 0x2F Phone Mute
	PhoneMute,
	/// 0x0B 0xB0 Phone Key 0
	PhoneKey0,
	/// 0x0B 0xB1 Phone Key 1
	PhoneKey1,
	/// 0x0B 0xB2 Phone Key 2
	PhoneKey2,
	/// 0x0B 0xB3 Phone Key 3
	PhoneKey3,
	/// 0x0B 0xB4 Phone Key 4
	PhoneKey4,
	/// 0x0B 0xB5 Phone Key 5
	PhoneKey5,
	/// 0x0B 0xB6 Phone Key 6
	PhoneKey6,
	/// 0x0B 0xB7 Phone Key 7
	PhoneKey7,
	/// 0x0B 0xB8 Phone Key 8
	PhoneKey8,
	/// 0x0B 0xB9 Phone Key 9
	PhoneKey9,
	/// 0x0B 0xBA Phone Key Star
	PhoneKeyStar,
	/// 0x0B 0xBB Phone Key Pound
	PhoneKeyPound,
	/// 0x09 0x01 Button 1
	Primary,
}

impl InputKey {
	pub(crate) fn from_usage(usage: u32) -> Option<Self> {
		Some(match usage {
			0x0c00b0 => Self::Play,
			0x0c00b1 => Self::Pause,
			0x0c00b5 => Self::NextTrack,
			0x0c00b6 => Self::PreviousTrack,
			0x0c00b9 => Self::Shuffle,
			0x0c00bc => Self::Repeat,
			0x0c00be => Self::TrackingNormal,
			0x0c00ca => Self::TrackingIncrement,
			0x0c00cb => Self::TrackingDecrement,
			0x0c00cd => Self::PlayPause,
			0x0c00e2 => Self::Mute,
			0x0c00e9 => Self::VolumeUp,
			0x0c00ea => Self::VolumeDown,
			0x0c025b => Self::Promote,
			0x0c025c => Self::Demote,
			0x0c0262 => Self::AddToCart,
			0x0c0223 => Self::Home,
			0x0c0224 => Self::Back,
			0x0c029e => Self::NavGuidance,
			0x07002a => Self::Backspace,
			0x0b0020 => Self::HookSwitch,
			0x0b0021 => Self::Flash,
			0x0b0026 => Self::Drop,
			0x0b002f => Self::PhoneMute,
			0x0b00b0 => Self::PhoneKey0,
			0x0b00b1 => Self::PhoneKey1,
			0x0b00b2 => Self::PhoneKey2,
			0x0b00b3 => Self::PhoneKey3,
			0x0b00b4 => Self::PhoneKey4,
			0x0b00b5 => Self::PhoneKey5,
			0x0b00b6 => Self::PhoneKey6,
			0x0b00b7 => Self::PhoneKey7,
			0x0b00b8 => Self::PhoneKey8,
			0x0b00b9 => Self::PhoneKey9,
			0x0b00ba => Self::PhoneKeyStar,
			0x0b00bb => Self::PhoneKeyPound,
			0x090001 => Self::Primary,
			_ => return None,
		})
	}
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TouchContact {
	pub id: u32,
	pub x: f32, // normalized 0..=1 within the surface
	pub y: f32,
	pub touching: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InputAxis {
	X,
	Y,
	Rz,
	Dial,
	Wheel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
	Key {
		key: InputKey,
		pressed: bool,
	},
	/// Changed contacts; `touching: false` explicitly releases a contact.
	TouchEvent {
		contacts: Vec<TouchContact>,
	},
	/// Absolute position or relative delta, as described by the axis capability.
	Axis {
		id: u32,
		value: i64,
	},
}

#[derive(Clone, Debug)]
pub struct InputReport {
	pub time: Instant,
	pub events: Vec<InputEvent>,
}
impl InputReport {
	pub fn is_empty(&self) -> bool {
		self.events.is_empty()
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum InputDevice {
	/// All media remote components, including ones started after provider setup.
	IapRemote,
	Network {
		id: String,
		/// Applies to the whole HID entry, rather than an individual collection.
		associated_display: Option<String>,
		keys: Vec<InputKey>,
		touch: Option<InputTouchCapability>,
		axes: Vec<InputAxisCapability>,
	},
}
impl InputDevice {
	pub fn supported_keys(&self) -> &[InputKey] {
		use InputKey as I;
		match self {
			Self::IapRemote => &[
				I::Play,
				I::Pause,
				I::NextTrack,
				I::PreviousTrack,
				I::Shuffle,
				I::Repeat,
				I::TrackingNormal,
				I::TrackingIncrement,
				I::TrackingDecrement,
				I::PlayPause,
				I::Mute,
				I::VolumeUp,
				I::VolumeDown,
				I::Promote,
				I::Demote,
				I::AddToCart,
			],
			Self::Network { keys, .. } => keys,
		}
	}
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InputTouchCapability {
	Touchscreen(InputTouchInfo),
	Touchpad(InputTouchInfo),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputTouchInfo {
	pub max_touches: u32,
	/// Physical surface dimensions, independent of logical coordinates and pixels.
	pub physical_size_mm: Option<[f32; 2]>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputAxisCapability {
	/// Identifies a field within this device, including separate knob collections.
	pub id: u32,
	pub axis: InputAxis,
	pub relative: bool,
	pub logical_minimum: i64,
	pub logical_maximum: i64,
}

/// Combines held state without releasing a key still held by another source.
/// Sources are report IDs in a parser and component IDs in the USB policy.
pub(crate) struct KeyMerger<S> {
	pressed: HashMap<S, BTreeSet<InputKey>>,
}
impl<S> Default for KeyMerger<S> {
	fn default() -> Self {
		Self {
			pressed: HashMap::new(),
		}
	}
}
impl<S: Eq + Hash> KeyMerger<S> {
	fn held(&self) -> BTreeSet<InputKey> {
		self.pressed.values().flatten().copied().collect()
	}

	fn changes(old: &BTreeSet<InputKey>, new: &BTreeSet<InputKey>) -> Vec<InputEvent> {
		old.difference(new)
			.map(|&key| InputEvent::Key {
				key,
				pressed: false,
			})
			.chain(
				new.difference(old)
					.map(|&key| InputEvent::Key { key, pressed: true }),
			)
			.collect()
	}

	pub(crate) fn replace(&mut self, source: S, keys: BTreeSet<InputKey>) -> Vec<InputEvent> {
		let old = self.held();
		if keys.is_empty() {
			self.pressed.remove(&source);
		} else {
			self.pressed.insert(source, keys);
		}
		Self::changes(&old, &self.held())
	}

	pub(crate) fn apply(
		&mut self,
		source: S,
		changes: impl Iterator<Item = (InputKey, bool)>,
	) -> Vec<InputEvent> {
		let mut keys = self.pressed.get(&source).cloned().unwrap_or_default();
		for (key, pressed) in changes {
			if pressed {
				keys.insert(key);
			} else {
				keys.remove(&key);
			}
		}
		self.replace(source, keys)
	}

	pub(crate) fn remove(&mut self, source: &S) -> Vec<InputEvent> {
		let old = self.held();
		self.pressed.remove(source);
		Self::changes(&old, &self.held())
	}

	pub(crate) fn flush(&mut self) -> Vec<InputEvent> {
		let old = self.held();
		self.pressed.clear();
		Self::changes(&old, &BTreeSet::new())
	}
}
