use super::*;

// R29 examples, §§30.3.5.1.4, 30.3.5.3 and 30.3.5.4.
const TOUCH: &[u8] = &[
	0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x05, 0x0d, 0x09, 0x33,
	0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x95, 0x01, 0x81, 0x01,
	0x05, 0x01, 0x09, 0x30, 0x15, 0x00, 0x26, 0x20, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0x09,
	0x31, 0x15, 0x00, 0x26, 0xe0, 0x01, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0xc0, 0xc0,
];

const KNOB: &[u8] = &[
	0x05, 0x01, 0x09, 0x08, 0xa1, 0x01, 0x05, 0x09, 0x09, 0x01, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
	0x95, 0x01, 0x81, 0x02, 0x05, 0x0c, 0x0a, 0x23, 0x02, 0x0a, 0x24, 0x02, 0x15, 0x00, 0x25, 0x01,
	0x75, 0x01, 0x95, 0x02, 0x81, 0x02, 0x75, 0x05, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
	0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38,
	0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0,
];

const MEDIA: &[u8] = &[
	0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x05, 0x0c, 0x09, 0xb5, 0x09, 0xb6, 0x09, 0xcd, 0x15, 0x00,
	0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x75, 0x05, 0x95, 0x01, 0x81, 0x01, 0xc0,
];

fn parse(parser: &mut HidParser, bytes: &[u8]) -> Vec<InputEvent> {
	parser.parse(Instant::now(), bytes).unwrap().events
}

fn key(key: InputKey, pressed: bool) -> InputEvent {
	InputEvent::Key { key, pressed }
}

fn contacts(events: &[InputEvent]) -> &[TouchContact] {
	let [InputEvent::TouchEvent { contacts }] = events else {
		panic!("expected touch event: {events:?}")
	};
	contacts
}

fn position(bytes: &[u8], needle: &[u8]) -> usize {
	bytes
		.windows(needle.len())
		.position(|value| value == needle)
		.unwrap()
}

#[test]
fn r29_media_reports_emit_changes_and_flush_once() {
	let mut parser = HidParser::new(MEDIA).unwrap();
	assert_eq!(
		parser.supported_keys(),
		[
			InputKey::NextTrack,
			InputKey::PreviousTrack,
			InputKey::PlayPause
		]
	);
	assert!(parser.supported_touch().is_none());
	assert_eq!(
		parse(&mut parser, &[5]),
		[
			key(InputKey::NextTrack, true),
			key(InputKey::PlayPause, true)
		]
	);
	assert!(parse(&mut parser, &[5]).is_empty());
	assert_eq!(parse(&mut parser, &[4]), [key(InputKey::NextTrack, false)]);
	assert_eq!(
		parser.flush(Instant::now()).unwrap().events,
		[key(InputKey::PlayPause, false)]
	);
	assert!(parser.flush(Instant::now()).is_none());
	assert_eq!(parse(&mut parser, &[4]), [key(InputKey::PlayPause, true)]);
}

#[test]
fn r29_touch_has_no_report_id_and_keeps_position_on_release() {
	let mut parser = HidParser::new(TOUCH).unwrap();
	assert_eq!(
		parser.supported_touch(),
		Some(InputTouchCapability::Touchscreen(InputTouchInfo {
			max_touches: 1,
			physical_size_mm: None
		}))
	);
	let down = parse(&mut parser, &[1, 0x90, 1, 0xf0, 0]);
	assert_eq!(
		contacts(&down),
		[TouchContact {
			id: 0,
			x: 0.5,
			y: 0.5,
			touching: true
		}]
	);
	assert!(parse(&mut parser, &[1, 0x90, 1, 0xf0, 0]).is_empty());
	let moved = parse(&mut parser, &[1, 0x20, 3, 0xe0, 1]);
	assert_eq!(contacts(&moved)[0].x, 1.0);
	assert_eq!(contacts(&moved)[0].y, 1.0);
	let released = parse(&mut parser, &[0, 0xff, 0xff, 0xff, 0xff]);
	assert_eq!(
		contacts(&released),
		[TouchContact {
			id: 0,
			x: 1.0,
			y: 1.0,
			touching: false
		}]
	);
	assert!(parser.flush(Instant::now()).is_none());
}

#[test]
fn numbered_touch_includes_the_prefix_in_extraction_and_length() {
	let mut descriptor = TOUCH.to_vec();
	descriptor.splice(6..6, [0x85, 7]);
	let mut parser = HidParser::new(&descriptor).unwrap();
	let down = parse(&mut parser, &[7, 1, 0x90, 1, 0xf0, 0]);
	assert_eq!(contacts(&down)[0].x, 0.5);
	assert!(
		parser
			.parse(Instant::now(), &[8, 1, 0x90, 1, 0xf0, 0])
			.is_err()
	);
	assert!(
		parser
			.parse(Instant::now(), &[7, 1, 0x90, 1, 0xf0])
			.is_err()
	);
	let flushed = parser.flush(Instant::now()).unwrap();
	assert_eq!(
		contacts(&flushed.events)[0],
		TouchContact {
			id: 0,
			x: 0.5,
			y: 0.5,
			touching: false
		}
	);
}

#[test]
fn touch_normalization_uses_signed_nonzero_minima() {
	let mut descriptor = TOUCH.to_vec();
	let x = position(&descriptor, &[0x09, 0x30, 0x15, 0]);
	descriptor[x + 3] = 0x9c; // -100..800
	let y = position(&descriptor, &[0x09, 0x31, 0x15, 0]);
	descriptor[y + 3] = 20; // 20..480
	let mut parser = HidParser::new(&descriptor).unwrap();
	let down = parse(&mut parser, &[1, 0x5e, 1, 0xfa, 0]); // 350,250
	assert_eq!(contacts(&down)[0].x, 0.5);
	assert_eq!(contacts(&down)[0].y, 0.5);
}

#[test]
fn touch_reports_can_split_flags_and_coordinates() {
	let mut descriptor = TOUCH.to_vec();
	descriptor.splice(6..6, [0x85, 1]);
	let coordinates = position(&descriptor, &[0x05, 1, 0x09, 0x30]);
	descriptor.splice(coordinates..coordinates, [0x85, 2]);
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert!(parse(&mut parser, &[1, 1]).is_empty());
	let down = parse(&mut parser, &[2, 0x90, 1, 0xf0, 0]);
	assert_eq!(contacts(&down)[0].x, 0.5);
	let up = parse(&mut parser, &[1, 0]);
	assert!(!contacts(&up)[0].touching);
}

#[test]
fn proximity_and_validity_do_not_create_a_touch_and_invalid_samples_can_release() {
	let mut descriptor = TOUCH.to_vec();
	let flags_end = position(&descriptor, &[0x81, 2]) + 2;
	descriptor.splice(
		flags_end..flags_end,
		[0x09, 0x32, 0x09, 0x37, 0x95, 2, 0x81, 2],
	);
	let padding = position(&descriptor, &[0x75, 7]);
	descriptor[padding + 1] = 5;
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert!(parse(&mut parser, &[6, 0x90, 1, 0xf0, 0]).is_empty());
	assert!(contacts(&parse(&mut parser, &[7, 0x90, 1, 0xf0, 0]))[0].touching);
	assert!(parse(&mut parser, &[3, 0xff, 0xff, 0xff, 0xff]).is_empty());
	let up = parse(&mut parser, &[0, 0xff, 0xff, 0xff, 0xff]);
	assert_eq!(
		contacts(&up)[0],
		TouchContact {
			id: 0,
			x: 0.5,
			y: 0.5,
			touching: false
		}
	);
}

#[test]
fn invalid_touch_sample_does_not_commit_state() {
	let mut parser = HidParser::new(TOUCH).unwrap();
	let down = parse(&mut parser, &[1, 0x90, 1, 0xf0, 0]);
	assert!(
		parser
			.parse(Instant::now(), &[1, 0xff, 0xff, 0xff, 0xff])
			.is_err()
	);
	assert!(parse(&mut parser, &[1, 0x90, 1, 0xf0, 0]).is_empty());
	assert_eq!(
		contacts(&parser.flush(Instant::now()).unwrap().events)[0].x,
		contacts(&down)[0].x
	);
}

#[test]
fn contact_id_changes_release_the_previous_contact() {
	let mut descriptor = TOUCH.to_vec();
	let end = descriptor.len() - 2;
	descriptor.splice(
		end..end,
		[
			0x05, 0x0d, 0x09, 0x51, 0x15, 0, 0x26, 0xff, 0, 0x75, 8, 0x95, 1, 0x81, 2,
		],
	);
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert_eq!(
		contacts(&parse(&mut parser, &[1, 0x90, 1, 0xf0, 0, 3]))[0].id,
		3
	);
	let next = parse(&mut parser, &[1, 0x90, 1, 0xf0, 0, 4]);
	assert_eq!(
		contacts(&next),
		[
			TouchContact {
				id: 3,
				x: 0.5,
				y: 0.5,
				touching: false
			},
			TouchContact {
				id: 4,
				x: 0.5,
				y: 0.5,
				touching: true
			}
		]
	);
}

#[test]
fn touchpad_dimensions_use_units_and_decimal_exponents() {
	let mut descriptor = TOUCH.to_vec();
	descriptor[3] = 5;
	assert!(HidParser::new(&descriptor).is_err());
	let x = position(&descriptor, &[0x09, 0x30]);
	descriptor.splice(x..x, [0x35, 0, 0x46, 100, 0, 0x55, 0x0e, 0x65, 0x11]);
	let y = position(&descriptor, &[0x09, 0x31]);
	descriptor.splice(y..y, [0x46, 50, 0]);
	let parser = HidParser::new(&descriptor).unwrap();
	assert_eq!(
		parser.supported_touch(),
		Some(InputTouchCapability::Touchpad(InputTouchInfo {
			max_touches: 1,
			physical_size_mm: Some([10.0, 5.0])
		}))
	);
}

#[test]
fn multiple_surfaces_and_fingers_are_explicitly_rejected() {
	let mut first = TOUCH.to_vec();
	first.splice(6..6, [0x85, 1]);
	let mut second = TOUCH.to_vec();
	second.splice(6..6, [0x85, 2]);
	first.extend(second);
	assert!(
		HidParser::new(&first)
			.err()
			.unwrap()
			.to_string()
			.contains("multiple touch surfaces")
	);
	let mut fingers = TOUCH.to_vec();
	let end = fingers.len() - 1;
	fingers.splice(end..end, TOUCH[6..TOUCH.len() - 1].iter().copied());
	assert!(
		HidParser::new(&fingers)
			.err()
			.unwrap()
			.to_string()
			.contains("multiple Finger")
	);
}

#[test]
fn array_selectors_map_through_the_usage_list_and_invalid_reports_preserve_state() {
	let descriptor = [
		0x05, 0x0c, 0x09, 1, 0xa1, 1, 0x09, 0xb0, 0x09, 0xb1, 0x15, 1, 0x25, 2, 0x75, 8, 0x95, 2,
		0x81, 0, 0xc0,
	];
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert_eq!(parser.supported_keys(), [InputKey::Play, InputKey::Pause]);
	assert_eq!(
		parse(&mut parser, &[1, 2]),
		[key(InputKey::Play, true), key(InputKey::Pause, true)]
	);
	assert!(parser.parse(Instant::now(), &[3, 0]).is_err());
	assert_eq!(parse(&mut parser, &[0, 2]), [key(InputKey::Play, false)]);
	assert_eq!(parse(&mut parser, &[0, 0]), [key(InputKey::Pause, false)]);
}

#[test]
fn signed_packed_array_selectors_use_each_elements_width() {
	let descriptor = [
		0x05, 0x0c, 0x09, 1, 0xa1, 1, 0x09, 0xb0, 0x09, 0xb1, 0x15, 0xfe, 0x25, 0xff, 0x75, 4,
		0x95, 2, 0x81, 0, 0xc0,
	];
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert_eq!(
		parse(&mut parser, &[0xfe]),
		[key(InputKey::Play, true), key(InputKey::Pause, true)]
	);
	assert_eq!(parse(&mut parser, &[0xee]), [key(InputKey::Pause, false)]);
}

#[test]
fn keyboard_array_ranges_and_rollover_preserve_held_keys() {
	let descriptor = [
		0x05, 7, 0x09, 6, 0xa1, 1, 0x19, 0, 0x29, 0x65, 0x15, 0, 0x25, 0x65, 0x75, 8, 0x95, 2,
		0x81, 0, 0xc0,
	];
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert_eq!(parser.supported_keys(), [InputKey::Backspace]);
	assert_eq!(
		parse(&mut parser, &[0x2a, 0]),
		[key(InputKey::Backspace, true)]
	);
	assert!(parser.parse(Instant::now(), &[1, 1]).is_err());
	assert_eq!(
		parse(&mut parser, &[0, 0]),
		[key(InputKey::Backspace, false)]
	);
}

#[test]
fn report_ids_cannot_release_each_others_keys() {
	let body = [
		0x09, 0xb0, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0x81, 2, 0x75, 7, 0x81, 1,
	];
	let mut descriptor = vec![0x05, 0x0c, 0x09, 1, 0xa1, 1, 0x85, 1];
	descriptor.extend(body);
	descriptor.extend([0x85, 2]);
	descriptor.extend(body);
	descriptor.push(0xc0);
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert_eq!(parse(&mut parser, &[1, 1]), [key(InputKey::Play, true)]);
	assert!(parse(&mut parser, &[2, 1]).is_empty());
	assert!(parse(&mut parser, &[1, 0]).is_empty());
	assert_eq!(parse(&mut parser, &[2, 0]), [key(InputKey::Play, false)]);
}

#[test]
fn r29_knob_uses_signed_absolute_axes_and_repeated_relative_deltas() {
	let mut parser = HidParser::new(KNOB).unwrap();
	assert_eq!(
		parser.supported_keys(),
		[InputKey::Home, InputKey::Back, InputKey::Primary]
	);
	assert!(parser.supported_touch().is_none());
	let axes = parser.supported_axes();
	assert_eq!(
		axes.iter()
			.map(|axis| (axis.axis, axis.relative))
			.collect::<Vec<_>>(),
		[
			(InputAxis::X, false),
			(InputAxis::Y, false),
			(InputAxis::Wheel, true)
		]
	);
	assert_eq!(
		parse(&mut parser, &[1, 0xff, 0x7f, 0xff]),
		[
			key(InputKey::Primary, true),
			InputEvent::Axis { id: 0, value: -1 },
			InputEvent::Axis { id: 1, value: 127 },
			InputEvent::Axis { id: 2, value: -1 }
		]
	);
	assert_eq!(
		parse(&mut parser, &[1, 0xff, 0x7f, 0xff]),
		[InputEvent::Axis { id: 2, value: -1 }]
	);
	assert_eq!(
		parser.flush(Instant::now()).unwrap().events,
		[
			key(InputKey::Primary, false),
			InputEvent::Axis { id: 0, value: 0 },
			InputEvent::Axis { id: 1, value: 0 }
		]
	);
	assert!(parser.flush(Instant::now()).is_none());
}

#[test]
fn positive_z_axis_translates_to_primary_selection() {
	let descriptor = [
		0x05, 1, 0x09, 8, 0xa1, 1, 0x09, 0x32, 0x15, 0x81, 0x25, 0x7f, 0x75, 8, 0x95, 1, 0x81, 2,
		0xc0,
	];
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert_eq!(parser.supported_keys(), [InputKey::Primary]);
	assert!(parser.supported_axes().is_empty());
	assert_eq!(parse(&mut parser, &[1]), [key(InputKey::Primary, true)]);
	assert_eq!(parse(&mut parser, &[0xff]), [key(InputKey::Primary, false)]);
}

#[test]
fn tip_switch_is_supported_and_flush_releases_the_contact() {
	let mut descriptor = TOUCH.to_vec();
	let touch = position(&descriptor, &[0x09, 0x33]);
	descriptor[touch + 1] = 0x42;
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert!(contacts(&parse(&mut parser, &[1, 0x90, 1, 0xf0, 0]))[0].touching);
	assert_eq!(
		contacts(&parser.flush(Instant::now()).unwrap().events)[0],
		TouchContact {
			id: 0,
			x: 0.5,
			y: 0.5,
			touching: false
		}
	);
	assert!(parser.flush(Instant::now()).is_none());
}

#[test]
fn unsupported_numeric_widths_and_relative_touch_axes_are_rejected_at_registration() {
	let wide_key = [
		0x05, 0x0c, 0x09, 1, 0xa1, 1, 0x09, 0xb0, 0x15, 0, 0x25, 1, 0x75, 64, 0x95, 1, 0x81, 2,
		0xc0,
	];
	assert!(HidParser::new(&wide_key).is_err());
	let mut descriptor = TOUCH.to_vec();
	let x = position(&descriptor, &[0x09, 0x30]);
	let input = x + position(&descriptor[x..], &[0x81, 2]);
	descriptor[input + 1] = 6;
	assert!(HidParser::new(&descriptor).is_err());
}

#[test]
fn unrelated_vendor_data_is_ignored_without_extracting_wide_fields() {
	let descriptor = [
		0x06, 0, 0xff, 0x09, 1, 0xa1, 1, 0x09, 2, 0x75, 64, 0x95, 1, 0x81, 2, 0xc0,
	];
	let mut parser = HidParser::new(&descriptor).unwrap();
	assert!(parse(&mut parser, &[0; 8]).is_empty());
	assert!(parser.supported_keys().is_empty());
}
