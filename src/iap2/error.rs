//! Errors surfaced by the iAP2 stack.

use std::io;

/// Anything that can go wrong between the wire and a decoded control-session message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// The underlying transport failed.
	#[error("iAP2 transport error")]
	Transport(#[from] io::Error),

	/// A link packet could not be parsed. The codec resynchronizes past these
	/// (§56.2.1.7), so they are informational unless they repeat.
	#[error("malformed link packet: {0}")]
	Packet(#[from] PacketError),

	/// A control-session message could not be decoded.
	#[error("malformed control-session message: {0}")]
	Message(#[from] MessageError),

	/// The accessory sent a Link Synchronization Payload we cannot work with.
	#[error("bad link synchronization payload: {0}")]
	LinkSync(&'static str),

	/// The peer reset the link, or we gave up on it.
	#[error("iAP2 link reset")]
	Reset,

	/// The handshake did not finish inside its budget.
	#[error("iAP2 {0} timed out")]
	Timeout(&'static str),

	/// The transport ended.
	#[error("iAP2 transport closed")]
	Closed,
}

/// Reason a byte slice could not be parsed as a link packet (§56.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
	/// Missing the `FF 5A` start-of-packet marker.
	#[error("missing FF 5A start-of-packet marker")]
	BadSop,
	/// Declared packet length is < 9 bytes, or 10 (a payload of just its checksum).
	#[error("invalid declared packet length {0}")]
	BadLength(u16),
	/// Declared length exceeds the negotiated `MaxReceivedPacketLength`.
	#[error("declared length {len} exceeds negotiated maximum {max}")]
	TooLong { len: u16, max: usize },
	/// Header checksum mismatch.
	#[error("header checksum mismatch")]
	BadHeaderChecksum,
	/// Payload checksum mismatch.
	#[error("payload checksum mismatch")]
	BadPayloadChecksum,
	/// SYN, EAK and RST are mutually exclusive (§56.2.1.3).
	#[error("mutually exclusive control bits set: {0:#04x}")]
	BadControl(u8),
}

/// Reason a control-session payload could not be decoded (§56.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
	/// Missing the `40 40` start-of-message marker.
	#[error("missing 40 40 start-of-message marker")]
	BadSom,
	/// Declared message length is < 6, or disagrees with the payload size.
	#[error("declared message length {declared} does not fit {actual} bytes")]
	BadLength { declared: u16, actual: usize },
	/// A parameter's declared length is < 4 or overruns the message.
	#[error("parameter length {0} overruns the message")]
	BadParamLength(u16),
	/// A parameter's data does not match the width its type requires.
	#[error("parameter {id:#06x} has {len} data bytes, expected {want}")]
	BadParamWidth { id: u16, len: usize, want: usize },
	/// A string parameter was not valid UTF-8.
	#[error("parameter {0:#06x} is not valid UTF-8")]
	BadUtf8(u16),
	/// A message or parameter grew past the 16-bit length field.
	#[error("message too long to encode")]
	TooLong,
}
