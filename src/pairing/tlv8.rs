//! HAP TLV8 items, per HomeKit Accessory Protocol R13 §16.1.
//!
//! §30.3.2.1 sends the pairing exchange as `application/pairing+tlv8` bodies on
//! `/pair-setup` and `/pair-verify`, so the wire format is HomeKit's rather than
//! anything CarPlay-specific.

use anyhow::{Result, bail};

/// Item types, HAP R13 Table 5-17.
pub mod ty {
	pub const METHOD: u8 = 0x00;
	pub const IDENTIFIER: u8 = 0x01;
	pub const SALT: u8 = 0x02;
	pub const PUBLIC_KEY: u8 = 0x03;
	pub const PROOF: u8 = 0x04;
	pub const ENCRYPTED_DATA: u8 = 0x05;
	pub const STATE: u8 = 0x06;
	pub const ERROR: u8 = 0x07;
	pub const RETRY_DELAY: u8 = 0x08;
	pub const CERTIFICATE: u8 = 0x09;
	pub const SIGNATURE: u8 = 0x0A;
	pub const PERMISSIONS: u8 = 0x0B;
	pub const FRAGMENT_DATA: u8 = 0x0C;
	pub const FRAGMENT_LAST: u8 = 0x0D;
	pub const FLAGS: u8 = 0x13;
	pub const SEPARATOR: u8 = 0xFF;
}

/// Pairing methods, HAP R13 Table 5-14.
///
/// CarPlay uses [`PAIR_SETUP`] rather than [`PAIR_SETUP_WITH_AUTH`]: §30.3.2.1 calls it
/// "rudimentary 'unauthenticated' pair-setup with the fixed PIN provided by the device",
/// and head unit logs report it as `pair-setup HK, type 0`. The MFi coprocessor is not
/// involved here at all; that exchange lives on `/auth-setup` (§30.3.2.3).
pub const PAIR_SETUP: u8 = 0;
#[allow(dead_code)]
pub const PAIR_SETUP_WITH_AUTH: u8 = 1;
#[allow(dead_code)]
pub const PAIR_VERIFY: u8 = 2;

/// The single value the largest item may occupy before it must be fragmented.
const MAX_FRAGMENT: usize = 255;

/// A decoded pairing error, HAP R13 Table 5-16.
pub fn error_name(code: u8) -> &'static str {
	match code {
		0x01 => "kTLVError_Unknown",
		0x02 => "kTLVError_Authentication",
		0x03 => "kTLVError_Backoff",
		0x04 => "kTLVError_MaxPeers",
		0x05 => "kTLVError_MaxTries",
		0x06 => "kTLVError_Unavailable",
		0x07 => "kTLVError_Busy",
		_ => "reserved",
	}
}

/// An ordered list of TLV8 items.
///
/// Insertion order is preserved on the wire. HAP does not require a particular order,
/// but keeping ours stable makes captures comparable against a real device.
#[derive(Default, Clone)]
pub struct Tlv8 {
	items: Vec<(u8, Vec<u8>)>,
}

impl Tlv8 {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn push(&mut self, ty: u8, value: impl Into<Vec<u8>>) -> &mut Self {
		self.items.push((ty, value.into()));
		self
	}

	pub fn push_u8(&mut self, ty: u8, value: u8) -> &mut Self {
		self.push(ty, vec![value])
	}

	/// Serialise, fragmenting any value longer than 255 bytes.
	///
	/// Per §16.1.1 a long value becomes several contiguous items of the same type, each
	/// full at 255 bytes except the last. The 384-byte SRP public keys hit this on every
	/// pair-setup, so this is not an edge case.
	pub fn encode(&self) -> Vec<u8> {
		let mut out = Vec::new();

		for (ty, value) in &self.items {
			if value.is_empty() {
				out.push(*ty);
				out.push(0);
				continue;
			}

			for chunk in value.chunks(MAX_FRAGMENT) {
				out.push(*ty);
				out.push(chunk.len() as u8);
				out.extend_from_slice(chunk);
			}
		}

		out
	}

	/// Parse, reassembling fragmented values.
	///
	/// Contiguous items sharing a type are concatenated; a type that reappears after an
	/// item of a different type starts a fresh entry (§16.1.1, last rule).
	pub fn parse(data: &[u8]) -> Result<Self> {
		let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
		let mut offset = 0;

		while offset < data.len() {
			if offset + 2 > data.len() {
				bail!("truncated TLV header at offset {offset}");
			}

			let ty = data[offset];
			let len = data[offset + 1] as usize;
			offset += 2;

			if offset + len > data.len() {
				bail!(
					"truncated TLV value at offset {offset}: want {len}, have {}",
					data.len() - offset
				);
			}

			let value = &data[offset..offset + len];
			offset += len;

			match items.last_mut() {
				Some((last, buf)) if *last == ty => buf.extend_from_slice(value),
				_ => items.push((ty, value.to_vec())),
			}
		}

		Ok(Self { items })
	}

	pub fn get(&self, ty: u8) -> Option<&[u8]> {
		self.items
			.iter()
			.find(|(t, _)| *t == ty)
			.map(|(_, v)| v.as_slice())
	}

	/// A single-byte integer item. HAP integers are little endian and minimally encoded,
	/// so a one-byte read covers every value we care about (state, method, error).
	pub fn get_u8(&self, ty: u8) -> Option<u8> {
		self.get(ty).and_then(|v| v.first().copied())
	}

	/// Fail if the accessory reported an error, naming it.
	pub fn check_error(&self, context: &str) -> Result<()> {
		if let Some(code) = self.get_u8(ty::ERROR) {
			bail!(
				"{context}: accessory returned {} ({code:#04x})",
				error_name(code)
			);
		}

		Ok(())
	}

	/// Fail unless the accessory is at the state we expect.
	pub fn expect_state(&self, want: u8, context: &str) -> Result<()> {
		self.check_error(context)?;

		match self.get_u8(ty::STATE) {
			Some(state) if state == want => Ok(()),
			Some(state) => bail!("{context}: expected state M{want}, got M{state}"),
			None => bail!("{context}: response has no state item"),
		}
	}

	/// A required item, erroring by name when absent.
	pub fn require(&self, ty: u8, name: &str) -> Result<&[u8]> {
		self.get(ty)
			.ok_or_else(|| anyhow::anyhow!("pairing response is missing {name}"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trips_small_items() {
		let mut tlv = Tlv8::new();
		tlv.push_u8(ty::STATE, 3)
			.push(ty::IDENTIFIER, b"hello".to_vec());

		// HAP R13 §16.1.2, TLV Example 1.
		assert_eq!(tlv.encode(), b"\x06\x01\x03\x01\x05hello");

		let parsed = Tlv8::parse(&tlv.encode()).unwrap();
		assert_eq!(parsed.get_u8(ty::STATE), Some(3));
		assert_eq!(parsed.get(ty::IDENTIFIER), Some(b"hello".as_slice()));
	}

	#[test]
	fn fragments_long_values() {
		let value = vec![0x61u8; 300];
		let mut tlv = Tlv8::new();
		tlv.push_u8(ty::STATE, 3)
			.push(ty::CERTIFICATE, value.clone())
			.push(ty::IDENTIFIER, b"hello".to_vec());

		let encoded = tlv.encode();

		// State(3) + cert fragments of 255 and 45 + identifier(5): §16.1.2 Example 2.
		assert_eq!(encoded.len(), 3 + (2 + 255) + (2 + 45) + (2 + 5));
		assert_eq!(&encoded[3..5], &[ty::CERTIFICATE, 255]);
		assert_eq!(&encoded[260..262], &[ty::CERTIFICATE, 45]);

		let parsed = Tlv8::parse(&encoded).unwrap();
		assert_eq!(parsed.get(ty::CERTIFICATE), Some(value.as_slice()));
		assert_eq!(parsed.get(ty::IDENTIFIER), Some(b"hello".as_slice()));
	}

	#[test]
	fn rejects_truncated_input() {
		assert!(Tlv8::parse(&[ty::STATE, 4, 1, 2]).is_err());
	}
}
