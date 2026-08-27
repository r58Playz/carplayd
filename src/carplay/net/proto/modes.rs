//! Resource ownership and app state (§30.3.3, §30.3.4).
//!
//! Two shapes share these enums and they are easy to confuse:
//!
//! * [`ModeChange`] is a *request*. The accessory sends it as `changeModes` to ask for the
//!   screen or main audio, and it is also the shape of the `modes` group in the accessory's
//!   `/info` response, which the device reads as the accessory's initial claim on both
//!   resources (§30.3.4.1). Resources carry a transfer type and constraints, not an owner.
//! * [`Modes`] is a *statement of fact*. The device sends it as `modesChanged` to say who
//!   owns what now. Resources carry an [`Entity`] and nothing else.
//!
//! Getting them backwards produces a plist the head unit silently ignores, so the two are
//! separate types rather than one type with optional fields.

use serde::{Deserialize, Serialize};

use crate::carplay_enum;

carplay_enum! {
	/// Who owns a resource, or is in an app state (Table 30-59).
	Entity {
		/// App states only. Never valid as a resource owner.
		None = 0,
		/// The device — us.
		Controller = 1,
		/// The head unit.
		Accessory = 2,
	}
}

carplay_enum! {
	/// The shared resources (Table 30-60).
	///
	/// Alternate audio is deliberately absent: §30.3.3.1 requires it to be available to the
	/// device at all times, so it is never negotiated.
	ResourceId {
		/// The head unit's display.
		MainScreen = 1,
		/// The full-duplex audio stream used by telephony and media.
		MainAudio = 2,
	}
}

carplay_enum! {
	/// How ownership moves (Table 30-61).
	TransferType {
		/// Acquire permanently.
		Take = 1,
		/// No longer needed. Ownership changes on the next request, not now.
		Untake = 2,
		/// Acquire temporarily; the previous owner keeps its state.
		Borrow = 3,
		/// Give a borrowed resource back.
		Unborrow = 4,
	}
}

carplay_enum! {
	/// How badly the sender wants the resource (Table 30-62).
	///
	/// Compared against the current owner's constraint: the transfer succeeds when the
	/// priority is at least the constraint.
	TransferPriority {
		/// Succeeds only against an `Anytime` constraint.
		NiceToHave = 100,
		/// Succeeds against `UserInitiated` or `Anytime`. Only legitimate in response to a
		/// direct user action.
		UserInitiated = 500,
	}
}

carplay_enum! {
	/// The minimum priority a later transfer must carry to succeed (Table 30-63).
	Constraint {
		Anytime = 100,
		UserInitiated = 500,
		/// Never valid on a take of main audio, and discouraged for the screen.
		Never = 1000,
	}
}

carplay_enum! {
	/// The app states both sides must keep in sync (Table 30-64).
	AppStateId {
		/// Speaking or recognising speech. Further specified by [`SpeechMode`].
		Speech = 1,
		/// On a phone call. Only one entity at a time.
		PhoneCall = 2,
		/// Running turn-by-turn navigation. Only one entity at a time.
		TurnByTurn = 3,
	}
}

carplay_enum! {
	/// Detail for [`AppStateId::Speech`] (Table 30-65).
	SpeechMode {
		/// Note the negative value; this is why the plist integers are signed.
		None = -1,
		/// Speaking to the user. Includes a connected call once ringing has stopped.
		Speaking = 1,
		RecognizingSpeech = 2,
	}
}

/// A request to change ownership or app state.
///
/// Sent by the accessory as `changeModes` (Table 30-76), and returned by the accessory as
/// the `modes` group of `/info` to declare its initial claim (§30.3.4.1). Observed on a
/// real dongle as `{reasonStr: "InitMode", resources: [...]}` with no `appStates`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeChange {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub app_states: Option<Vec<AppStateChange>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub resources: Option<Vec<ResourceChange>>,
	/// Free text describing why, for logs on the other side.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reason_str: Option<String>,
}

/// One app state in a [`ModeChange`] (Table 30-77).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStateChange {
	#[serde(rename = "appStateID")]
	pub app_state_id: AppStateId,
	/// Required when `app_state_id` is [`AppStateId::Speech`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub speech_mode: Option<SpeechMode>,
	/// Required when `app_state_id` is [`AppStateId::PhoneCall`] or
	/// [`AppStateId::TurnByTurn`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub state: Option<bool>,
}

/// One resource in a [`ModeChange`] (Table 30-78).
///
/// Which constraints are required depends on the transfer type: `Take` needs
/// `take_constraint` and `borrow_constraint`, `Borrow` needs `unborrow_constraint`, and
/// `Untake`/`Unborrow` need none. Note that the spec's own table says `borrowConstraint` is
/// required "if transferType is take" while `unborrowConstraint` says the same thing — the
/// latter is a typo for borrow, per §30.3.4.2.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceChange {
	#[serde(rename = "resourceID")]
	pub resource_id: ResourceId,
	pub transfer_type: TransferType,
	/// Required for `Take` and `Borrow`; must be absent for `Untake` and `Unborrow`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub transfer_priority: Option<TransferPriority>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub take_constraint: Option<Constraint>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub borrow_constraint: Option<Constraint>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub unborrow_constraint: Option<Constraint>,
}

/// The current owner of each resource and app state.
///
/// The device sends this as `modesChanged` (Table 30-80) after every ownership change, and
/// the accessory must honour it within 100 ms (§30.3.4.2). The accessory also gets it back
/// as the `params` of a successful `changeModes` response.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Modes {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub app_states: Option<Vec<AppState>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub resources: Option<Vec<Resource>>,
}

impl Modes {
	/// The announcement that puts CarPlay on the screen while leaving audio with the head
	/// unit, and declares no app state active.
	///
	/// This is byte-for-byte the `modesChanged` a Carlinkit dongle sends immediately before
	/// it sets up the screen stream, and is the minimum a head unit needs before it will
	/// switch its display over. **[V]**
	pub fn screen_to_controller() -> Self {
		Self {
			app_states: Some(vec![
				AppState::none(AppStateId::PhoneCall),
				AppState {
					app_state_id: AppStateId::Speech,
					entity: Entity::None,
					speech_mode: Some(SpeechMode::None),
				},
				AppState::none(AppStateId::TurnByTurn),
			]),
			resources: Some(vec![
				Resource {
					resource_id: ResourceId::MainScreen,
					entity: Entity::Controller,
				},
				Resource {
					resource_id: ResourceId::MainAudio,
					entity: Entity::Accessory,
				},
			]),
		}
	}
}

/// One app state in a [`Modes`] (Table 30-81).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
	#[serde(rename = "appStateID")]
	pub app_state_id: AppStateId,
	pub entity: Entity,
	/// Required when `app_state_id` is [`AppStateId::Speech`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub speech_mode: Option<SpeechMode>,
}

impl AppState {
	/// Nobody is in this state.
	pub fn none(app_state_id: AppStateId) -> Self {
		Self {
			app_state_id,
			entity: Entity::None,
			speech_mode: None,
		}
	}
}

/// One resource in a [`Modes`] (Table 30-82).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
	#[serde(rename = "resourceID")]
	pub resource_id: ResourceId,
	/// Must not be [`Entity::None`]: a resource always has an owner.
	pub entity: Entity,
}
