use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Iap2Error {
	BufferTooShort {
		expected_at_least: usize,
		actual: usize,
	},
	InvalidStartOfPacket {
		first: u8,
		second: u8,
	},
	InvalidPacketLength {
		advertised: u16,
		actual: usize,
	},
	PayloadTooLarge {
		size: usize,
		max: usize,
	},
	InvalidControlByte(u8),
	InvalidHeaderChecksum {
		expected: u8,
		actual: u8,
	},
	InvalidPayloadChecksum {
		expected: u8,
		actual: u8,
	},
	MissingPayloadData,
	MissingPayload,
	UnexpectedPayload,
	InvalidControlMessageStart {
		first: u8,
		second: u8,
	},
	InvalidControlMessageLength {
		advertised: u16,
		actual: usize,
	},
	InvalidControlParameterLength {
		advertised: u16,
		remaining: usize,
	},
	InvalidControlMessage(&'static str),
	MissingControlParameter(u16),
	InvalidControlParameterValue {
		parameter_id: u16,
		reason: &'static str,
	},
	UnsupportedControlMessage(u16),
	AckNumberMustBeZero(u8),
	SessionIdMustBeZero(u8),
	SessionIdRequired,
	SessionPayloadRequiresAck,
	InvalidSyncPayload(&'static str),
	InvalidSessionTableLength(usize),
	DuplicateSessionId(u8),
	SequenceOutsideWindow {
		sequence: u8,
		left_edge: u8,
		right_edge: u8,
	},
	FlowControlWindowFull,
	MaxRetransmissionsReached {
		sequence: u8,
		attempts: u8,
		max_attempts: u8,
	},
	MissingControlSessionV2,
	UnexpectedMachineState(&'static str),
	IdentificationRejected(&'static str),
	UnexpectedPacketKind(&'static str),
}

impl Display for Iap2Error {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::BufferTooShort {
				expected_at_least,
				actual,
			} => {
				write!(
					f,
					"buffer too short: expected at least {expected_at_least} bytes, got {actual}"
				)
			}
			Self::InvalidStartOfPacket { first, second } => {
				write!(
					f,
					"invalid start-of-packet bytes {first:#04x} {second:#04x}"
				)
			}
			Self::InvalidPacketLength { advertised, actual } => {
				write!(
					f,
					"invalid packet length: header advertises {advertised}, frame has {actual} bytes"
				)
			}
			Self::PayloadTooLarge { size, max } => {
				write!(f, "payload too large: {size} bytes (max {max})")
			}
			Self::InvalidControlByte(control) => write!(f, "invalid control byte {control:#04x}"),
			Self::InvalidHeaderChecksum { expected, actual } => {
				write!(
					f,
					"invalid header checksum: expected {expected:#04x}, got {actual:#04x}"
				)
			}
			Self::InvalidPayloadChecksum { expected, actual } => {
				write!(
					f,
					"invalid payload checksum: expected {expected:#04x}, got {actual:#04x}"
				)
			}
			Self::MissingPayloadData => {
				write!(f, "packet contains payload checksum but no payload data")
			}
			Self::MissingPayload => write!(f, "packet is missing required payload"),
			Self::UnexpectedPayload => {
				write!(f, "packet payload is not allowed for this control byte")
			}
			Self::InvalidControlMessageStart { first, second } => {
				write!(
					f,
					"invalid control message start bytes {first:#04x} {second:#04x}"
				)
			}
			Self::InvalidControlMessageLength { advertised, actual } => {
				write!(
					f,
					"invalid control message length: advertised {advertised}, actual {actual}"
				)
			}
			Self::InvalidControlParameterLength {
				advertised,
				remaining,
			} => {
				write!(
					f,
					"invalid control parameter length: advertised {advertised}, remaining {remaining}"
				)
			}
			Self::InvalidControlMessage(reason) => {
				write!(f, "invalid control message: {reason}")
			}
			Self::MissingControlParameter(parameter_id) => {
				write!(f, "missing control parameter {parameter_id:#06x}")
			}
			Self::InvalidControlParameterValue {
				parameter_id,
				reason,
			} => {
				write!(f, "invalid control parameter {parameter_id:#06x}: {reason}")
			}
			Self::UnsupportedControlMessage(message_id) => {
				write!(f, "unsupported control message {message_id:#06x}")
			}
			Self::AckNumberMustBeZero(ack) => {
				write!(
					f,
					"acknowledgement number must be zero when ACK is unset (got {ack})"
				)
			}
			Self::SessionIdMustBeZero(session_id) => {
				write!(
					f,
					"session id must be zero for this packet (got {session_id})"
				)
			}
			Self::SessionIdRequired => {
				write!(f, "session id must be non-zero for session payloads")
			}
			Self::SessionPayloadRequiresAck => {
				write!(f, "session payload requires ACK bit to be set")
			}
			Self::InvalidSyncPayload(reason) => {
				write!(f, "invalid synchronization payload: {reason}")
			}
			Self::InvalidSessionTableLength(length) => {
				write!(f, "invalid synchronization session table length: {length}")
			}
			Self::DuplicateSessionId(session_id) => {
				write!(f, "duplicate synchronization session id {session_id}")
			}
			Self::SequenceOutsideWindow {
				sequence,
				left_edge,
				right_edge,
			} => {
				write!(
					f,
					"sequence {sequence} outside receive window [{left_edge}..={right_edge}]"
				)
			}
			Self::FlowControlWindowFull => write!(f, "flow control window is full"),
			Self::MaxRetransmissionsReached {
				sequence,
				attempts,
				max_attempts,
			} => {
				write!(
					f,
					"packet {sequence} reached retransmission limit ({attempts}/{max_attempts})"
				)
			}
			Self::MissingControlSessionV2 => {
				write!(f, "control session version 2 is required for CarPlay")
			}
			Self::UnexpectedMachineState(reason) => {
				write!(f, "unexpected state-machine transition: {reason}")
			}
			Self::IdentificationRejected(reason) => {
				write!(f, "identification rejected: {reason}")
			}
			Self::UnexpectedPacketKind(expected) => {
				write!(f, "unexpected packet kind (expected {expected})")
			}
		}
	}
}

impl std::error::Error for Iap2Error {}
