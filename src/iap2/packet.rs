//! iAP2 link packet wire codec (spec §56.2).

use std::{fmt, ops::BitOr};

/// The 6-byte iAP2 device-detection sequence (§56.2.7.2). The accessory sends it
/// at 1 Hz; a device that supports iAP2 echoes the identical bytes back.
pub const DETECTION: [u8; 6] = [0xFF, 0x55, 0x02, 0x00, 0xEE, 0x10];

/// iAP2 checksum (§56.2.1.10): the two's complement of the 8-bit sum of `bytes`, so
/// that `(sum(bytes) + checksum) & 0xFF == 0`.
pub fn checksum(bytes: &[u8]) -> u8 {
	bytes
		.iter()
		.fold(0u8, |acc, &b| acc.wrapping_add(b))
		.wrapping_neg()
}

/// Link packet control-byte bits (§56.2.1.3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Control(pub u8);

impl Control {
	/// Link Synchronization Payload is present.
	pub const SYN: Control = Control(0x80);
	/// Packet Acknowledgement Number is valid; a session payload may be present.
	pub const ACK: Control = Control(0x40);
	/// Extended Acknowledgement Payload is present.
	pub const EAK: Control = Control(0x20);
	/// Link reset (device → accessory only).
	pub const RST: Control = Control(0x10);
	/// Device sleep (device → accessory only).
	pub const SLP: Control = Control(0x08);

	pub const fn bits(self) -> u8 {
		self.0
	}

	pub const fn contains(self, other: Control) -> bool {
		self.0 & other.0 == other.0
	}
}

impl BitOr for Control {
	type Output = Control;
	fn bitor(self, rhs: Control) -> Control {
		Control(self.0 | rhs.0)
	}
}

impl fmt::Debug for Control {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		let mut first = true;
		for (bit, name) in [
			(Self::SYN, "SYN"),
			(Self::ACK, "ACK"),
			(Self::EAK, "EAK"),
			(Self::RST, "RST"),
			(Self::SLP, "SLP"),
		] {
			if self.contains(bit) {
				if !first {
					write!(f, "|")?;
				}
				write!(f, "{name}")?;
				first = false;
			}
		}
		if first {
			write!(f, "(none)")?;
		}
		Ok(())
	}
}

/// A parsed iAP2 link packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkPacket {
	pub control: Control,
	pub seq: u8,
	pub ack: u8,
	pub session: u8,
	/// Session / Link-Sync / EAK payload (empty for control-only packets).
	pub payload: Vec<u8>,
}

/// Reason a byte slice could not be parsed as a link packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
	/// Fewer bytes than the packet's declared length are available yet.
	Incomplete,
	/// Missing the `FF 5A` start-of-packet marker.
	BadSop,
	/// Declared packet length is < 9 bytes.
	BadLength,
	/// Header checksum mismatch.
	BadHeaderChecksum,
	/// Payload checksum mismatch.
	BadPayloadChecksum,
}

impl LinkPacket {
	pub fn new(control: Control, seq: u8, ack: u8, session: u8, payload: Vec<u8>) -> Self {
		Self { control, seq, ack, session, payload }
	}

	/// Whether the packet carries a session / Link-Sync / EAK payload.
	pub fn has_payload(&self) -> bool {
		!self.payload.is_empty()
	}

	/// Parse one packet from the front of `buf`. Returns the packet and the number
	/// of bytes it occupied, so callers can parse successive packets from a stream.
	pub fn parse(buf: &[u8]) -> Result<(Self, usize), ParseError> {
		if buf.len() < 9 {
			return Err(ParseError::Incomplete);
		}
		if buf[0] != 0xFF || buf[1] != 0x5A {
			return Err(ParseError::BadSop);
		}
		let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
		if len < 9 {
			return Err(ParseError::BadLength);
		}
		if buf.len() < len {
			return Err(ParseError::Incomplete);
		}
		if checksum(&buf[0..8]) != buf[8] {
			return Err(ParseError::BadHeaderChecksum);
		}

		let payload = if len > 9 {
			let body = &buf[9..len - 1];
			if checksum(body) != buf[len - 1] {
				return Err(ParseError::BadPayloadChecksum);
			}
			body.to_vec()
		} else {
			Vec::new()
		};

		Ok((
			Self {
				control: Control(buf[4]),
				seq: buf[5],
				ack: buf[6],
				session: buf[7],
				payload,
			},
			len,
		))
	}

	/// Serialize to wire bytes with computed length and checksums.
	pub fn to_bytes(&self) -> Vec<u8> {
		let total = if self.payload.is_empty() {
			9
		} else {
			9 + self.payload.len() + 1
		};
		let mut buf = Vec::with_capacity(total);
		buf.push(0xFF);
		buf.push(0x5A);
		buf.extend_from_slice(&(total as u16).to_be_bytes());
		buf.push(self.control.bits());
		buf.push(self.seq);
		buf.push(self.ack);
		buf.push(self.session);
		let hdr_cksum = checksum(&buf[0..8]);
		buf.push(hdr_cksum);
		if !self.payload.is_empty() {
			buf.extend_from_slice(&self.payload);
			buf.push(checksum(&self.payload));
		}
		buf
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn checksum_sums_to_zero() {
		for data in [&[][..], &[0x00], &[0xFF], &[0x01, 0x02, 0xFE], &DETECTION] {
			let c = checksum(data);
			let sum = data.iter().fold(0u8, |a, &b| a.wrapping_add(b));
			assert_eq!(sum.wrapping_add(c), 0);
		}
	}

	#[test]
	fn roundtrip_control_only() {
		let pkt = LinkPacket::new(Control::ACK, 7, 4, 0, Vec::new());
		let bytes = pkt.to_bytes();
		assert_eq!(bytes.len(), 9);
		let (back, used) = LinkPacket::parse(&bytes).unwrap();
		assert_eq!(used, 9);
		assert_eq!(back.control, pkt.control);
		assert_eq!(back.seq, 7);
		assert_eq!(back.ack, 4);
		assert!(!back.has_payload());
	}

	#[test]
	fn roundtrip_with_payload() {
		let pkt = LinkPacket::new(Control::ACK, 1, 2, 0x0B, vec![0xDE, 0xAD, 0xBE, 0xEF]);
		let bytes = pkt.to_bytes();
		assert_eq!(bytes.len(), 9 + 4 + 1);
		let (back, used) = LinkPacket::parse(&bytes).unwrap();
		assert_eq!(used, bytes.len());
		assert_eq!(back.session, 0x0B);
		assert_eq!(back.payload, vec![0xDE, 0xAD, 0xBE, 0xEF]);
	}

	#[test]
	fn detects_corruption() {
		let pkt = LinkPacket::new(Control::SYN | Control::ACK, 3, 0, 0, vec![1, 2, 3]);
		let mut bytes = pkt.to_bytes();
		bytes[5] ^= 0xFF; // corrupt the sequence number → header checksum fails
		assert_eq!(LinkPacket::parse(&bytes), Err(ParseError::BadHeaderChecksum));

		let mut bytes = pkt.to_bytes();
		let last = bytes.len() - 1;
		bytes[last] ^= 0xFF; // corrupt the payload checksum
		assert_eq!(LinkPacket::parse(&bytes), Err(ParseError::BadPayloadChecksum));
	}

	#[test]
	fn rejects_bad_sop_and_short() {
		assert_eq!(LinkPacket::parse(&[0; 5]), Err(ParseError::Incomplete));
		assert_eq!(
			LinkPacket::parse(&[0x00, 0x5A, 0, 9, 0, 0, 0, 0, 0]),
			Err(ParseError::BadSop)
		);
	}
}
