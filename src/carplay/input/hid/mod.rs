//! Semantic decoding of the R29 key, single-touch and multi-axis input profiles.
//! Report IDs select layouts; collections determine what each field means.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap},
	time::Instant,
};

use anyhow::{Context, Result, ensure};
use hidreport::{
	ArrayField, Collection, CollectionType, Field, FieldAttributes, Report, ReportDescriptor,
	VariableField,
};

use super::{
	InputAxis, InputAxisCapability, InputEvent, InputKey, InputReport, InputTouchCapability,
	InputTouchInfo, KeyMerger, TouchContact,
};

#[cfg(test)]
mod tests;

fn bounds(min: i32, max: i32) -> (i64, i64) {
	(
		i64::from(min),
		if min < 0 {
			i64::from(max)
		} else {
			i64::from(max as u32)
		},
	)
}

fn variable_bounds(field: &VariableField) -> (i64, i64) {
	bounds(
		i32::from(field.logical_minimum),
		i32::from(field.logical_maximum),
	)
}

fn validate_variable(field: &VariableField) -> Result<()> {
	ensure!(
		(1..=32).contains(&field.bits.len()),
		"unsupported numeric HID field width"
	);
	let (min, max) = variable_bounds(field);
	ensure!(min <= max, "invalid HID logical range");
	Ok(())
}

fn number(field: &VariableField, bytes: &[u8]) -> Result<i64> {
	let value = field
		.extract(bytes)
		.context("failed to extract HID field")?;
	Ok(if value.is_signed() {
		i64::from(i32::from(value))
	} else {
		i64::from(u32::from(value))
	})
}

fn checked_number(field: &VariableField, bytes: &[u8]) -> Result<Option<i64>> {
	let value = number(field, bytes)?;
	let (min, max) = variable_bounds(field);
	if !(min..=max).contains(&value) && field.has_null_state() {
		return Ok(None);
	}
	ensure!(
		(min..=max).contains(&value),
		"HID value outside logical range"
	);
	Ok(Some(value))
}

fn application<'a>(field: &'a VariableField, usages: &[u32]) -> Option<&'a Collection> {
	field.collections.iter().find(|collection| {
		collection.collection_type() == CollectionType::Application
			&& collection
				.usages()
				.iter()
				.any(|usage| usages.contains(&u32::from(usage)))
	})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum TouchField {
	X,
	Y,
	Touch,
	Tip,
	InRange,
	DataValid,
	Id,
}

impl TouchField {
	fn from_usage(usage: u32) -> Option<Self> {
		Some(match usage {
			0x010030 => Self::X,
			0x010031 => Self::Y,
			0x0d0033 => Self::Touch,
			0x0d0042 => Self::Tip,
			0x0d0032 => Self::InRange,
			0x0d0037 => Self::DataValid,
			0x0d0038 | 0x0d0051 => Self::Id,
			_ => return None,
		})
	}
}

#[derive(Default)]
struct TouchBuilder {
	application: Option<Collection>,
	finger: Option<Collection>,
	fields: BTreeMap<TouchField, (u32, VariableField)>,
}

impl TouchBuilder {
	fn observe(&mut self, input: &Field) -> Result<()> {
		let Field::Variable(field) = input else {
			return Ok(());
		};
		let Some(app) = application(field, &[0x0d0004, 0x0d0005]) else {
			return Ok(());
		};
		if let Some(previous) = &self.application {
			ensure!(
				previous == app,
				"multiple touch surfaces in one HID device are unsupported"
			);
		} else {
			self.application = Some(app.clone());
		}
		let Some(role) = TouchField::from_usage(u32::from(field.usage)) else {
			return Ok(());
		};
		let finger = field
			.collections
			.iter()
			.find(|collection| {
				collection.collection_type() == CollectionType::Logical
					&& collection
						.usages()
						.iter()
						.any(|usage| u32::from(usage) == 0x0d0022)
			})
			.context("touch field without a logical Finger collection")?;
		if let Some(previous) = &self.finger {
			ensure!(
				previous == finger,
				"multiple Finger collections are unsupported (single touch only)"
			);
		} else {
			self.finger = Some(finger.clone());
		}
		validate_variable(field)?;
		ensure!(field.is_absolute(), "touch fields must be absolute");
		// Prefer Contact Identifier when both identifier usages are present.
		if role == TouchField::Id && u32::from(field.usage) == 0x0d0051 {
			self.fields
				.insert(role, (u32::from(input.id()), field.clone()));
		} else if role != TouchField::Id || !self.fields.contains_key(&role) {
			ensure!(
				self.fields
					.insert(role, (u32::from(input.id()), field.clone()))
					.is_none(),
				"duplicate touch field"
			);
		}
		Ok(())
	}

	fn finish(self) -> Result<Option<TouchLayout>> {
		let Some(app) = self.application else {
			return Ok(None);
		};
		let x = &self
			.fields
			.get(&TouchField::X)
			.context("touch surface without X")?
			.1;
		let y = &self
			.fields
			.get(&TouchField::Y)
			.context("touch surface without Y")?
			.1;
		ensure!(
			self.fields.contains_key(&TouchField::Touch)
				|| self.fields.contains_key(&TouchField::Tip),
			"touch surface without Touch or Tip Switch"
		);
		let x_range = variable_bounds(x);
		let y_range = variable_bounds(y);
		ensure!(
			x_range.0 < x_range.1 && y_range.0 < y_range.1,
			"empty touch coordinate range"
		);
		let physical_size_mm = physical_length_mm(x)
			.zip(physical_length_mm(y))
			.map(|(x, y)| [x, y]);
		let info = InputTouchInfo {
			max_touches: 1,
			physical_size_mm,
		};
		let is_pad = app
			.usages()
			.iter()
			.any(|usage| u32::from(usage) == 0x0d0005);
		if is_pad {
			ensure!(
				x.unit_exponent.is_some()
					&& y.unit_exponent.is_some()
					&& physical_size_mm.is_some(),
				"touchpad requires physical dimensions, length units and unit exponents"
			);
		}
		Ok(Some(TouchLayout {
			capability: if is_pad {
				InputTouchCapability::Touchpad(info)
			} else {
				InputTouchCapability::Touchscreen(info)
			},
			x_range,
			y_range,
			tip: self.fields.contains_key(&TouchField::Tip),
			fields: self.fields,
		}))
	}
}

fn physical_length_mm(field: &VariableField) -> Option<f32> {
	// Inspect the encoded unit rather than calling Unit::system(), which panics
	// for vendor-defined unit systems in hidreport 0.6.
	let factor = match u32::from(field.unit?) {
		0x11 => 10.0,
		0x13 => 25.4,
		_ => return None,
	};
	let min = i32::from(field.physical_minimum?) as f64;
	let max = i32::from(field.physical_maximum?) as f64;
	let exponent = field
		.unit_exponent
		.map_or(0, |value| i32::from(value.exponent()));
	let length = ((max - min) * 10f64.powi(exponent) * factor) as f32;
	(length.is_finite() && length > 0.0).then_some(length)
}

struct TouchLayout {
	capability: InputTouchCapability,
	x_range: (i64, i64),
	y_range: (i64, i64),
	tip: bool,
	fields: BTreeMap<TouchField, (u32, VariableField)>,
}

#[derive(Clone)]
struct TouchState {
	x: Option<i64>,
	y: Option<i64>,
	touch: bool,
	tip: bool,
	in_range: bool,
	data_valid: bool,
	id: u32,
	active: Option<TouchContact>,
}

impl Default for TouchState {
	fn default() -> Self {
		Self {
			x: None,
			y: None,
			touch: false,
			tip: false,
			in_range: true,
			data_valid: true,
			id: 0,
			active: None,
		}
	}
}

impl TouchState {
	fn update(
		&mut self,
		layout: &TouchLayout,
		values: &[(TouchField, i64)],
	) -> Result<Vec<InputEvent>> {
		for &(role, value) in values {
			match role {
				TouchField::X => self.x = Some(value),
				TouchField::Y => self.y = Some(value),
				TouchField::Touch => self.touch = value != 0,
				TouchField::Tip => self.tip = value != 0,
				TouchField::InRange => self.in_range = value != 0,
				TouchField::DataValid => self.data_valid = value != 0,
				TouchField::Id => {
					self.id = u32::try_from(value).context("invalid touch contact ID")?
				}
			}
		}
		let down = (if layout.tip { self.tip } else { self.touch }) && self.in_range;
		let mut contacts = Vec::new();
		if !down {
			if let Some(contact) = self.active.take() {
				contacts.push(TouchContact {
					touching: false,
					..contact
				});
			}
		} else if self.data_valid {
			if let (Some(x), Some(y)) = (self.x, self.y) {
				let (xmin, xmax) = layout.x_range;
				let (ymin, ymax) = layout.y_range;
				ensure!(
					(xmin..=xmax).contains(&x) && (ymin..=ymax).contains(&y),
					"touch position outside logical range"
				);
				let contact = TouchContact {
					id: self.id,
					x: ((x - xmin) as f64 / (xmax - xmin) as f64) as f32,
					y: ((y - ymin) as f64 / (ymax - ymin) as f64) as f32,
					touching: true,
				};
				if let Some(previous) = self.active {
					if previous.id != contact.id {
						contacts.push(TouchContact {
							touching: false,
							..previous
						});
					}
				}
				if self.active != Some(contact) {
					contacts.push(contact);
				}
				self.active = Some(contact);
			}
		}
		Ok(if contacts.is_empty() {
			vec![]
		} else {
			vec![InputEvent::TouchEvent { contacts }]
		})
	}
}

enum Role {
	Key(InputKey),
	/// R29 maps positive knob Z movement to the primary button.
	Selection,
	Touch(TouchField),
	Axis(usize),
}
enum Binding {
	Variable(VariableField, Role),
	Array(ArrayField),
}
struct ReportPlan {
	size: usize,
	bindings: Vec<Binding>,
}

pub struct HidParser {
	uses_ids: bool,
	reports: HashMap<Option<u8>, ReportPlan>,
	keys: Vec<InputKey>,
	axes: Vec<InputAxisCapability>,
	pressed: KeyMerger<Option<u8>>,
	axis_values: HashMap<u32, i64>,
	touch: Option<TouchLayout>,
	touch_state: TouchState,
}

impl HidParser {
	pub fn new(descriptor: &[u8]) -> Result<Self> {
		let desc = ReportDescriptor::try_from(descriptor)
			.context("failed to parse HID report descriptor")?;
		let reports = desc.input_reports();
		let first = reports
			.first()
			.context("HID descriptor has no input reports")?;
		let uses_ids = first.report_id().is_some();
		let mut builder = TouchBuilder::default();
		for report in reports {
			ensure!(
				report.report_id().is_some() == uses_ids,
				"mixed numbered and unnumbered HID reports"
			);
			for field in report.fields() {
				builder.observe(field)?;
			}
		}
		let touch = builder.finish()?;
		let mut plans = HashMap::new();
		let mut keys = BTreeSet::new();
		let mut axes = Vec::new();
		for report in reports {
			let mut bindings = Vec::new();
			for input in report.fields() {
				match input {
					Field::Variable(field) => {
						let usage = u32::from(field.usage);
						let touch_role = touch.as_ref().and_then(|layout| {
							layout
								.fields
								.iter()
								.find(|(_, candidate)| candidate.0 == u32::from(input.id()))
								.map(|(&role, _)| role)
						});
						let role = if let Some(role) = touch_role {
							Some(Role::Touch(role))
						} else if let Some(key) = InputKey::from_usage(usage) {
							keys.insert(key);
							Some(Role::Key(key))
						} else if application(field, &[0x010008]).is_some() {
							if usage == 0x010032 {
								keys.insert(InputKey::Primary);
								Some(Role::Selection)
							} else {
								axis_from_usage(usage).map(|axis| {
									let (min, max) = variable_bounds(field);
									let index = axes.len();
									axes.push(InputAxisCapability {
										id: index as u32,
										axis,
										relative: field.is_relative(),
										logical_minimum: min,
										logical_maximum: max,
									});
									Role::Axis(index)
								})
							}
						} else {
							None
						};
						if let Some(role) = role {
							validate_variable(field)?;
							if matches!(role, Role::Selection) {
								ensure!(field.is_absolute(), "knob Z selection must be absolute");
								let (min, max) = variable_bounds(field);
								ensure!(
									min <= 0 && max > 0,
									"knob Z selection requires a zero rest position and positive movement"
								);
							}
							bindings.push(Binding::Variable(field.clone(), role));
						}
					}
					Field::Array(field) => {
						let (min, max) = bounds(
							i32::from(field.logical_minimum),
							i32::from(field.logical_maximum),
						);
						let supported: Vec<_> = field
							.usages()
							.iter()
							.take((max - min + 1).max(0) as usize)
							.filter_map(|usage| InputKey::from_usage(u32::from(usage)))
							.collect();
						if !supported.is_empty() {
							let count = usize::from(field.report_count);
							ensure!(
								min <= max
									&& count > 0
									&& field.bits.len() % count == 0
									&& (1..=32).contains(&(field.bits.len() / count)),
								"invalid HID key array"
							);
							keys.extend(supported);
							bindings.push(Binding::Array(field.clone()));
						}
					}
					Field::Constant(_) => {}
				}
			}
			let id = report.report_id().map(u8::from);
			ensure!(id != Some(0), "HID report ID zero is reserved");
			ensure!(
				plans
					.insert(
						id,
						ReportPlan {
							size: report.size_in_bytes(),
							bindings
						}
					)
					.is_none(),
				"duplicate HID input report ID"
			);
		}
		Ok(Self {
			uses_ids,
			reports: plans,
			keys: keys.into_iter().collect(),
			axes,
			pressed: KeyMerger::default(),
			axis_values: HashMap::new(),
			touch,
			touch_state: TouchState::default(),
		})
	}

	pub fn supported_keys(&self) -> Vec<InputKey> {
		self.keys.clone()
	}
	pub fn supported_touch(&self) -> Option<InputTouchCapability> {
		self.touch.as_ref().map(|layout| layout.capability.clone())
	}
	pub fn supported_axes(&self) -> Vec<InputAxisCapability> {
		self.axes.clone()
	}

	pub fn parse(&mut self, time: Instant, report: &[u8]) -> Result<InputReport> {
		ensure!(!report.is_empty(), "empty HID report");
		let id = self.uses_ids.then_some(report[0]);
		let plan = self
			.reports
			.get(&id)
			.context("unknown HID input report ID")?;
		ensure!(
			report.len() == plan.size,
			"HID report size incorrect: expected {}, got {}",
			plan.size,
			report.len()
		);
		let mut keys = BTreeSet::new();
		let mut touch_values = Vec::new();
		let mut axis_values = Vec::new();
		// Decode and validate everything before committing any held state.
		for binding in &plan.bindings {
			match binding {
				Binding::Variable(field, Role::Key(key)) => {
					if checked_number(field, report)?.is_some_and(|value| value != 0) {
						keys.insert(*key);
					}
				}
				Binding::Variable(field, Role::Selection) => {
					if checked_number(field, report)?.is_some_and(|value| value > 0) {
						keys.insert(InputKey::Primary);
					}
				}
				Binding::Variable(field, Role::Touch(role)) => {
					// Release reports may contain undefined coordinates. The touch state
					// validates coordinates only for a valid, active contact.
					let value = if matches!(role, TouchField::X | TouchField::Y) {
						Some(number(field, report)?)
					} else {
						checked_number(field, report)?
					};
					if let Some(value) = value {
						touch_values.push((*role, value));
					}
				}
				Binding::Variable(field, Role::Axis(index)) => {
					let axis = &self.axes[*index];
					let value = checked_number(field, report)?.unwrap_or_else(|| neutral(axis));
					axis_values.push((*index, value));
				}
				Binding::Array(field) => decode_array(field, report, &mut keys)?,
			}
		}
		let mut touch_state = self.touch_state.clone();
		let touch_events = if !touch_values.is_empty() {
			touch_state.update(
				self.touch
					.as_ref()
					.expect("touch bindings require a layout"),
				&touch_values,
			)?
		} else {
			vec![]
		};
		let mut events = self.pressed.replace(id, keys);
		events.extend(touch_events);
		self.touch_state = touch_state;
		for (index, value) in axis_values {
			let axis = &self.axes[index];
			if axis.relative {
				if value != 0 {
					events.push(InputEvent::Axis { id: axis.id, value });
				}
			} else {
				let previous = self
					.axis_values
					.insert(axis.id, value)
					.unwrap_or_else(|| neutral(axis));
				if previous != value {
					events.push(InputEvent::Axis { id: axis.id, value });
				}
			}
		}
		Ok(InputReport { time, events })
	}

	pub fn flush(&mut self, time: Instant) -> Option<InputReport> {
		let mut events = self.pressed.flush();
		if let Some(contact) = self.touch_state.active {
			events.push(InputEvent::TouchEvent {
				contacts: vec![TouchContact {
					touching: false,
					..contact
				}],
			});
		}
		self.touch_state = TouchState::default();
		for axis in &self.axes {
			if !axis.relative
				&& self
					.axis_values
					.get(&axis.id)
					.is_some_and(|&value| value != neutral(axis))
			{
				events.push(InputEvent::Axis {
					id: axis.id,
					value: neutral(axis),
				});
			}
		}
		self.axis_values.clear();
		(!events.is_empty()).then_some(InputReport { time, events })
	}
}

fn axis_from_usage(usage: u32) -> Option<InputAxis> {
	Some(match usage {
		0x010030 => InputAxis::X,
		0x010031 => InputAxis::Y,
		0x010035 => InputAxis::Rz,
		0x010037 => InputAxis::Dial,
		0x010038 => InputAxis::Wheel,
		_ => return None,
	})
}

fn neutral(axis: &InputAxisCapability) -> i64 {
	0i64.clamp(axis.logical_minimum, axis.logical_maximum)
}

fn decode_array(field: &ArrayField, bytes: &[u8], keys: &mut BTreeSet<InputKey>) -> Result<()> {
	let (min, max) = bounds(
		i32::from(field.logical_minimum),
		i32::from(field.logical_maximum),
	);
	let count = usize::from(field.report_count);
	let width = field.bits.len() / count;
	for index in 0..count {
		let raw = u32::from(
			field
				.extract_one(bytes, index)
				.context("failed to extract HID array")?,
		);
		// hidreport 0.6 sign-extends using the whole array width. Correct each
		// selector using its element width, also for non-byte-aligned arrays.
		let value = if min < 0 {
			i64::from(((raw << (32 - width)) as i32) >> (32 - width))
		} else {
			i64::from(raw)
		};
		if value == 0 && min > 0 {
			continue;
		}
		if !(min..=max).contains(&value) && field.has_null_state() {
			continue;
		}
		ensure!(
			(min..=max).contains(&value),
			"HID array selector outside logical range"
		);
		let usage = field
			.usages()
			.get((value - min) as usize)
			.context("HID array selector has no declared usage")?;
		let usage = u32::from(usage);
		ensure!(
			!matches!(usage, 0x070001..=0x070003),
			"keyboard array reports rollover or error"
		);
		if let Some(key) = InputKey::from_usage(usage) {
			keys.insert(key);
		}
	}
	Ok(())
}
