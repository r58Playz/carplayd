use super::{checksum, control::Control, error::Iap2Error, sync::LinkSynchronizationPayload};

pub const START_OF_PACKET: [u8; 2] = [0xff, 0x5a];
pub const HEADER_LEN: usize = 9;
pub const MAX_PACKET_LENGTH: usize = u16::MAX as usize;
pub const MAX_PAYLOAD_LENGTH: usize = MAX_PACKET_LENGTH - HEADER_LEN - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
	Syn,
	SynAck,
	Ack,
	ExtendedAck,
	Session,
	Reset,
	Sleep,
	Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPacket {
	pub control: Control,
	pub sequence_number: u8,
	pub acknowledgement_number: u8,
	pub session_id: u8,
	pub payload: Vec<u8>,
}

impl LinkPacket {
	pub fn new(
		control: Control,
		sequence_number: u8,
		acknowledgement_number: u8,
		session_id: u8,
		payload: Vec<u8>,
	) -> Result<Self, Iap2Error> {
		if payload.len() > MAX_PAYLOAD_LENGTH {
			return Err(Iap2Error::PayloadTooLarge {
				size: payload.len(),
				max: MAX_PAYLOAD_LENGTH,
			});
		}

		control.validate_packet_fields(!payload.is_empty(), acknowledgement_number, session_id)?;

		Ok(Self {
			control,
			sequence_number,
			acknowledgement_number,
			session_id,
			payload,
		})
	}

	pub fn syn(
		sequence_number: u8,
		payload: LinkSynchronizationPayload,
	) -> Result<Self, Iap2Error> {
		let payload_bytes = payload.encode()?;
		Self::new(Control::syn_only(), sequence_number, 0, 0, payload_bytes)
	}

	pub fn syn_ack(
		sequence_number: u8,
		acknowledgement_number: u8,
		payload: LinkSynchronizationPayload,
	) -> Result<Self, Iap2Error> {
		let payload_bytes = payload.encode()?;
		Self::new(
			Control::syn_ack(),
			sequence_number,
			acknowledgement_number,
			0,
			payload_bytes,
		)
	}

	pub fn ack(sequence_number: u8, acknowledgement_number: u8) -> Result<Self, Iap2Error> {
		Self::new(
			Control::ack_only(),
			sequence_number,
			acknowledgement_number,
			0,
			Vec::new(),
		)
	}

	pub fn eak(
		sequence_number: u8,
		acknowledgement_number: u8,
		out_of_sequence: &[u8],
	) -> Result<Self, Iap2Error> {
		if out_of_sequence.is_empty() {
			return Err(Iap2Error::MissingPayload);
		}

		Self::new(
			Control::eak(),
			sequence_number,
			acknowledgement_number,
			0,
			out_of_sequence.to_vec(),
		)
	}

	pub fn session(
		sequence_number: u8,
		acknowledgement_number: u8,
		session_id: u8,
		payload: Vec<u8>,
	) -> Result<Self, Iap2Error> {
		Self::new(
			Control::ack_only(),
			sequence_number,
			acknowledgement_number,
			session_id,
			payload,
		)
	}

	pub fn reset(sequence_number: u8) -> Result<Self, Iap2Error> {
		Self::new(Control::rst(), sequence_number, 0, 0, Vec::new())
	}

	pub fn sleep(sequence_number: u8) -> Result<Self, Iap2Error> {
		Self::new(Control::sleep(), sequence_number, 0, 0, Vec::new())
	}

	pub fn packet_length(&self) -> usize {
		if self.payload.is_empty() {
			HEADER_LEN
		} else {
			HEADER_LEN + self.payload.len() + 1
		}
	}

	pub fn packet_kind(&self) -> PacketKind {
		if self.control.is_rst() {
			PacketKind::Reset
		} else if self.control.is_sleep() {
			PacketKind::Sleep
		} else if self.control.is_eak() {
			PacketKind::ExtendedAck
		} else if self.control.is_syn() && self.control.is_ack() {
			PacketKind::SynAck
		} else if self.control.is_syn() {
			PacketKind::Syn
		} else if !self.payload.is_empty() {
			PacketKind::Session
		} else if self.control.is_ack() {
			PacketKind::Ack
		} else {
			PacketKind::Other
		}
	}

	pub fn encode(&self) -> Result<Vec<u8>, Iap2Error> {
		self.control.validate_packet_fields(
			!self.payload.is_empty(),
			self.acknowledgement_number,
			self.session_id,
		)?;

		let packet_length = self.packet_length();
		if packet_length > MAX_PACKET_LENGTH {
			return Err(Iap2Error::InvalidPacketLength {
				advertised: u16::MAX,
				actual: packet_length,
			});
		}

		let packet_length_u16 = packet_length as u16;

		let mut out = Vec::with_capacity(packet_length);
		out.extend_from_slice(&START_OF_PACKET);
		out.extend_from_slice(&packet_length_u16.to_be_bytes());
		out.push(self.control.raw());
		out.push(self.sequence_number);
		out.push(self.acknowledgement_number);
		out.push(self.session_id);

		let header_checksum = checksum::calculate(&out);
		out.push(header_checksum);

		if !self.payload.is_empty() {
			out.extend_from_slice(&self.payload);
			out.push(checksum::calculate(&self.payload));
		}

		Ok(out)
	}

	pub fn decode(frame: &[u8]) -> Result<Self, Iap2Error> {
		if frame.len() < HEADER_LEN {
			return Err(Iap2Error::BufferTooShort {
				expected_at_least: HEADER_LEN,
				actual: frame.len(),
			});
		}

		if frame[0] != START_OF_PACKET[0] || frame[1] != START_OF_PACKET[1] {
			return Err(Iap2Error::InvalidStartOfPacket {
				first: frame[0],
				second: frame[1],
			});
		}

		let advertised = u16::from_be_bytes([frame[2], frame[3]]);
		let advertised_len = usize::from(advertised);
		if advertised_len != frame.len() || advertised_len < HEADER_LEN {
			return Err(Iap2Error::InvalidPacketLength {
				advertised,
				actual: frame.len(),
			});
		}

		let control = Control::new(frame[4])?;
		let sequence_number = frame[5];
		let acknowledgement_number = frame[6];
		let session_id = frame[7];

		let expected_header_checksum = checksum::calculate(&frame[..8]);
		let header_checksum = frame[8];
		if expected_header_checksum != header_checksum {
			return Err(Iap2Error::InvalidHeaderChecksum {
				expected: expected_header_checksum,
				actual: header_checksum,
			});
		}

		let payload = if advertised_len == HEADER_LEN {
			Vec::new()
		} else {
			if advertised_len == HEADER_LEN + 1 {
				return Err(Iap2Error::MissingPayloadData);
			}

			let payload = frame[9..advertised_len - 1].to_vec();
			let expected_payload_checksum = checksum::calculate(&payload);
			let payload_checksum = frame[advertised_len - 1];
			if expected_payload_checksum != payload_checksum {
				return Err(Iap2Error::InvalidPayloadChecksum {
					expected: expected_payload_checksum,
					actual: payload_checksum,
				});
			}

			payload
		};

		Self::new(
			control,
			sequence_number,
			acknowledgement_number,
			session_id,
			payload,
		)
	}

	pub fn sync_payload(&self) -> Result<LinkSynchronizationPayload, Iap2Error> {
		match self.packet_kind() {
			PacketKind::Syn | PacketKind::SynAck => {
				LinkSynchronizationPayload::decode(&self.payload)
			}
			_ => Err(Iap2Error::UnexpectedPacketKind("SYN or SYN+ACK packet")),
		}
	}

	pub fn eak_payload(&self) -> Result<&[u8], Iap2Error> {
		match self.packet_kind() {
			PacketKind::ExtendedAck => Ok(&self.payload),
			_ => Err(Iap2Error::UnexpectedPacketKind("EAK packet")),
		}
	}

	pub fn session_payload(&self) -> Result<(u8, &[u8]), Iap2Error> {
		match self.packet_kind() {
			PacketKind::Session => Ok((self.session_id, &self.payload)),
			_ => Err(Iap2Error::UnexpectedPacketKind("session packet")),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{LinkPacket, PacketKind};
	use crate::iap2::sync::{LinkSynchronizationPayload, SessionDescriptor, TransportProfile};

	#[test]
	fn session_packet_round_trip() {
		let packet = LinkPacket::session(7, 3, 1, vec![0x10, 0x20, 0x30]).expect("build packet");
		let encoded = packet.encode().expect("encode packet");
		let decoded = LinkPacket::decode(&encoded).expect("decode packet");

		assert_eq!(packet, decoded);
		assert_eq!(decoded.packet_kind(), PacketKind::Session);
	}

	#[test]
	fn syn_packet_round_trip() {
		let syn_payload = LinkSynchronizationPayload::recommended_for_transport(
			TransportProfile::Bluetooth,
			vec![SessionDescriptor {
				identifier: 1,
				session_type: 4,
				session_version: 1,
			}],
		);
		let packet = LinkPacket::syn(44, syn_payload.clone()).expect("build syn packet");
		let encoded = packet.encode().expect("encode packet");
		let decoded = LinkPacket::decode(&encoded).expect("decode packet");

		assert_eq!(decoded.packet_kind(), PacketKind::Syn);
		assert_eq!(
			decoded.sync_payload().expect("decode syn payload"),
			syn_payload
		);
	}

	#[test]
	fn decode_rejects_bad_payload_checksum() {
		let packet = LinkPacket::session(4, 3, 2, vec![0xaa, 0xbb]).expect("build packet");
		let mut encoded = packet.encode().expect("encode packet");
		let last_index = encoded.len() - 1;
		encoded[last_index] = encoded[last_index].wrapping_add(1);

		assert!(LinkPacket::decode(&encoded).is_err());
	}
}
