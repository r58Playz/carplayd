//! Type-directed CarPlay scalar decoding over an existing Serde deserializer.
//!
//! Only requests for numbers or booleans coerce strings. `deserialize_any` preserves
//! raw plist values, and strings, data, dates and UIDs keep their original meaning.
//! Types that buffer through `deserialize_any` (such as untagged enums) need a typed
//! decode of their payload afterwards, as the command dispatcher does.

use std::fmt;

use serde::{
	Deserialize, Deserializer,
	de::{
		self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
	},
};

pub(super) fn from_value<T: DeserializeOwned>(value: &plist::Value) -> Result<T, plist::Error> {
	plist::from_value::<Flexible<T>>(value).map(|value| value.0)
}

struct Flexible<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Flexible<T> {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		T::deserialize(Adapter(deserializer)).map(Self)
	}
}

struct Adapter<D>(D);

#[derive(Clone, Copy)]
enum Scalar {
	Unchanged,
	Integer,
	Float,
	Boolean,
}

struct Visitor<V> {
	inner: V,
	scalar: Scalar,
}

impl<V> Visitor<V> {
	fn unchanged(inner: V) -> Self {
		Self {
			inner,
			scalar: Scalar::Unchanged,
		}
	}
}

macro_rules! deserialize {
	($($method:ident $(($($arg:ident: $ty:ty),*))? => $scalar:ident),* $(,)?) => {
		$(
			fn $method<V: de::Visitor<'de>>(
				self, $($($arg: $ty,)*)? visitor: V,
			) -> Result<V::Value, Self::Error> {
				self.0.$method($($($arg,)*)? Visitor { inner: visitor, scalar: Scalar::$scalar })
			}
		)*
	};
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Adapter<D> {
	type Error = D::Error;

	deserialize! {
		deserialize_any => Unchanged,
		deserialize_bool => Boolean,
		deserialize_i8 => Integer,
		deserialize_i16 => Integer,
		deserialize_i32 => Integer,
		deserialize_i64 => Integer,
		deserialize_u8 => Integer,
		deserialize_u16 => Integer,
		deserialize_u32 => Integer,
		deserialize_u64 => Integer,
		deserialize_f32 => Float,
		deserialize_f64 => Float,
		deserialize_char => Unchanged,
		deserialize_str => Unchanged,
		deserialize_string => Unchanged,
		deserialize_bytes => Unchanged,
		deserialize_byte_buf => Unchanged,
		deserialize_option => Unchanged,
		deserialize_unit => Unchanged,
		deserialize_unit_struct(name: &'static str) => Unchanged,
		deserialize_newtype_struct(name: &'static str) => Unchanged,
		deserialize_seq => Unchanged,
		deserialize_tuple(len: usize) => Unchanged,
		deserialize_tuple_struct(name: &'static str, len: usize) => Unchanged,
		deserialize_map => Unchanged,
		deserialize_struct(name: &'static str, fields: &'static [&'static str]) => Unchanged,
		deserialize_enum(name: &'static str, variants: &'static [&'static str]) => Unchanged,
		deserialize_identifier => Unchanged,
		deserialize_ignored_any => Unchanged,
	}

	// `plist` has no native 128-bit integers, but a string may still contain one.
	fn deserialize_i128<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
		self.0.deserialize_any(Visitor {
			inner: visitor,
			scalar: Scalar::Integer,
		})
	}

	fn deserialize_u128<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
		self.0.deserialize_any(Visitor {
			inner: visitor,
			scalar: Scalar::Integer,
		})
	}

	fn is_human_readable(&self) -> bool {
		self.0.is_human_readable()
	}
}

enum Integer {
	Signed(i128),
	Unsigned(u128),
}

impl Integer {
	fn nonzero(&self) -> bool {
		match self {
			Self::Signed(n) => *n != 0,
			Self::Unsigned(n) => *n != 0,
		}
	}

	fn visit<'de, V: de::Visitor<'de>, E: de::Error>(self, visitor: V) -> Result<V::Value, E> {
		match self {
			Self::Signed(n) => match i64::try_from(n) {
				Ok(n) => visitor.visit_i64(n),
				Err(_) => visitor.visit_i128(n),
			},
			Self::Unsigned(n) => match u64::try_from(n) {
				Ok(n) => visitor.visit_u64(n),
				Err(_) => visitor.visit_u128(n),
			},
		}
	}
}

// The Prius ships hex bitfields with trailing configuration comments. Strip those
// only while reading a scalar; never edit the original plist or a string field.
fn scalar_text(text: &str) -> &str {
	text.split_once('#').map_or(text, |(value, _)| value).trim()
}

fn integer(text: &str) -> Option<Integer> {
	if let Ok(n) = text.parse::<i64>() {
		return Some(Integer::Signed(n.into()));
	}
	if let Ok(n) = text.parse::<u64>() {
		return Some(Integer::Unsigned(n.into()));
	}
	if let Ok(n) = text.parse::<i128>() {
		return Some(Integer::Signed(n));
	}
	if let Ok(n) = text.parse::<u128>() {
		return Some(Integer::Unsigned(n));
	}

	let negative = text.starts_with('-');
	let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
	let digits = digits
		.strip_prefix("0x")
		.or_else(|| digits.strip_prefix("0X"))?;
	if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
		return None;
	}
	let magnitude = u128::from_str_radix(digits, 16).ok()?;
	if negative {
		let n = if magnitude == (1u128 << 127) {
			i128::MIN
		} else {
			i128::try_from(magnitude).ok()?.checked_neg()?
		};
		Some(Integer::Signed(n))
	} else {
		Some(Integer::Unsigned(magnitude))
	}
}

macro_rules! visit_integer {
	($($method:ident: $ty:ty),* $(,)?) => {
		$(
			fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
				if matches!(self.scalar, Scalar::Boolean) {
					self.inner.visit_bool(value != 0)
				} else {
					self.inner.$method(value)
				}
			}
		)*
	};
}

macro_rules! visit_scalar {
	($($method:ident: $ty:ty),* $(,)?) => {
		$(
			fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
				self.inner.$method(value)
			}
		)*
	};
}

impl<'de, V: de::Visitor<'de>> de::Visitor<'de> for Visitor<V> {
	type Value = V::Value;

	fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
		self.inner.expecting(formatter)
	}

	visit_integer! {
		visit_i8: i8, visit_i16: i16, visit_i32: i32, visit_i64: i64, visit_i128: i128,
		visit_u8: u8, visit_u16: u16, visit_u32: u32, visit_u64: u64, visit_u128: u128,
	}

	visit_scalar! {
		visit_bool: bool, visit_f32: f32, visit_f64: f64, visit_char: char,
		visit_bytes: &[u8], visit_borrowed_bytes: &'de [u8], visit_byte_buf: Vec<u8>,
	}

	fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
		let text = scalar_text(value);
		match self.scalar {
			Scalar::Unchanged => self.inner.visit_str(value),
			Scalar::Integer => match integer(text) {
				Some(n) => n.visit(self.inner),
				None => Err(E::invalid_value(de::Unexpected::Str(value), &self)),
			},
			Scalar::Float => {
				if let Ok(n) = text.parse::<f64>() {
					self.inner.visit_f64(n)
				} else if let Some(n) = integer(text) {
					n.visit(self.inner)
				} else {
					Err(E::invalid_value(de::Unexpected::Str(value), &self))
				}
			}
			Scalar::Boolean => {
				if text.eq_ignore_ascii_case("true") {
					self.inner.visit_bool(true)
				} else if text.eq_ignore_ascii_case("false") {
					self.inner.visit_bool(false)
				} else if let Some(n) = integer(text) {
					self.inner.visit_bool(n.nonzero())
				} else {
					Err(E::invalid_value(de::Unexpected::Str(value), &self))
				}
			}
		}
	}

	fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
		if matches!(self.scalar, Scalar::Unchanged) {
			self.inner.visit_borrowed_str(value)
		} else {
			self.visit_str(value)
		}
	}

	fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
		if matches!(self.scalar, Scalar::Unchanged) {
			self.inner.visit_string(value)
		} else {
			self.visit_str(&value)
		}
	}

	fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
		self.inner.visit_unit()
	}

	fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
		self.inner.visit_none()
	}

	fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
		self.inner.visit_some(Adapter(deserializer))
	}

	fn visit_newtype_struct<D: Deserializer<'de>>(
		self,
		deserializer: D,
	) -> Result<Self::Value, D::Error> {
		self.inner.visit_newtype_struct(Adapter(deserializer))
	}

	fn visit_seq<A: SeqAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
		self.inner.visit_seq(Adapter(access))
	}

	fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
		self.inner.visit_map(Adapter(access))
	}

	fn visit_enum<A: EnumAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
		self.inner.visit_enum(Adapter(access))
	}
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Adapter<S> {
	type Value = S::Value;

	fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
		self.0.deserialize(Adapter(deserializer))
	}
}

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Adapter<A> {
	type Error = A::Error;

	fn next_element_seed<S: DeserializeSeed<'de>>(
		&mut self,
		seed: S,
	) -> Result<Option<S::Value>, Self::Error> {
		self.0.next_element_seed(Adapter(seed))
	}

	fn size_hint(&self) -> Option<usize> {
		self.0.size_hint()
	}
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for Adapter<A> {
	type Error = A::Error;

	fn next_key_seed<S: DeserializeSeed<'de>>(
		&mut self,
		seed: S,
	) -> Result<Option<S::Value>, Self::Error> {
		self.0.next_key_seed(Adapter(seed))
	}

	fn next_value_seed<S: DeserializeSeed<'de>>(
		&mut self,
		seed: S,
	) -> Result<S::Value, Self::Error> {
		self.0.next_value_seed(Adapter(seed))
	}

	fn size_hint(&self) -> Option<usize> {
		self.0.size_hint()
	}
}

impl<'de, A: EnumAccess<'de>> EnumAccess<'de> for Adapter<A> {
	type Error = A::Error;
	type Variant = Adapter<A::Variant>;

	fn variant_seed<S: DeserializeSeed<'de>>(
		self,
		seed: S,
	) -> Result<(S::Value, Self::Variant), Self::Error> {
		self.0
			.variant_seed(Adapter(seed))
			.map(|(value, variant)| (value, Adapter(variant)))
	}
}

impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Adapter<A> {
	type Error = A::Error;

	fn unit_variant(self) -> Result<(), Self::Error> {
		self.0.unit_variant()
	}

	fn newtype_variant_seed<S: DeserializeSeed<'de>>(
		self,
		seed: S,
	) -> Result<S::Value, Self::Error> {
		self.0.newtype_variant_seed(Adapter(seed))
	}

	fn tuple_variant<V: de::Visitor<'de>>(
		self,
		len: usize,
		visitor: V,
	) -> Result<V::Value, Self::Error> {
		self.0.tuple_variant(len, Visitor::unchanged(visitor))
	}

	fn struct_variant<V: de::Visitor<'de>>(
		self,
		fields: &'static [&'static str],
		visitor: V,
	) -> Result<V::Value, Self::Error> {
		self.0.struct_variant(fields, Visitor::unchanged(visitor))
	}
}

/// Some head units use integers for HID identifiers. This extra compatibility is
/// explicit: ordinary string fields must not accept arbitrary numeric values.
pub fn string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
	match plist::Value::deserialize(deserializer)? {
		plist::Value::String(s) => Ok(s),
		plist::Value::Integer(i) => i
			.as_signed()
			.map(|n| n.to_string())
			.or_else(|| i.as_unsigned().map(|n| n.to_string()))
			.ok_or_else(|| de::Error::custom("integer out of range for an identifier")),
		other => Err(de::Error::custom(format!(
			"expected a string identifier, got {other:?}"
		))),
	}
}

#[cfg(test)]
mod tests {
	use std::{collections::BTreeMap, fmt::Debug};

	use plist::{Data, Date, Uid, Value};
	use serde::{Deserialize, Serialize};

	use super::*;
	use crate::carplay::net::proto::{self, modes::Entity};

	fn text(value: &str) -> Value {
		Value::String(value.into())
	}

	fn dict<const N: usize>(fields: [(&str, Value); N]) -> Value {
		Value::Dictionary(
			fields
				.into_iter()
				.map(|(key, value)| (key.to_owned(), value))
				.collect(),
		)
	}

	// Exercise both public entry points and both wire encodings.
	fn assert_decodes<T: DeserializeOwned + PartialEq + Debug>(value: &Value, expected: T) {
		assert_eq!(proto::from_value::<T>(value).unwrap(), expected);
		let binary = proto::to_binary(value).unwrap();
		assert_eq!(proto::from_binary::<T>(&binary).unwrap(), expected);
		let mut xml = Vec::new();
		plist::to_writer_xml(&mut xml, value).unwrap();
		assert_eq!(proto::from_binary::<T>(&xml).unwrap(), expected);
	}

	#[test]
	fn strings_and_raw_plists_keep_their_original_values() {
		let value = dict([
			("1234", text("1234")),
			("true", text("true")),
			("false", text("false")),
			("hex", text("0x54")),
			("negative", text("-7")),
			("padded", text("007")),
			("comment", text("0x4000 # a literal string")),
			("space", text(" 1 ")),
		]);
		let expected: BTreeMap<String, String> = value
			.as_dictionary()
			.unwrap()
			.iter()
			.map(|(key, value)| (key.clone(), value.as_string().unwrap().into()))
			.collect();
		assert_decodes(&value, expected);
		assert_decodes(&value, value.clone());
		assert!(proto::from_value::<String>(&Value::Integer(1.into())).is_err());
	}

	#[test]
	fn scalar_strings_accept_decimal_hex_float_and_boolean_forms() {
		for value in ["007", "+7", " 7 ", "0x7", "0X07", "7 # comment"] {
			assert_decodes(&text(value), 7i64);
		}
		assert_decodes(&text("-0x80"), -128i8);
		assert_decodes(&text("-0x8000000000000000"), i64::MIN);
		assert_decodes(&text("18446744073709551615"), u64::MAX);
		assert_decodes(&text("0xffffffffffffffff"), u64::MAX);
		for (value, expected) in [("1.25", 1.25), ("-2.5e-3", -0.0025), ("0x54", 84.0)] {
			assert_decodes(&text(value), expected);
		}
		for (value, expected) in [
			("true", true),
			("FALSE", false),
			("1", true),
			("0", false),
			("-1", true),
			("0x0", false),
		] {
			assert_decodes(&text(value), expected);
		}
		assert_decodes(&Value::Integer((-1).into()), true);
		assert_decodes(&Value::Integer(0.into()), false);
	}

	#[test]
	fn integer_ranges_and_invalid_scalars_are_rejected() {
		for value in ["128", "-129", "1.5", "1e3", "0x+1", "", "no number"] {
			assert!(
				proto::from_value::<i8>(&text(value)).is_err(),
				"accepted {value:?}"
			);
		}
		assert!(proto::from_value::<u64>(&text("-1")).is_err());
		assert!(proto::from_value::<u64>(&text("18446744073709551616")).is_err());
		assert!(proto::from_value::<i64>(&text("9223372036854775808")).is_err());
		assert!(proto::from_value::<i64>(&Value::Real(1.5)).is_err());
		assert!(proto::from_value::<i64>(&Value::Boolean(true)).is_err());
		assert!(proto::from_value::<bool>(&text("maybe")).is_err());
		assert!(proto::from_value::<f64>(&text("1.5garbage")).is_err());
		assert_decodes(&text(&i128::MIN.to_string()), i128::MIN);
		assert_decodes(&text(&u128::MAX.to_string()), u128::MAX);
	}

	#[test]
	fn flexibility_reaches_nested_structs_maps_options_and_numeric_enums() {
		#[derive(Debug, Deserialize, PartialEq)]
		struct Stream {
			port: u16,
			rate: f64,
			enabled: bool,
			owner: Entity,
			optional: Option<bool>,
			absent: Option<i64>,
		}
		#[derive(Debug, Deserialize, PartialEq)]
		struct Session {
			streams: BTreeMap<String, Vec<Stream>>,
		}
		let stream = dict([
			("port", text("6768")),
			("rate", text("48000.5")),
			("enabled", Value::Integer(1.into())),
			("owner", text("2")),
			("optional", text("false")),
			(
				"unknown",
				Value::Array(vec![text("1234"), Value::Data(vec![1, 2])]),
			),
		]);
		let value = dict([("streams", dict([("007", Value::Array(vec![stream]))]))]);
		assert_decodes(
			&value,
			Session {
				streams: BTreeMap::from([(
					"007".into(),
					vec![Stream {
						port: 6768,
						rate: 48000.5,
						enabled: true,
						owner: Entity::Accessory,
						optional: Some(false),
						absent: None,
					}],
				)]),
			},
		);
		assert_decodes(&text("-7"), Entity::Other(-7));
	}

	#[test]
	fn flexibility_reaches_newtypes_tuples_and_enum_payloads() {
		#[derive(Debug, Deserialize, PartialEq)]
		struct Count(u16);
		#[derive(Debug, Deserialize, PartialEq)]
		enum Event {
			Idle,
			Count(Count),
			Pair(i16, bool),
			Named { rate: f64, active: bool },
		}
		assert_decodes(&text("Idle"), Event::Idle);
		assert_decodes(&dict([("Count", text("007"))]), Event::Count(Count(7)));
		assert_decodes(
			&dict([("Pair", Value::Array(vec![text("-7"), text("1")]))]),
			Event::Pair(-7, true),
		);
		assert_decodes(
			&dict([(
				"Named",
				dict([("rate", text("1.5")), ("active", text("true"))]),
			)]),
			Event::Named {
				rate: 1.5,
				active: true,
			},
		);
		assert_decodes(&Value::Array(vec![text("007"), text("0")]), (7u16, false));
	}

	#[test]
	fn explicit_container_options_keep_plist_encoding_semantics() {
		let value = plist::to_value(&vec![Some("7"), None, Some("-1")]).unwrap();
		assert_decodes(&value, vec![Some(7i64), None, Some(-1)]);
		let value = plist::to_value(&BTreeMap::from([("true", Some("0")), ("007", None)])).unwrap();
		assert_decodes(
			&value,
			BTreeMap::from([("true".to_string(), Some(false)), ("007".to_string(), None)]),
		);
	}

	#[test]
	fn plist_data_dates_and_uids_survive_the_adapter() {
		#[derive(Debug, Deserialize, PartialEq, Serialize)]
		struct Payload {
			data: Data,
			date: Date,
			uid: Uid,
			raw: Value,
		}
		let date = Date::from_xml_format("2026-10-04T12:00:00Z").unwrap();
		let uid = Uid::new(7);
		let raw = dict([
			("data", Value::Data(vec![0, 255, 1])),
			("date", Value::Date(date)),
			("uid", Value::Uid(uid)),
			("text", text("true")),
		]);
		let expected = Payload {
			data: Data::new(vec![0, 255, 1]),
			date,
			uid,
			raw,
		};
		let value = plist::to_value(&expected).unwrap();
		// Binary plists represent UIDs directly; XML has no native UID node.
		assert_eq!(proto::from_value::<Payload>(&value).unwrap(), expected);
		assert_eq!(
			proto::from_binary::<Payload>(&proto::to_binary(&value).unwrap()).unwrap(),
			expected
		);
	}
}
