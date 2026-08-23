//! Control-session message codec (spec §56.3.2).
//!
//! Wire format: `40 40 <len16 BE> <msgID16 BE> <parameter…>`, where each parameter is
//! `<len16 BE> <paramID16 BE> <data>` and every length is inclusive of its own field.
//! Group parameters (§56.3.2.3.10) nest that same triple recursively.
//!
//! A parameter's *type* is not on the wire — it is implied by the `(message id,
//! parameter id)` pair (§56.3.2.1: "Type of the data is determined by the parameter ID
//! for this specific message ID"). So a decoded [`Param`] holds raw bytes and you pick
//! the interpretation with the typed accessors ([`Param::as_u16`], [`Param::as_utf8`],
//! [`Param::as_group`], …). Encoding goes the other way, through the typed constructors.

pub mod ids;

use std::fmt;

use bytes::{BufMut, Bytes, BytesMut};

use crate::iap2::error::MessageError;

/// Start-of-message marker (§56.3.2.1).
pub const SOM: [u8; 2] = [0x40, 0x40];

/// Bytes a message occupies before its first parameter.
const MSG_HEADER: usize = 6;
/// Bytes a parameter occupies before its data.
const PARAM_HEADER: usize = 4;

/// One control-session message parameter, or group sub-parameter.
///
/// The data is kept verbatim; use the `as_*` accessors to read it as the type the spec
/// assigns to this parameter of this message.
#[derive(Clone, PartialEq, Eq)]
pub struct Param {
	pub id: u16,
	pub data: Bytes,
}

macro_rules! int_accessors {
	($($ty:ident, $ctor:ident, $get:ident, $n:literal;)*) => {
		$(
			#[doc = concat!("Build a `", stringify!($ty), "` parameter.")]
			pub fn $ctor(id: u16, value: $ty) -> Self {
				Self { id, data: Bytes::copy_from_slice(&value.to_be_bytes()) }
			}

			#[doc = concat!("Read the data as a `", stringify!($ty), "`.")]
			pub fn $get(&self) -> Result<$ty, MessageError> {
				let bytes: [u8; $n] = self.fixed()?;
				Ok($ty::from_be_bytes(bytes))
			}
		)*
	};
}

impl Param {
	/// A `none` parameter (§56.3.2.3.9): presence alone carries the meaning.
	pub fn none(id: u16) -> Self {
		Self {
			id,
			data: Bytes::new(),
		}
	}

	/// A `bool` parameter (§56.3.2.3.4): one byte, 0 = NO, 1 = YES.
	pub fn bool(id: u16, value: bool) -> Self {
		Self {
			id,
			data: Bytes::copy_from_slice(&[u8::from(value)]),
		}
	}

	/// A `utf8` parameter (§56.3.2.3.7): NUL-terminated UTF-8.
	pub fn utf8(id: u16, value: &str) -> Self {
		let mut data = BytesMut::with_capacity(value.len() + 1);
		data.put_slice(value.as_bytes());
		data.put_u8(0);
		Self {
			id,
			data: data.freeze(),
		}
	}

	/// A `blob` parameter (§56.3.2.3.8).
	pub fn blob(id: u16, value: impl Into<Bytes>) -> Self {
		Self {
			id,
			data: value.into(),
		}
	}

	/// A `group` parameter (§56.3.2.3.10): sub-parameters, serialized now.
	pub fn group(id: u16, sub: &[Param]) -> Self {
		let mut data = BytesMut::new();
		encode_params(&mut data, sub);
		Self {
			id,
			data: data.freeze(),
		}
	}

	int_accessors! {
		u8,  u8_,  as_u8,  1;
		u16, u16_, as_u16, 2;
		u32, u32_, as_u32, 4;
		u64, u64_, as_u64, 8;
		i8,  i8_,  as_i8,  1;
		i16, i16_, as_i16, 2;
		i32, i32_, as_i32, 4;
		i64, i64_, as_i64, 8;
	}

	/// Whether this is a `none` parameter.
	pub fn is_none(&self) -> bool {
		self.data.is_empty()
	}

	/// Read the data as a `bool`.
	pub fn as_bool(&self) -> Result<bool, MessageError> {
		Ok(self.as_u8()? != 0)
	}

	/// Read the data as a UTF-8 string, dropping the NUL terminator if present.
	pub fn as_utf8(&self) -> Result<&str, MessageError> {
		let bytes = self.data.strip_suffix(&[0u8]).unwrap_or(&self.data);
		str::from_utf8(bytes).map_err(|_| MessageError::BadUtf8(self.id))
	}

	/// Read the data as group sub-parameters.
	pub fn as_group(&self) -> Result<Vec<Param>, MessageError> {
		decode_params(&self.data)
	}

	/// Find a sub-parameter of a group by id.
	pub fn sub(&self, id: u16) -> Result<Option<Param>, MessageError> {
		Ok(self.as_group()?.into_iter().find(|p| p.id == id))
	}

	fn fixed<const N: usize>(&self) -> Result<[u8; N], MessageError> {
		self.data
			.as_ref()
			.try_into()
			.map_err(|_| MessageError::BadParamWidth {
				id: self.id,
				len: self.data.len(),
				want: N,
			})
	}

	fn encoded_len(&self) -> usize {
		PARAM_HEADER + self.data.len()
	}
}

impl fmt::Debug for Param {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "param {:#06x} ", self.id)?;
		match self.as_utf8() {
			// Printable NUL-terminated text is almost always what it looks like.
			Ok(s) if !s.is_empty() && s.chars().all(|c| !c.is_control()) => write!(f, "{s:?}"),
			_ if self.data.is_empty() => write!(f, "(none)"),
			_ => write!(f, "{:02x?}", self.data.as_ref()),
		}
	}
}

/// A decoded control-session message.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
	pub id: u16,
	pub params: Vec<Param>,
}

impl Message {
	/// A message with no parameters.
	pub fn new(id: u16) -> Self {
		Self {
			id,
			params: Vec::new(),
		}
	}

	/// A message with the given parameters.
	pub fn with(id: u16, params: Vec<Param>) -> Self {
		Self { id, params }
	}

	/// Add a parameter, builder style.
	pub fn param(mut self, param: Param) -> Self {
		self.params.push(param);
		self
	}

	/// The first parameter with this id, if present.
	pub fn get(&self, id: u16) -> Option<&Param> {
		self.params.iter().find(|p| p.id == id)
	}

	/// The spec name of this message, if it is one we know.
	pub fn name(&self) -> Option<&'static str> {
		ids::name(self.id)
	}

	/// Serialize to wire bytes.
	pub fn encode(&self) -> Result<Bytes, MessageError> {
		let total = MSG_HEADER + self.params.iter().map(Param::encoded_len).sum::<usize>();
		let len = u16::try_from(total).map_err(|_| MessageError::TooLong)?;
		let mut buf = BytesMut::with_capacity(total);
		buf.put_slice(&SOM);
		buf.put_u16(len);
		buf.put_u16(self.id);
		encode_params(&mut buf, &self.params);
		Ok(buf.freeze())
	}

	/// Decode one message from the front of `buf`, returning it and the bytes it used.
	pub fn decode_one(buf: &[u8]) -> Result<(Self, usize), MessageError> {
		if buf.len() < MSG_HEADER || buf[..2] != SOM {
			return Err(MessageError::BadSom);
		}
		let declared = u16::from_be_bytes([buf[2], buf[3]]);
		let total = declared as usize;
		if total < MSG_HEADER || total > buf.len() {
			return Err(MessageError::BadLength {
				declared,
				actual: buf.len(),
			});
		}
		let id = u16::from_be_bytes([buf[4], buf[5]]);
		let params = decode_params(&buf[MSG_HEADER..total])?;
		Ok((Self { id, params }, total))
	}

	/// Decode a whole session payload, which may hold more than one message.
	///
	/// Stops at the first byte that is not a message start, so trailing padding is
	/// tolerated; returns an error only if a message itself is malformed.
	pub fn decode_all(buf: &[u8]) -> Result<Vec<Self>, MessageError> {
		let mut out = Vec::new();
		let mut rest = buf;
		while rest.len() >= MSG_HEADER && rest[..2] == SOM {
			let (msg, used) = Self::decode_one(rest)?;
			out.push(msg);
			rest = &rest[used..];
		}
		Ok(out)
	}
}

impl fmt::Debug for Message {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.name() {
			Some(name) => write!(f, "{name} ({:#06x})", self.id)?,
			None => write!(f, "unknown message {:#06x}", self.id)?,
		}
		if self.params.is_empty() {
			return Ok(());
		}
		write!(f, " {:?}", self.params)
	}
}

fn encode_params(buf: &mut BytesMut, params: &[Param]) {
	for p in params {
		// Callers reach this only via Message::encode, which has already bounded the
		// total; a single parameter therefore also fits.
		buf.put_u16(p.encoded_len() as u16);
		buf.put_u16(p.id);
		buf.put_slice(&p.data);
	}
}

/// Decode a parameter list. Per §56.3.2.2 parameters may appear in any order, so the
/// caller looks them up by id rather than relying on position.
fn decode_params(mut buf: &[u8]) -> Result<Vec<Param>, MessageError> {
	let mut out = Vec::new();
	while !buf.is_empty() {
		if buf.len() < PARAM_HEADER {
			return Err(MessageError::BadParamLength(buf.len() as u16));
		}
		let len = u16::from_be_bytes([buf[0], buf[1]]);
		let total = len as usize;
		if total < PARAM_HEADER || total > buf.len() {
			return Err(MessageError::BadParamLength(len));
		}
		out.push(Param {
			id: u16::from_be_bytes([buf[2], buf[3]]),
			data: Bytes::copy_from_slice(&buf[PARAM_HEADER..total]),
		});
		buf = &buf[total..];
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parameterless_message_layout() {
		let m = Message::new(ids::ID_REQUEST_AUTHENTICATION_CERTIFICATE);
		assert_eq!(
			&m.encode().unwrap()[..],
			&[0x40, 0x40, 0x00, 0x06, 0xAA, 0x00]
		);
	}

	#[test]
	fn blob_parameter_layout() {
		// RequestAuthenticationChallengeResponse with a 20-byte challenge:
		// 40 40 | len=0x001E | AA 02 | plen=0x0018 | pid=0x0000 | 20 bytes
		let m = Message::new(ids::ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE)
			.param(Param::blob(0, vec![0xA5; 20]));
		let bytes = m.encode().unwrap();
		assert_eq!(
			&bytes[..10],
			&[0x40, 0x40, 0x00, 0x1E, 0xAA, 0x02, 0x00, 0x18, 0x00, 0x00]
		);
		assert_eq!(&bytes[10..], &[0xA5; 20]);
	}

	#[test]
	fn utf8_parameter_is_nul_terminated() {
		let m = Message::new(ids::ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION)
			.param(Param::utf8(1, "usb0"));
		let bytes = m.encode().unwrap();
		assert_eq!(
			&bytes[..],
			&[
				0x40, 0x40, 0x00, 0x0F, 0x4E, 0x0E, 0x00, 0x09, 0x00, 0x01, b'u', b's', b'b', b'0',
				0x00
			]
		);
		let back = Message::decode_one(&bytes).unwrap().0;
		assert_eq!(back.get(1).unwrap().as_utf8().unwrap(), "usb0");
	}

	#[test]
	fn round_trips_every_scalar_type() {
		let m = Message::with(
			0x1234,
			vec![
				Param::none(0),
				Param::bool(1, true),
				Param::u8_(2, 0x7F),
				Param::u16_(3, 0xBEEF),
				Param::u32_(4, 0xDEAD_BEEF),
				Param::u64_(5, u64::MAX),
				Param::i16_(6, -2),
				Param::i32_(7, i32::MIN),
			],
		);
		let back = Message::decode_one(&m.encode().unwrap()).unwrap().0;
		assert_eq!(back, m);
		assert!(back.get(0).unwrap().is_none());
		assert!(back.get(1).unwrap().as_bool().unwrap());
		assert_eq!(back.get(2).unwrap().as_u8().unwrap(), 0x7F);
		assert_eq!(back.get(3).unwrap().as_u16().unwrap(), 0xBEEF);
		assert_eq!(back.get(4).unwrap().as_u32().unwrap(), 0xDEAD_BEEF);
		assert_eq!(back.get(5).unwrap().as_u64().unwrap(), u64::MAX);
		assert_eq!(back.get(6).unwrap().as_i16().unwrap(), -2);
		assert_eq!(back.get(7).unwrap().as_i32().unwrap(), i32::MIN);
	}

	#[test]
	fn nested_groups_round_trip() {
		let inner = Param::group(9, &[Param::utf8(0, "deep"), Param::u8_(1, 3)]);
		let m = Message::with(
			0x1D01,
			vec![Param::group(2, &[Param::utf8(0, "outer"), inner.clone()])],
		);
		let back = Message::decode_one(&m.encode().unwrap()).unwrap().0;
		let outer = back.get(2).unwrap().as_group().unwrap();
		assert_eq!(outer[0].as_utf8().unwrap(), "outer");
		assert_eq!(outer[1], inner);
		let deep = outer[1].as_group().unwrap();
		assert_eq!(deep[0].as_utf8().unwrap(), "deep");
		assert_eq!(deep[1].as_u8().unwrap(), 3);
	}

	#[test]
	fn wrong_width_is_an_error_not_a_panic() {
		let p = Param::u8_(7, 1);
		assert_eq!(
			p.as_u32(),
			Err(MessageError::BadParamWidth {
				id: 7,
				len: 1,
				want: 4
			})
		);
	}

	#[test]
	fn rejects_bad_framing() {
		assert_eq!(
			Message::decode_one(&[0x40, 0x40]),
			Err(MessageError::BadSom)
		);
		assert_eq!(
			Message::decode_one(&[0x00, 0x40, 0, 6, 0xAA, 0x00]),
			Err(MessageError::BadSom)
		);
		// Declared length longer than the buffer.
		assert_eq!(
			Message::decode_one(&[0x40, 0x40, 0x00, 0x20, 0xAA, 0x00]),
			Err(MessageError::BadLength {
				declared: 0x20,
				actual: 6
			})
		);
		// Parameter length shorter than its own header.
		assert_eq!(
			Message::decode_one(&[0x40, 0x40, 0x00, 0x0A, 0xAA, 0x00, 0x00, 0x02, 0x00, 0x00]),
			Err(MessageError::BadParamLength(2))
		);
	}

	#[test]
	fn decodes_several_messages_from_one_payload() {
		let mut buf = Vec::new();
		buf.extend_from_slice(
			&Message::new(ids::ID_AUTHENTICATION_SUCCEEDED)
				.encode()
				.unwrap(),
		);
		buf.extend_from_slice(&Message::new(ids::ID_START_IDENTIFICATION).encode().unwrap());
		buf.extend_from_slice(&[0x00, 0x00, 0x00]); // trailing padding
		let msgs = Message::decode_all(&buf).unwrap();
		assert_eq!(msgs.len(), 2);
		assert_eq!(msgs[0].id, ids::ID_AUTHENTICATION_SUCCEEDED);
		assert_eq!(msgs[1].id, ids::ID_START_IDENTIFICATION);
	}

	#[test]
	fn debug_uses_spec_names() {
		let m = Message::new(ids::ID_IDENTIFICATION_ACCEPTED);
		assert_eq!(format!("{m:?}"), "IdentificationAccepted (0x1d02)");
	}
}
