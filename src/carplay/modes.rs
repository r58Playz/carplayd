use anyhow::{Context, Result, bail};
use log::warn;

use crate::carplay::net::proto::modes::{
	AppState, AppStateId, Constraint, Entity, ModeChange, Modes, Resource, ResourceChange,
	ResourceId, SpeechMode, TransferPriority, TransferType,
};

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ModesEntity {
	Controller,
	Accessory,
}
impl From<ModesEntity> for Entity {
	fn from(value: ModesEntity) -> Self {
		match value {
			ModesEntity::Controller => Entity::Controller,
			ModesEntity::Accessory => Entity::Accessory,
		}
	}
}
impl From<Option<ModesEntity>> for Entity {
	fn from(value: Option<ModesEntity>) -> Self {
		value.map(|x| x.into()).unwrap_or(Entity::None)
	}
}
impl TryFrom<Entity> for Option<ModesEntity> {
	type Error = anyhow::Error;

	fn try_from(value: Entity) -> Result<Self, Self::Error> {
		Ok(match value {
			Entity::None => None,
			Entity::Controller => Some(ModesEntity::Controller),
			Entity::Accessory => Some(ModesEntity::Accessory),
			Entity::Other(x) => bail!("invalid entity {x}"),
		})
	}
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub enum ModesConstraint {
	Anytime = 0,
	NiceToHave = 1,
	UserInitiated = 2,
	Never = 3,
}
impl TryFrom<TransferPriority> for ModesConstraint {
	type Error = anyhow::Error;

	fn try_from(value: TransferPriority) -> Result<Self, Self::Error> {
		Ok(match value {
			TransferPriority::NiceToHave => Self::NiceToHave,
			TransferPriority::UserInitiated => Self::UserInitiated,
			TransferPriority::Other(x) => bail!("invalid transfer priority {x}"),
		})
	}
}
impl TryFrom<Constraint> for ModesConstraint {
	type Error = anyhow::Error;

	fn try_from(value: Constraint) -> Result<Self, Self::Error> {
		Ok(match value {
			Constraint::Anytime => Self::Anytime,
			Constraint::UserInitiated => Self::UserInitiated,
			Constraint::Never => Self::Never,
			Constraint::Other(x) => bail!("invalid constraint {x}"),
		})
	}
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ModesResourceId {
	MainScreen,
	MainAudio,
}
impl TryFrom<ResourceId> for ModesResourceId {
	type Error = anyhow::Error;

	fn try_from(value: ResourceId) -> Result<Self, Self::Error> {
		Ok(match value {
			ResourceId::MainScreen => Self::MainScreen,
			ResourceId::MainAudio => Self::MainAudio,
			ResourceId::Other(x) => bail!("invalid resource id {x}"),
		})
	}
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ModesSpeechState {
	Speaking(ModesEntity),
	Listening(ModesEntity),
}
impl ModesSpeechState {
	pub fn new(entity: ModesEntity, mode: SpeechMode) -> Result<Option<Self>> {
		Ok(match mode {
			SpeechMode::None => None,
			SpeechMode::Speaking => Some(Self::Speaking(entity)),
			SpeechMode::RecognizingSpeech => Some(Self::Listening(entity)),
			SpeechMode::Other(x) => bail!("invalid speech mode {x}"),
		})
	}

	pub fn entity(&self) -> ModesEntity {
		match self {
			Self::Speaking(x) => *x,
			Self::Listening(x) => *x,
		}
	}

	pub fn state(&self) -> SpeechMode {
		match self {
			Self::Speaking(_) => SpeechMode::Speaking,
			Self::Listening(_) => SpeechMode::RecognizingSpeech,
		}
	}
}
impl From<Option<ModesSpeechState>> for SpeechMode {
	fn from(value: Option<ModesSpeechState>) -> Self {
		value.map(|x| x.state()).unwrap_or(SpeechMode::None)
	}
}

fn updated_activity(
	current: Option<ModesEntity>,
	entity: ModesEntity,
	active: bool,
) -> Option<ModesEntity> {
	if active {
		Some(entity)
	} else if current == Some(entity) {
		None
	} else {
		current
	}
}

fn updated_phone_call(
	current: Option<ModesEntity>,
	entity: ModesEntity,
	active: bool,
) -> Result<Option<ModesEntity>> {
	if active && current.is_some_and(|caller| caller != entity) {
		bail!("phone call already active for the other entity");
	}

	Ok(updated_activity(current, entity, active))
}

fn updated_speech(
	current: Option<ModesSpeechState>,
	entity: ModesEntity,
	mode: SpeechMode,
) -> Result<Option<ModesSpeechState>> {
	let next = ModesSpeechState::new(entity, mode)?;
	if next.is_some() || current.is_some_and(|speech| speech.entity() == entity) {
		Ok(next)
	} else {
		Ok(current)
	}
}

#[derive(Copy, Clone)]
struct Borrow {
	entity: ModesEntity,
	constraint: ModesConstraint,
}

#[derive(Clone)]
pub struct ModesResource {
	entity: ModesEntity,
	take_constraint: ModesConstraint,
	borrow_constraint: ModesConstraint,

	borrows: Vec<Borrow>,
}
impl ModesResource {
	pub fn new() -> Self {
		Self {
			entity: ModesEntity::Accessory,
			take_constraint: ModesConstraint::Anytime,
			borrow_constraint: ModesConstraint::Anytime,
			borrows: vec![],
		}
	}

	pub fn owner(&self) -> ModesEntity {
		self.borrows.last().map(|x| x.entity).unwrap_or(self.entity)
	}

	fn handle_change(
		&mut self,
		entity: ModesEntity,
		change: &ResourceChange,
		allow_take_never: bool,
	) -> Result<()> {
		match change.transfer_type {
			TransferType::Take => {
				if !allow_take_never
					&& change
						.take_constraint
						.is_some_and(|x| x == Constraint::Never)
				{
					bail!("take never not allowed for resource");
				}

				self.take(
					entity,
					change
						.transfer_priority
						.context("no transfer_priority provided")?
						.try_into()
						.context("failed to convert transfer_priority")?,
					change
						.take_constraint
						.context("no take_constraint provided")?
						.try_into()
						.context("failed to convert take_constraint")?,
					change
						.borrow_constraint
						.context("no borrow_constraint provided")?
						.try_into()
						.context("failed to convert borrow_constraint")?,
				)?;
			}
			TransferType::Borrow => {
				self.borrow(
					entity,
					change
						.transfer_priority
						.context("no transfer_priority provided")?
						.try_into()
						.context("failed to convert transfer_priority")?,
					change
						.unborrow_constraint
						.context("no unborrow_constraint provided")?
						.try_into()
						.context("failed to convert unborrow_constraint")?,
				)?;
			}
			TransferType::Untake => {
				if !self.untake(entity)? {
					warn!("{entity:?} sent an invalid untake request, ignoring");
				}
			}
			TransferType::Unborrow => {
				self.unborrow(entity)?;
			}
			TransferType::Other(_) => bail!("invalid transfer_type"),
		}

		Ok(())
	}

	pub fn borrow(
		&mut self,
		entity: ModesEntity,
		priority: ModesConstraint,
		unborrow_constraint: ModesConstraint,
	) -> Result<()> {
		let constraint = match self.borrows.last() {
			Some(borrow) if borrow.entity == entity => ModesConstraint::Anytime,
			Some(borrow) => borrow.constraint,
			None if self.entity == entity => ModesConstraint::Anytime,
			None => self.borrow_constraint,
		};

		if priority >= constraint {
			if self.borrows.last().is_some_and(|x| x.entity != entity) {
				self.borrows.clear();
			}

			self.borrows.push(Borrow {
				entity,
				constraint: unborrow_constraint,
			});
			Ok(())
		} else {
			bail!("not enough priority to borrow resource");
		}
	}

	pub fn unborrow(&mut self, entity: ModesEntity) -> Result<()> {
		if self.borrows.last().is_none_or(|x| x.entity != entity) {
			bail!("invalid unborrow");
		}

		self.borrows.pop();

		Ok(())
	}

	pub fn take(
		&mut self,
		entity: ModesEntity,
		priority: ModesConstraint,
		take_constraint: ModesConstraint,
		borrow_constraint: ModesConstraint,
	) -> Result<()> {
		let constraint = match self.borrows.last() {
			Some(borrow) if borrow.entity != entity => borrow.constraint,
			_ if self.entity == entity => ModesConstraint::Anytime, // modify existing take
			Some(_) => self.take_constraint,                        // upgrade
			None => self.take_constraint,
		};

		if priority >= constraint {
			self.entity = entity;
			self.borrow_constraint = borrow_constraint;
			self.take_constraint = take_constraint;
			self.borrows.clear();

			Ok(())
		} else {
			bail!("not enough priority to take resource");
		}
	}

	pub fn untake(&mut self, entity: ModesEntity) -> Result<bool> {
		if self.entity != entity {
			if self.owner() == entity {
				return Ok(false);
			} else {
				bail!("invalid untake");
			}
		}

		self.take_constraint = ModesConstraint::Anytime;
		self.borrow_constraint = ModesConstraint::Anytime;

		Ok(true)
	}
}

pub struct ModesMachine {
	main_screen: ModesResource,
	main_audio: ModesResource,

	speech: Option<ModesSpeechState>,
	phone_call: Option<ModesEntity>,
	turn_by_turn: Option<ModesEntity>,
}
impl ModesMachine {
	pub fn new() -> Self {
		Self {
			main_screen: ModesResource::new(),
			main_audio: ModesResource::new(),

			speech: None,
			phone_call: None,
			turn_by_turn: None,
		}
	}

	pub fn main_screen_owner(&self) -> ModesEntity {
		self.main_screen.owner()
	}
	pub fn main_audio_owner(&self) -> ModesEntity {
		self.main_audio.owner()
	}

	pub fn speech(&self) -> Option<ModesSpeechState> {
		self.speech
	}
	pub fn phone_call(&self) -> Option<ModesEntity> {
		self.phone_call
	}
	pub fn turn_by_turn(&self) -> Option<ModesEntity> {
		self.turn_by_turn
	}

	/// Update either entity's speech state. Inactive updates only clear the sender's state.
	pub fn set_speech(&mut self, entity: ModesEntity, mode: SpeechMode) -> Result<Modes> {
		self.speech = updated_speech(self.speech, entity, mode)?;
		Ok(self.current_modes())
	}

	/// Update either entity's call state. A competing caller is rejected without mutation.
	pub fn set_phone_call(&mut self, entity: ModesEntity, active: bool) -> Result<Modes> {
		self.phone_call = updated_phone_call(self.phone_call, entity, active)?;
		Ok(self.current_modes())
	}

	/// The latest navigation activation wins; deactivation only clears the sender's state.
	pub fn set_turn_by_turn(&mut self, entity: ModesEntity, active: bool) -> Modes {
		self.turn_by_turn = updated_activity(self.turn_by_turn, entity, active);
		self.current_modes()
	}

	pub fn current_modes(&self) -> Modes {
		let resources = vec![
			Resource {
				resource_id: ResourceId::MainScreen,
				entity: self.main_screen.owner().into(),
			},
			Resource {
				resource_id: ResourceId::MainAudio,
				entity: self.main_audio.owner().into(),
			},
		];

		let app_states = vec![
			AppState {
				app_state_id: AppStateId::Speech,
				entity: self.speech.map(|x| x.entity()).into(),
				speech_mode: Some(self.speech.into()),
			},
			AppState {
				app_state_id: AppStateId::PhoneCall,
				entity: self.phone_call.into(),
				speech_mode: None,
			},
			AppState {
				app_state_id: AppStateId::TurnByTurn,
				entity: self.turn_by_turn.into(),
				speech_mode: None,
			},
		];

		Modes {
			resources: Some(resources),
			app_states: Some(app_states),
		}
	}

	pub fn handle_initial_mode_changes(&mut self, changes: ModeChange) -> Result<Modes> {
		let resources = changes
			.resources
			.as_ref()
			.map(|x| {
				x.iter()
					.filter_map(|x| {
						(x.transfer_type == TransferType::Borrow)
							.then_some(x.resource_id.try_into())
					})
					.collect::<Result<Vec<ModesResourceId>>>()
			})
			.transpose()
			.context("failed to find weird resources")?
			.unwrap_or_default();
		let res = self.handle_mode_changes(ModesEntity::Accessory, changes)?;

		// carlinkit decides to give borrows back to the controller when they end
		for resource in resources {
			let resource = match resource {
				ModesResourceId::MainScreen => &mut self.main_screen,
				ModesResourceId::MainAudio => &mut self.main_audio,
			};
			resource.entity = ModesEntity::Controller;
		}

		Ok(res)
	}

	pub fn handle_mode_changes(
		&mut self,
		entity: ModesEntity,
		changes: ModeChange,
	) -> Result<Modes> {
		let resources = changes.resources.unwrap_or_default();
		let app_states = changes.app_states.unwrap_or_default();

		let mut main_screen = self.main_screen.clone();
		let mut main_audio = self.main_audio.clone();
		let mut speech = self.speech;
		let mut phone_call = self.phone_call;
		let mut turn_by_turn = self.turn_by_turn;

		if let Some(change) = resources
			.iter()
			.find(|x| x.resource_id == ResourceId::MainScreen)
		{
			main_screen.handle_change(entity, change, true)?;
		}

		if let Some(change) = resources
			.iter()
			.find(|x| x.resource_id == ResourceId::MainAudio)
		{
			main_audio.handle_change(entity, change, false)?;
		}

		if let Some(app_state) = app_states
			.iter()
			.find(|x| x.app_state_id == AppStateId::Speech)
		{
			speech = updated_speech(
				speech,
				entity,
				app_state.speech_mode.context("no speech mode provided")?,
			)?;
		}

		if let Some(app_state) = app_states
			.iter()
			.find(|x| x.app_state_id == AppStateId::PhoneCall)
		{
			phone_call = updated_phone_call(
				phone_call,
				entity,
				app_state.state.context("no state provided")?,
			)?;
		}

		if let Some(app_state) = app_states
			.iter()
			.find(|x| x.app_state_id == AppStateId::TurnByTurn)
		{
			turn_by_turn = updated_activity(
				turn_by_turn,
				entity,
				app_state.state.context("no state provided")?,
			);
		}

		self.main_screen = main_screen;
		self.main_audio = main_audio;
		self.speech = speech;
		self.phone_call = phone_call;
		self.turn_by_turn = turn_by_turn;

		Ok(self.current_modes())
	}

	pub fn resource(
		&mut self,
		id: ModesResourceId,
		func: impl Fn(&mut ModesResource) -> Result<()>,
	) -> Result<Modes> {
		let resource = match id {
			ModesResourceId::MainScreen => &mut self.main_screen,
			ModesResourceId::MainAudio => &mut self.main_audio,
		};

		(func)(resource)?;

		Ok(self.current_modes())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::carplay::net::proto::modes::AppStateChange;

	fn entity_pairs() -> [(ModesEntity, ModesEntity); 2] {
		[
			(ModesEntity::Controller, ModesEntity::Accessory),
			(ModesEntity::Accessory, ModesEntity::Controller),
		]
	}

	fn activate_app_states(machine: &mut ModesMachine, entity: ModesEntity) {
		machine.set_speech(entity, SpeechMode::Speaking).unwrap();
		machine.set_phone_call(entity, true).unwrap();
		machine.set_turn_by_turn(entity, true);
	}

	fn assert_active_app_states(machine: &ModesMachine, entity: ModesEntity) {
		assert_eq!(machine.speech(), Some(ModesSpeechState::Speaking(entity)));
		assert_eq!(machine.phone_call(), Some(entity));
		assert_eq!(machine.turn_by_turn(), Some(entity));
	}

	fn inactive_changes() -> ModeChange {
		ModeChange {
			app_states: Some(vec![
				AppStateChange {
					app_state_id: AppStateId::Speech,
					speech_mode: Some(SpeechMode::None),
					state: None,
				},
				AppStateChange {
					app_state_id: AppStateId::PhoneCall,
					speech_mode: None,
					state: Some(false),
				},
				AppStateChange {
					app_state_id: AppStateId::TurnByTurn,
					speech_mode: None,
					state: Some(false),
				},
			]),
			..Default::default()
		}
	}

	fn activity_change(app_state_id: AppStateId, active: bool) -> ModeChange {
		ModeChange {
			app_states: Some(vec![AppStateChange {
				app_state_id,
				speech_mode: None,
				state: Some(active),
			}]),
			..Default::default()
		}
	}

	#[test]
	fn helpers_ignore_other_entity_inactive_updates() {
		for (owner, other) in entity_pairs() {
			let mut machine = ModesMachine::new();
			activate_app_states(&mut machine, owner);

			machine.set_speech(other, SpeechMode::None).unwrap();
			machine.set_phone_call(other, false).unwrap();
			machine.set_turn_by_turn(other, false);

			assert_active_app_states(&machine, owner);
		}
	}

	#[test]
	fn helpers_clear_sender_state_and_accept_repeated_inactive_updates() {
		for (entity, _) in entity_pairs() {
			let mut machine = ModesMachine::new();
			activate_app_states(&mut machine, entity);

			for _ in 0..2 {
				machine.set_speech(entity, SpeechMode::None).unwrap();
				machine.set_phone_call(entity, false).unwrap();
				machine.set_turn_by_turn(entity, false);

				assert_eq!(machine.speech(), None);
				assert_eq!(machine.phone_call(), None);
				assert_eq!(machine.turn_by_turn(), None);
			}
		}
	}

	#[test]
	fn phone_call_first_caller_wins_for_helpers_and_requests() {
		for (caller, other) in entity_pairs() {
			let mut machine = ModesMachine::new();
			machine.set_phone_call(caller, true).unwrap();
			machine.set_phone_call(caller, true).unwrap();

			assert!(machine.set_phone_call(other, true).is_err());
			assert!(
				machine
					.handle_mode_changes(other, activity_change(AppStateId::PhoneCall, true))
					.is_err()
			);
			assert_eq!(machine.phone_call(), Some(caller));

			machine.set_phone_call(caller, false).unwrap();
			machine.set_phone_call(other, true).unwrap();
			assert_eq!(machine.phone_call(), Some(other));
		}
	}

	#[test]
	fn navigation_activation_last_system_wins_for_helpers_and_requests() {
		for (first, second) in entity_pairs() {
			let mut machine = ModesMachine::new();
			machine.set_turn_by_turn(first, true);
			machine
				.handle_mode_changes(second, activity_change(AppStateId::TurnByTurn, true))
				.unwrap();
			assert_eq!(machine.turn_by_turn(), Some(second));

			machine.set_turn_by_turn(first, false);
			assert_eq!(machine.turn_by_turn(), Some(second));
			machine.set_turn_by_turn(first, true);
			machine
				.handle_mode_changes(second, activity_change(AppStateId::TurnByTurn, false))
				.unwrap();
			assert_eq!(machine.turn_by_turn(), Some(first));
		}
	}

	#[test]
	fn speech_mode_changes_validate_before_mutation() {
		for (entity, other) in entity_pairs() {
			let mut machine = ModesMachine::new();
			machine.set_speech(entity, SpeechMode::Speaking).unwrap();
			machine
				.set_speech(entity, SpeechMode::RecognizingSpeech)
				.unwrap();
			assert_eq!(machine.speech(), Some(ModesSpeechState::Listening(entity)));

			assert!(machine.set_speech(other, SpeechMode::Other(42)).is_err());
			assert_eq!(machine.speech(), Some(ModesSpeechState::Listening(entity)));
		}
	}

	#[test]
	fn incoming_inactive_updates_allow_other_valid_changes() {
		for (owner, other) in entity_pairs() {
			let mut machine = ModesMachine::new();
			activate_app_states(&mut machine, owner);
			machine
				.resource(ModesResourceId::MainScreen, |resource| {
					resource.take(
						owner,
						ModesConstraint::UserInitiated,
						ModesConstraint::Anytime,
						ModesConstraint::Anytime,
					)
				})
				.unwrap();

			let changes = ModeChange {
				resources: Some(vec![ResourceChange {
					resource_id: ResourceId::MainScreen,
					transfer_type: TransferType::Borrow,
					transfer_priority: Some(TransferPriority::UserInitiated),
					take_constraint: None,
					borrow_constraint: None,
					unborrow_constraint: Some(Constraint::Anytime),
				}]),
				..inactive_changes()
			};
			let snapshot = machine.handle_mode_changes(other, changes).unwrap();

			assert_active_app_states(&machine, owner);
			assert_eq!(machine.main_screen_owner(), other);
			assert!(
				snapshot
					.app_states
					.unwrap()
					.iter()
					.all(|state| state.entity == owner.into())
			);
			assert_eq!(
				snapshot
					.resources
					.unwrap()
					.iter()
					.find(|resource| resource.resource_id == ResourceId::MainScreen)
					.unwrap()
					.entity,
				other.into()
			);
		}
	}

	#[test]
	fn conflicting_phone_call_rolls_back_resource_and_speech_changes() {
		let mut machine = ModesMachine::new();
		machine
			.set_phone_call(ModesEntity::Accessory, true)
			.unwrap();
		let changes = ModeChange {
			resources: Some(vec![ResourceChange {
				resource_id: ResourceId::MainScreen,
				transfer_type: TransferType::Take,
				transfer_priority: Some(TransferPriority::UserInitiated),
				take_constraint: Some(Constraint::Anytime),
				borrow_constraint: Some(Constraint::Anytime),
				unborrow_constraint: None,
			}]),
			app_states: Some(vec![
				AppStateChange {
					app_state_id: AppStateId::Speech,
					speech_mode: Some(SpeechMode::Speaking),
					state: None,
				},
				AppStateChange {
					app_state_id: AppStateId::PhoneCall,
					speech_mode: None,
					state: Some(true),
				},
			]),
			..Default::default()
		};

		assert!(
			machine
				.handle_mode_changes(ModesEntity::Controller, changes)
				.is_err()
		);
		assert_eq!(machine.main_screen_owner(), ModesEntity::Accessory);
		assert_eq!(machine.speech(), None);
		assert_eq!(machine.phone_call(), Some(ModesEntity::Accessory));
	}

	#[test]
	fn incoming_deactivation_clears_sender_state() {
		for (entity, _) in entity_pairs() {
			let mut machine = ModesMachine::new();
			activate_app_states(&mut machine, entity);
			let snapshot = machine
				.handle_mode_changes(entity, inactive_changes())
				.unwrap();

			assert_eq!(machine.speech(), None);
			assert_eq!(machine.phone_call(), None);
			assert_eq!(machine.turn_by_turn(), None);
			let states = snapshot.app_states.unwrap();
			assert!(states.iter().all(|state| state.entity == Entity::None));
			assert_eq!(
				states
					.iter()
					.find(|state| state.app_state_id == AppStateId::Speech)
					.unwrap()
					.speech_mode,
				Some(SpeechMode::None)
			);
		}
	}
}
