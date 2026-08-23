//! Link Synchronization Payload and the parameters it negotiates (spec §56.2.2).
//!
//! One clarification worth recording, because it is easy to get backwards: CarPlay
//! requires **control session version 2** (§30.2.4.2), which is the third byte of a
//! session table entry — *not* the Link Version byte, which stays `1` (§56.2.2.1). So the
//! version-1 field layout in Table 56-3 is the right one to parse, and a CarPlay
//! accessory advertises `(id, type = 0, version = 2)` for its control session.

use std::{fmt, time::Duration};

use bytes::{BufMut, Bytes, BytesMut};

use crate::iap2::error::Error;

/// Fixed part of the payload, before the session table.
const FIXED_LEN: usize = 10;
/// Bytes per session table entry.
const SESSION_LEN: usize = 3;

/// Session types (§56.3.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionType {
	Control,
	FileTransfer,
	ExternalAccessory,
	Unknown(u8),
}

impl From<u8> for SessionType {
	fn from(v: u8) -> Self {
		match v {
			0 => Self::Control,
			1 => Self::FileTransfer,
			2 => Self::ExternalAccessory,
			other => Self::Unknown(other),
		}
	}
}

impl SessionType {
	pub fn as_u8(self) -> u8 {
		match self {
			Self::Control => 0,
			Self::FileTransfer => 1,
			Self::ExternalAccessory => 2,
			Self::Unknown(v) => v,
		}
	}
}

/// One session advertised in a Link Synchronization Payload.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionInfo {
	/// Nonzero and unique within the connection; 0 is reserved for the link (§56.3).
	pub id: u8,
	pub typ: SessionType,
	pub version: u8,
}

impl fmt::Debug for SessionInfo {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{:?} session {} v{}", self.typ, self.id, self.version)
	}
}

/// The negotiable and non-negotiable link parameters (§56.2.2).
#[derive(Clone, PartialEq, Eq)]
pub struct LinkParams {
	pub link_version: u8,
	/// Packets that may be in flight unacknowledged; 1..=127 (§56.2.2.2).
	pub max_outstanding: u8,
	/// Largest `Packet Length` we will receive; 24..=65535 (§56.2.2.3).
	pub max_recv_len: u16,
	/// Retransmission timeout; 20..=65535 ms, or zero for ZeroRetransmit.
	pub retransmit_timeout: Duration,
	/// Cumulative acknowledgement timeout; 10 ms..=half the retransmit timeout, or zero.
	pub cumulative_ack_timeout: Duration,
	/// Retransmissions before the link is considered broken; 1..=30, or zero.
	pub max_retransmit: u8,
	/// Acknowledgements to accumulate before one must be sent; 0..=max_outstanding.
	pub max_cumulative_ack: u8,
	pub sessions: Vec<SessionInfo>,
}

impl Default for LinkParams {
	fn default() -> Self {
		Self::usb_recommended()
	}
}

impl LinkParams {
	/// The values assumed until synchronization completes (Table 56-6).
	pub fn synchronizing_defaults() -> Self {
		Self {
			link_version: 1,
			max_outstanding: 1,
			max_recv_len: 128,
			retransmit_timeout: Duration::from_millis(1000),
			cumulative_ack_timeout: Duration::from_millis(10),
			max_retransmit: 30,
			max_cumulative_ack: 0,
			sessions: Vec::new(),
		}
	}

	/// Apple's recommended starting point for the USB transports (Tables 56-7 and 56-8),
	/// with a CarPlay control session (version 2, per §30.2.4.2).
	pub fn usb_recommended() -> Self {
		Self {
			link_version: 1,
			max_outstanding: 5,
			max_recv_len: 4096,
			retransmit_timeout: Duration::from_millis(2000),
			cumulative_ack_timeout: Duration::from_millis(22),
			max_retransmit: 30,
			max_cumulative_ack: 3,
			sessions: vec![SessionInfo {
				id: 0x0A,
				typ: SessionType::Control,
				version: 2,
			}],
		}
	}

	/// Whether this is a ZeroACK/ZeroRetransmit configuration (§56.2.2.8): no
	/// acknowledgements, no retransmissions, and a `SYN` on any detected sequence gap.
	pub fn zero_ack(&self) -> bool {
		self.retransmit_timeout.is_zero()
			&& self.cumulative_ack_timeout.is_zero()
			&& self.max_retransmit == 0
			&& self.max_cumulative_ack == 0
	}

	/// The identifier of the session with this type, if advertised.
	pub fn session_id(&self, typ: SessionType) -> Option<u8> {
		self.sessions.iter().find(|s| s.typ == typ).map(|s| s.id)
	}

	/// The control session's identifier (§56.3: every link has exactly one).
	pub fn control_session(&self) -> Option<u8> {
		self.session_id(SessionType::Control)
	}

	/// The control session's version, which CarPlay requires to be 2 (§30.2.4.2).
	pub fn control_session_version(&self) -> Option<u8> {
		self.sessions
			.iter()
			.find(|s| s.typ == SessionType::Control)
			.map(|s| s.version)
	}

	/// Check the ranges in §56.2.2. A ZeroACK configuration is accepted as-is.
	pub fn validate(&self) -> Result<(), Error> {
		if !(1..=127).contains(&self.max_outstanding) {
			return Err(Error::LinkSync(
				"MaximumNumberOfOutstandingPackets out of range",
			));
		}
		if self.max_recv_len < 24 {
			return Err(Error::LinkSync("MaximumReceivedPacketLength below 24"));
		}
		if self.control_session().is_none() {
			return Err(Error::LinkSync("no control session advertised"));
		}
		if self.sessions.iter().any(|s| s.id == 0) {
			return Err(Error::LinkSync(
				"session identifier 0 is reserved for the link",
			));
		}
		if self.max_cumulative_ack > self.max_outstanding {
			return Err(Error::LinkSync(
				"MaximumCumulativeAcknowledgements exceeds MaximumNumberOfOutstandingPackets",
			));
		}
		if self.zero_ack() {
			return Ok(());
		}
		let rto = self.retransmit_timeout.as_millis();
		let cat = self.cumulative_ack_timeout.as_millis();
		if !(20..=65535).contains(&rto) {
			return Err(Error::LinkSync("RetransmissionTimeout out of range"));
		}
		if cat != 0 && (cat < 10 || cat * 2 > rto) {
			return Err(Error::LinkSync(
				"CumulativeAcknowledgementTimeout out of range",
			));
		}
		if self.max_retransmit == 0 || self.max_retransmit > 30 {
			return Err(Error::LinkSync(
				"MaximumNumberOfRetransmissions out of range",
			));
		}
		Ok(())
	}

	/// Parse a Link Synchronization Payload from a `SYN` packet.
	pub fn decode(buf: &[u8]) -> Result<Self, Error> {
		if buf.len() < FIXED_LEN {
			return Err(Error::LinkSync("payload shorter than the fixed header"));
		}
		let table = &buf[FIXED_LEN..];
		if !table.len().is_multiple_of(SESSION_LEN) {
			return Err(Error::LinkSync(
				"session table is not a whole number of entries",
			));
		}
		Ok(Self {
			link_version: buf[0],
			max_outstanding: buf[1],
			max_recv_len: u16::from_be_bytes([buf[2], buf[3]]),
			retransmit_timeout: Duration::from_millis(u16::from_be_bytes([buf[4], buf[5]]).into()),
			cumulative_ack_timeout: Duration::from_millis(
				u16::from_be_bytes([buf[6], buf[7]]).into(),
			),
			max_retransmit: buf[8],
			max_cumulative_ack: buf[9],
			sessions: table
				.as_chunks::<SESSION_LEN>()
				.0
				.iter()
				.map(|c| SessionInfo {
					id: c[0],
					typ: c[1].into(),
					version: c[2],
				})
				.collect(),
		})
	}

	/// Serialize a Link Synchronization Payload.
	pub fn encode(&self) -> Bytes {
		let mut buf = BytesMut::with_capacity(FIXED_LEN + self.sessions.len() * SESSION_LEN);
		buf.put_u8(self.link_version);
		buf.put_u8(self.max_outstanding);
		buf.put_u16(self.max_recv_len);
		buf.put_u16(clamp_ms(self.retransmit_timeout));
		buf.put_u16(clamp_ms(self.cumulative_ack_timeout));
		buf.put_u8(self.max_retransmit);
		buf.put_u8(self.max_cumulative_ack);
		for s in &self.sessions {
			buf.put_u8(s.id);
			buf.put_u8(s.typ.as_u8());
			buf.put_u8(s.version);
		}
		buf.freeze()
	}
}

fn clamp_ms(d: Duration) -> u16 {
	u16::try_from(d.as_millis()).unwrap_or(u16::MAX)
}

impl fmt::Debug for LinkParams {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(
			f,
			"link v{} outstanding={} max_recv={} rto={:?} cat={:?} retransmit={} cum_ack={}{} {:?}",
			self.link_version,
			self.max_outstanding,
			self.max_recv_len,
			self.retransmit_timeout,
			self.cumulative_ack_timeout,
			self.max_retransmit,
			self.max_cumulative_ack,
			if self.zero_ack() { " ZeroACK" } else { "" },
			self.sessions,
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Table 56-12, the spec's own worked example.
	const EXAMPLE: [u8; 16] = [
		0x01, 0x05, 0x10, 0x00, 0x04, 0x0B, 0x00, 0x17, 0x03, 0x03, 0x0A, 0x00, 0x01, 0x0B, 0x02,
		0x01,
	];

	#[test]
	fn decodes_the_spec_example() {
		let p = LinkParams::decode(&EXAMPLE).unwrap();
		assert_eq!(p.link_version, 1);
		assert_eq!(p.max_outstanding, 5);
		assert_eq!(p.max_recv_len, 4096);
		assert_eq!(p.retransmit_timeout, Duration::from_millis(1035));
		assert_eq!(p.cumulative_ack_timeout, Duration::from_millis(23));
		assert_eq!(p.max_retransmit, 3);
		assert_eq!(p.max_cumulative_ack, 3);
		assert_eq!(p.sessions.len(), 2);
		assert_eq!(p.control_session(), Some(0x0A));
		assert_eq!(p.control_session_version(), Some(1));
		assert_eq!(p.session_id(SessionType::ExternalAccessory), Some(0x0B));
		assert!(!p.zero_ack());
		p.validate().unwrap();
	}

	#[test]
	fn round_trips_the_spec_example() {
		assert_eq!(
			&LinkParams::decode(&EXAMPLE).unwrap().encode()[..],
			&EXAMPLE[..]
		);
	}

	#[test]
	fn recommended_usb_params_are_valid() {
		let p = LinkParams::usb_recommended();
		p.validate().unwrap();
		assert_eq!(p.control_session_version(), Some(2));
		assert_eq!(&LinkParams::decode(&p.encode()).unwrap(), &p);
	}

	#[test]
	fn recognizes_zero_ack() {
		let mut p = LinkParams::usb_recommended();
		p.retransmit_timeout = Duration::ZERO;
		p.cumulative_ack_timeout = Duration::ZERO;
		p.max_retransmit = 0;
		p.max_cumulative_ack = 0;
		assert!(p.zero_ack());
		p.validate().unwrap();
	}

	#[test]
	fn rejects_nonsense() {
		assert!(LinkParams::decode(&EXAMPLE[..9]).is_err());
		// Session table not a multiple of 3.
		assert!(LinkParams::decode(&EXAMPLE[..14]).is_err());

		let mut p = LinkParams::usb_recommended();
		p.max_outstanding = 0;
		assert!(p.validate().is_err());

		let mut p = LinkParams::usb_recommended();
		p.sessions.clear();
		assert!(p.validate().is_err());

		let mut p = LinkParams::usb_recommended();
		p.sessions[0].id = 0;
		assert!(p.validate().is_err());

		let mut p = LinkParams::usb_recommended();
		p.cumulative_ack_timeout = p.retransmit_timeout; // must be <= half
		assert!(p.validate().is_err());

		let mut p = LinkParams::usb_recommended();
		p.max_cumulative_ack = 100; // > max_outstanding
		assert!(p.validate().is_err());
	}
}
