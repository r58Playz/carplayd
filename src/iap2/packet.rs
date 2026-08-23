//! iAP2 link packet wire codec (spec §56.2).
//!
//! [`LinkCodec`] is a [`tokio_util::codec::Decoder`] / [`tokio_util::codec::Encoder`] pair
//! over [`Frame`], so any byte transport becomes a framed `Stream`/`Sink` of link packets:
//! the bulk pipes in USB Host Mode, or reassembled HID report sets in USB Device Mode.
//!
//! The decoder resynchronizes rather than failing. §56.2.1.7 requires a receiver with a
//! checksum mismatch to "restart packet parsing from the next detected Start of Packet
//! sequence", and that same behaviour is what lets one codec serve both transports — the
//! zero padding at the tail of every HID report is just bytes before the next marker.

use std::{fmt, ops::BitOr};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::{debug, warn};
use tokio_util::codec::{Decoder, Encoder};

use crate::iap2::error::{Error, PacketError};

/// The 6-byte iAP2 device-detection sequence (§56.2.7.2). The accessory sends it at 1 Hz;
/// a device that supports iAP2 echoes the identical bytes back.
pub const DETECTION: [u8; 6] = [0xFF, 0x55, 0x02, 0x00, 0xEE, 0x10];

/// Sent by the accessory to say it has decided we only speak iAP1 (§56.2.7.2). Receiving
/// this means the detection echo was missed or malformed.
pub const IAP1_INCOMPATIBLE: [u8; 18] = [
	0xFF, 0x55, 0x0E, 0x00, 0x13, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
	0xFF, 0xEB,
];

/// Start-of-packet marker for a link packet (§56.2.1.1).
const SOP: [u8; 2] = [0xFF, 0x5A];
/// Start of the detection / iAP1 sequences, which share the 0xFF first byte.
const DETECT_SOP: [u8; 2] = [0xFF, 0x55];

/// Bytes in a link packet header, including its checksum.
pub const HEADER_LEN: usize = 9;

/// The `MaximumReceivedPacketLength` the spec allows (§56.2.1.2).
pub const MAX_PACKET_LEN: usize = 65535;

/// iAP2 checksum (§56.2.1.10): the two's complement of the 8-bit sum of `bytes`, so that
/// `(sum(bytes) + checksum) & 0xFF == 0`.
pub fn checksum(bytes: &[u8]) -> u8 {
	bytes
		.iter()
		.fold(0u8, |acc, &b| acc.wrapping_add(b))
		.wrapping_neg()
}

/// Link packet control-byte bits (§56.2.1.3).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
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

	/// Bits the spec does not assign; they must be zero.
	const RESERVED: u8 = 0x07;

	pub const fn bits(self) -> u8 {
		self.0
	}

	pub const fn contains(self, other: Control) -> bool {
		self.0 & other.0 == other.0
	}

	/// Check the combination rules in §56.2.1.3.
	pub fn validate(self) -> Result<(), PacketError> {
		let exclusive = [Self::SYN, Self::EAK, Self::RST]
			.into_iter()
			.filter(|b| self.contains(*b))
			.count();
		let ok = exclusive <= 1
			&& self.0 & Self::RESERVED == 0
			&& (!self.contains(Self::EAK) || self.contains(Self::ACK))
			&& (!self.contains(Self::RST) || self.0 == Self::RST.0)
			&& (!self.contains(Self::SLP) || self.0 == Self::SLP.0);
		if ok {
			Ok(())
		} else {
			Err(PacketError::BadControl(self.0))
		}
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
#[derive(Clone, PartialEq, Eq)]
pub struct LinkPacket {
	pub control: Control,
	pub seq: u8,
	pub ack: u8,
	pub session: u8,
	/// Session / Link-Sync / EAK payload (empty for control-only packets).
	pub payload: Bytes,
}

impl LinkPacket {
	pub fn new(control: Control, seq: u8, ack: u8, session: u8, payload: Bytes) -> Self {
		Self {
			control,
			seq,
			ack,
			session,
			payload,
		}
	}

	/// A packet with no payload: an ACK, RST or SLP.
	pub fn control_only(control: Control, seq: u8, ack: u8) -> Self {
		Self::new(control, seq, ack, 0, Bytes::new())
	}

	/// Whether the packet carries a session / Link-Sync / EAK payload.
	pub fn has_payload(&self) -> bool {
		!self.payload.is_empty()
	}

	/// Total wire size.
	pub fn wire_len(&self) -> usize {
		if self.payload.is_empty() {
			HEADER_LEN
		} else {
			HEADER_LEN + self.payload.len() + 1
		}
	}

	/// Parse one packet from the front of `buf`.
	///
	/// `Ok(None)` means the declared packet is not fully buffered yet; the caller should
	/// read more and retry with the same bytes. `max_len` is the negotiated
	/// `MaximumReceivedPacketLength`.
	pub fn parse(buf: &[u8], max_len: usize) -> Result<Option<(Self, usize)>, PacketError> {
		if buf.len() < HEADER_LEN {
			return Ok(None);
		}
		if buf[..2] != SOP {
			return Err(PacketError::BadSop);
		}
		let declared = u16::from_be_bytes([buf[2], buf[3]]);
		let len = declared as usize;
		// A declared length of 10 would mean a payload consisting only of its own
		// checksum, which no valid packet has.
		if len < HEADER_LEN || len == HEADER_LEN + 1 {
			return Err(PacketError::BadLength(declared));
		}
		if len > max_len {
			return Err(PacketError::TooLong {
				len: declared,
				max: max_len,
			});
		}
		if checksum(&buf[..HEADER_LEN - 1]) != buf[HEADER_LEN - 1] {
			return Err(PacketError::BadHeaderChecksum);
		}
		if buf.len() < len {
			return Ok(None);
		}

		let control = Control(buf[4]);
		control.validate()?;

		let payload = if len > HEADER_LEN {
			let body = &buf[HEADER_LEN..len - 1];
			if checksum(body) != buf[len - 1] {
				return Err(PacketError::BadPayloadChecksum);
			}
			Bytes::copy_from_slice(body)
		} else {
			Bytes::new()
		};

		Ok(Some((
			Self {
				control,
				seq: buf[5],
				ack: buf[6],
				session: buf[7],
				payload,
			},
			len,
		)))
	}

	/// Serialize to wire bytes with computed length and checksums.
	pub fn encode(&self, dst: &mut BytesMut) {
		let total = self.wire_len();
		dst.reserve(total);
		let start = dst.len();
		dst.put_slice(&SOP);
		dst.put_u16(total as u16);
		dst.put_u8(self.control.bits());
		dst.put_u8(self.seq);
		dst.put_u8(self.ack);
		dst.put_u8(self.session);
		dst.put_u8(checksum(&dst[start..start + HEADER_LEN - 1]));
		if !self.payload.is_empty() {
			dst.put_slice(&self.payload);
			dst.put_u8(checksum(&self.payload));
		}
	}

	/// Serialize to a standalone buffer.
	pub fn to_bytes(&self) -> Bytes {
		let mut buf = BytesMut::with_capacity(self.wire_len());
		self.encode(&mut buf);
		buf.freeze()
	}
}

impl fmt::Debug for LinkPacket {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(
			f,
			"[{:?} seq={} ack={} session={}",
			self.control, self.seq, self.ack, self.session
		)?;
		if self.has_payload() {
			write!(f, " payload={}B", self.payload.len())?;
		}
		write!(f, "]")
	}
}

/// A frame recovered from the transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
	/// The iAP2 detection sequence (`FF 55 02 00 EE 10`).
	Detection,
	/// The accessory's "you only speak iAP1" notice — a sign our echo went astray.
	Iap1Incompatible,
	/// A link packet.
	Packet(LinkPacket),
}

/// How much the decoder has had to throw away. Non-zero counts mean the transport is
/// noisier than "reliable USB" implies, or that we are framing it wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodecStats {
	/// Bytes skipped while hunting for a start-of-packet marker.
	pub resync_bytes: u64,
	/// Packets dropped because a checksum, length or control byte was invalid.
	pub bad_packets: u64,
}

/// Framing codec for the iAP2 link layer.
pub struct LinkCodec {
	max_len: usize,
	stats: CodecStats,
}

impl Default for LinkCodec {
	fn default() -> Self {
		Self::new(MAX_PACKET_LEN)
	}
}

impl LinkCodec {
	/// Build a codec bounded by the negotiated `MaximumReceivedPacketLength`.
	pub fn new(max_len: usize) -> Self {
		Self {
			max_len: max_len.clamp(HEADER_LEN, MAX_PACKET_LEN),
			stats: CodecStats::default(),
		}
	}

	/// Raise the bound once the accessory's Link Synchronization Payload is known.
	pub fn set_max_len(&mut self, max_len: usize) {
		self.max_len = max_len.clamp(HEADER_LEN, MAX_PACKET_LEN);
	}

	pub fn stats(&self) -> CodecStats {
		self.stats
	}

	/// Offset of the next plausible marker, or `None` if there is none in `buf`.
	fn find_marker(buf: &[u8]) -> Option<usize> {
		buf.windows(2).position(|w| w == SOP || w == DETECT_SOP)
	}
}

impl Decoder for LinkCodec {
	type Item = Frame;
	type Error = Error;

	fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, Error> {
		loop {
			let Some(offset) = Self::find_marker(src) else {
				// Keep a trailing 0xFF: it may be the first half of a marker that
				// straddles this read and the next.
				let keep = usize::from(src.last() == Some(&0xFF));
				let skip = src.len() - keep;
				if skip > 0 {
					self.stats.resync_bytes += skip as u64;
					src.advance(skip);
				}
				return Ok(None);
			};
			if offset > 0 {
				self.stats.resync_bytes += offset as u64;
				src.advance(offset);
			}

			if src[1] == DETECT_SOP[1] {
				if src.len() < DETECTION.len() {
					return Ok(None);
				}
				if src[..DETECTION.len()] == DETECTION {
					src.advance(DETECTION.len());
					return Ok(Some(Frame::Detection));
				}
				if src.len() < IAP1_INCOMPATIBLE.len() {
					return Ok(None);
				}
				if src[..IAP1_INCOMPATIBLE.len()] == IAP1_INCOMPATIBLE {
					src.advance(IAP1_INCOMPATIBLE.len());
					return Ok(Some(Frame::Iap1Incompatible));
				}
				// Not a sequence we know; step past this marker and keep hunting.
				self.stats.resync_bytes += 1;
				src.advance(1);
				continue;
			}

			match LinkPacket::parse(src, self.max_len) {
				Ok(Some((pkt, used))) => {
					src.advance(used);
					return Ok(Some(Frame::Packet(pkt)));
				}
				Ok(None) => return Ok(None),
				Err(err) => {
					// §56.2.1.7: resume from the next start-of-packet marker. Never
					// surface this as a stream error — that would end the connection
					// over what the spec treats as routine.
					self.stats.bad_packets += 1;
					warn!("discarding malformed link packet: {err}");
					self.stats.resync_bytes += 1;
					src.advance(1);
					continue;
				}
			}
		}
	}

	fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, Error> {
		match self.decode(src)? {
			Some(frame) => Ok(Some(frame)),
			None => {
				if !src.is_empty() {
					debug!("transport ended with {} undecodable bytes", src.len());
					src.clear();
				}
				Ok(None)
			}
		}
	}
}

impl Encoder<Frame> for LinkCodec {
	type Error = Error;

	fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), Error> {
		match frame {
			Frame::Detection => dst.put_slice(&DETECTION),
			Frame::Iap1Incompatible => dst.put_slice(&IAP1_INCOMPATIBLE),
			Frame::Packet(pkt) => pkt.encode(dst),
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pkt(control: Control, seq: u8, ack: u8, session: u8, payload: &[u8]) -> LinkPacket {
		LinkPacket::new(control, seq, ack, session, Bytes::copy_from_slice(payload))
	}

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
		let p = LinkPacket::control_only(Control::ACK, 7, 4);
		let bytes = p.to_bytes();
		assert_eq!(bytes.len(), HEADER_LEN);
		let (back, used) = LinkPacket::parse(&bytes, MAX_PACKET_LEN).unwrap().unwrap();
		assert_eq!(used, HEADER_LEN);
		assert_eq!(back, p);
		assert!(!back.has_payload());
	}

	#[test]
	fn roundtrip_with_payload() {
		let p = pkt(Control::ACK, 1, 2, 0x0B, &[0xDE, 0xAD, 0xBE, 0xEF]);
		let bytes = p.to_bytes();
		assert_eq!(bytes.len(), HEADER_LEN + 5);
		let (back, used) = LinkPacket::parse(&bytes, MAX_PACKET_LEN).unwrap().unwrap();
		assert_eq!(used, bytes.len());
		assert_eq!(back, p);
	}

	#[test]
	fn incomplete_is_not_an_error() {
		let bytes = pkt(Control::ACK, 1, 2, 3, &[1, 2, 3, 4]).to_bytes();
		for n in 0..bytes.len() {
			assert_eq!(
				LinkPacket::parse(&bytes[..n], MAX_PACKET_LEN),
				Ok(None),
				"prefix of {n} bytes"
			);
		}
		assert!(LinkPacket::parse(&bytes, MAX_PACKET_LEN).unwrap().is_some());
	}

	#[test]
	fn rejects_corruption_and_bad_lengths() {
		let p = pkt(Control::SYN, 3, 0, 0, &[1, 2, 3]);

		let mut b = p.to_bytes().to_vec();
		b[5] ^= 0xFF; // sequence number -> header checksum fails
		assert_eq!(
			LinkPacket::parse(&b, MAX_PACKET_LEN),
			Err(PacketError::BadHeaderChecksum)
		);

		let mut b = p.to_bytes().to_vec();
		let last = b.len() - 1;
		b[last] ^= 0xFF;
		assert_eq!(
			LinkPacket::parse(&b, MAX_PACKET_LEN),
			Err(PacketError::BadPayloadChecksum)
		);

		// A declared length of 10 has a payload of only its own checksum.
		let mut b = vec![0xFF, 0x5A, 0x00, 0x0A, 0x40, 0, 0, 0, 0];
		b[8] = checksum(&b[..8]);
		assert_eq!(
			LinkPacket::parse(&b, MAX_PACKET_LEN),
			Err(PacketError::BadLength(10))
		);

		let big = pkt(Control::ACK, 0, 0, 1, &[0u8; 64]).to_bytes();
		assert_eq!(
			LinkPacket::parse(&big, 32),
			Err(PacketError::TooLong {
				len: big.len() as u16,
				max: 32
			})
		);
	}

	#[test]
	fn control_byte_rules() {
		assert!(Control::ACK.validate().is_ok());
		assert!((Control::SYN | Control::ACK).validate().is_ok());
		assert!((Control::ACK | Control::EAK).validate().is_ok());
		assert!(Control::RST.validate().is_ok());
		assert!(Control::SLP.validate().is_ok());
		// SYN, EAK and RST are mutually exclusive.
		assert!((Control::SYN | Control::EAK).validate().is_err());
		assert!((Control::SYN | Control::RST).validate().is_err());
		// EAK is always accompanied by ACK.
		assert!(Control::EAK.validate().is_err());
		// RST and SLP stand alone.
		assert!((Control::RST | Control::ACK).validate().is_err());
		assert!((Control::SLP | Control::ACK).validate().is_err());
		// Reserved bits must be zero.
		assert!(Control(0x41).validate().is_err());
	}

	#[test]
	fn codec_roundtrips_frames() {
		let mut codec = LinkCodec::default();
		let mut buf = BytesMut::new();
		let p = pkt(
			Control::ACK,
			9,
			8,
			0x0A,
			&[0x40, 0x40, 0x00, 0x06, 0xAA, 0x00],
		);
		codec.encode(Frame::Detection, &mut buf).unwrap();
		codec.encode(Frame::Packet(p.clone()), &mut buf).unwrap();
		assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Detection));
		assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Packet(p)));
		assert_eq!(codec.decode(&mut buf).unwrap(), None);
		assert!(buf.is_empty());
	}

	#[test]
	fn codec_skips_hid_zero_padding_between_packets() {
		// What a reassembled HID report set looks like: packets, then padding.
		let a = pkt(Control::ACK, 1, 0, 0x0A, &[1, 2, 3]);
		let b = LinkPacket::control_only(Control::ACK, 2, 1);
		let mut buf = BytesMut::new();
		buf.put_slice(&a.to_bytes());
		buf.put_slice(&b.to_bytes());
		buf.put_slice(&[0u8; 40]);

		let mut codec = LinkCodec::default();
		assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Packet(a)));
		assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Packet(b)));
		assert_eq!(codec.decode(&mut buf).unwrap(), None);
		assert_eq!(codec.stats().resync_bytes, 40);
		assert_eq!(codec.stats().bad_packets, 0);
	}

	#[test]
	fn codec_resyncs_past_a_corrupt_packet() {
		let good = pkt(Control::ACK, 5, 4, 0x0A, &[0xAA, 0xBB]);
		let mut bad = pkt(Control::ACK, 4, 3, 0x0A, &[1, 2, 3, 4])
			.to_bytes()
			.to_vec();
		let last = bad.len() - 1;
		bad[last] ^= 0xFF;

		let mut buf = BytesMut::new();
		buf.put_slice(b"junk before the stream");
		buf.put_slice(&bad);
		buf.put_slice(&good.to_bytes());

		let mut codec = LinkCodec::default();
		// The corrupt packet is dropped silently and the good one still arrives.
		assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Packet(good)));
		assert_eq!(codec.stats().bad_packets, 1);
		assert!(codec.stats().resync_bytes > 0);
	}

	#[test]
	fn codec_waits_for_a_marker_split_across_reads() {
		let p = LinkPacket::control_only(Control::ACK, 1, 0);
		let bytes = p.to_bytes();
		let mut codec = LinkCodec::default();
		let mut buf = BytesMut::new();

		// A lone trailing 0xFF must be retained, not discarded as garbage.
		buf.put_slice(&[0x00, 0x00, 0xFF]);
		assert_eq!(codec.decode(&mut buf).unwrap(), None);
		assert_eq!(buf.len(), 1);

		buf.put_slice(&bytes[1..]);
		assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Packet(p)));
	}

	#[test]
	fn codec_recognizes_the_iap1_notice() {
		let mut codec = LinkCodec::default();
		let mut buf = BytesMut::new();
		buf.put_slice(&IAP1_INCOMPATIBLE);
		assert_eq!(
			codec.decode(&mut buf).unwrap(),
			Some(Frame::Iap1Incompatible)
		);
	}

	#[test]
	fn codec_rejects_oversized_declared_length_and_moves_on() {
		let mut codec = LinkCodec::new(64);
		let mut buf = BytesMut::new();
		buf.put_slice(&pkt(Control::ACK, 0, 0, 1, &[0u8; 200]).to_bytes());
		let good = LinkPacket::control_only(Control::ACK, 1, 0);
		buf.put_slice(&good.to_bytes());
		assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::Packet(good)));
		assert_eq!(codec.stats().bad_packets, 1);
	}
}
