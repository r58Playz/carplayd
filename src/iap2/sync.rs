use super::error::Iap2Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionDescriptor {
	pub identifier: u8,
	pub session_type: u8,
	pub session_version: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkTuning {
	pub max_outstanding_packets: u8,
	pub max_received_packet_length: u16,
	pub retransmission_timeout_ms: u16,
	pub cumulative_ack_timeout_ms: u16,
	pub max_retransmissions: u8,
	pub max_cumulative_acks: u8,
}

impl Default for LinkTuning {
	fn default() -> Self {
		Self {
			max_outstanding_packets: 1,
			max_received_packet_length: 128,
			retransmission_timeout_ms: 1000,
			cumulative_ack_timeout_ms: 10,
			max_retransmissions: 30,
			max_cumulative_acks: 0,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportProfile {
	UsbHostFullSpeed,
	UsbDeviceFullSpeed,
	Bluetooth,
	Uart57Kbps,
}

impl TransportProfile {
	pub const fn recommended_tuning(self) -> LinkTuning {
		match self {
			Self::UsbHostFullSpeed | Self::UsbDeviceFullSpeed => LinkTuning {
				max_outstanding_packets: 5,
				max_received_packet_length: 4096,
				retransmission_timeout_ms: 2000,
				cumulative_ack_timeout_ms: 22,
				max_retransmissions: 30,
				max_cumulative_acks: 3,
			},
			Self::Bluetooth => LinkTuning {
				max_outstanding_packets: 5,
				max_received_packet_length: 2048,
				retransmission_timeout_ms: 1500,
				cumulative_ack_timeout_ms: 73,
				max_retransmissions: 30,
				max_cumulative_acks: 3,
			},
			Self::Uart57Kbps => LinkTuning {
				max_outstanding_packets: 4,
				max_received_packet_length: 256,
				retransmission_timeout_ms: 1000,
				cumulative_ack_timeout_ms: 133,
				max_retransmissions: 30,
				max_cumulative_acks: 2,
			},
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSynchronizationPayload {
	pub link_version: u8,
	pub max_outstanding_packets: u8,
	pub max_received_packet_length: u16,
	pub retransmission_timeout_ms: u16,
	pub cumulative_ack_timeout_ms: u16,
	pub max_retransmissions: u8,
	pub max_cumulative_acks: u8,
	pub sessions: Vec<SessionDescriptor>,
}

impl Default for LinkSynchronizationPayload {
	fn default() -> Self {
		Self::with_tuning(LinkTuning::default(), Vec::new())
	}
}

impl LinkSynchronizationPayload {
	pub const LINK_VERSION_1: u8 = 1;
	pub const BASE_LENGTH: usize = 10;

	pub fn with_tuning(tuning: LinkTuning, sessions: Vec<SessionDescriptor>) -> Self {
		Self {
			link_version: Self::LINK_VERSION_1,
			max_outstanding_packets: tuning.max_outstanding_packets,
			max_received_packet_length: tuning.max_received_packet_length,
			retransmission_timeout_ms: tuning.retransmission_timeout_ms,
			cumulative_ack_timeout_ms: tuning.cumulative_ack_timeout_ms,
			max_retransmissions: tuning.max_retransmissions,
			max_cumulative_acks: tuning.max_cumulative_acks,
			sessions,
		}
	}

	pub fn recommended_for_transport(
		profile: TransportProfile,
		sessions: Vec<SessionDescriptor>,
	) -> Self {
		Self::with_tuning(profile.recommended_tuning(), sessions)
	}

	pub fn encode(&self) -> Result<Vec<u8>, Iap2Error> {
		self.validate()?;

		let mut out = Vec::with_capacity(Self::BASE_LENGTH + self.sessions.len() * 3);
		out.push(self.link_version);
		out.push(self.max_outstanding_packets);
		out.extend_from_slice(&self.max_received_packet_length.to_be_bytes());
		out.extend_from_slice(&self.retransmission_timeout_ms.to_be_bytes());
		out.extend_from_slice(&self.cumulative_ack_timeout_ms.to_be_bytes());
		out.push(self.max_retransmissions);
		out.push(self.max_cumulative_acks);

		for session in &self.sessions {
			out.push(session.identifier);
			out.push(session.session_type);
			out.push(session.session_version);
		}

		Ok(out)
	}

	pub fn decode(bytes: &[u8]) -> Result<Self, Iap2Error> {
		if bytes.len() < Self::BASE_LENGTH {
			return Err(Iap2Error::InvalidSyncPayload(
				"payload is shorter than the v1 synchronization header",
			));
		}

		let session_table = bytes.len() - Self::BASE_LENGTH;
		if session_table % 3 != 0 {
			return Err(Iap2Error::InvalidSessionTableLength(session_table));
		}

		let mut sessions = Vec::with_capacity(session_table / 3);
		for chunk in bytes[Self::BASE_LENGTH..].chunks_exact(3) {
			sessions.push(SessionDescriptor {
				identifier: chunk[0],
				session_type: chunk[1],
				session_version: chunk[2],
			});
		}

		let payload = Self {
			link_version: bytes[0],
			max_outstanding_packets: bytes[1],
			max_received_packet_length: u16::from_be_bytes([bytes[2], bytes[3]]),
			retransmission_timeout_ms: u16::from_be_bytes([bytes[4], bytes[5]]),
			cumulative_ack_timeout_ms: u16::from_be_bytes([bytes[6], bytes[7]]),
			max_retransmissions: bytes[8],
			max_cumulative_acks: bytes[9],
			sessions,
		};

		payload.validate()?;
		Ok(payload)
	}

	pub fn validate(&self) -> Result<(), Iap2Error> {
		if self.link_version != Self::LINK_VERSION_1 {
			return Err(Iap2Error::InvalidSyncPayload(
				"only link version 1 is supported",
			));
		}

		if !(1..=127).contains(&self.max_outstanding_packets) {
			return Err(Iap2Error::InvalidSyncPayload(
				"maximum outstanding packets must be 1..=127",
			));
		}

		if self.max_received_packet_length < 24 {
			return Err(Iap2Error::InvalidSyncPayload(
				"maximum received packet length must be >= 24",
			));
		}

		if self.is_zero_ack_retransmit() {
			if self.max_cumulative_acks != 0 {
				return Err(Iap2Error::InvalidSyncPayload(
					"max cumulative acknowledgements must be 0 in zero-ack mode",
				));
			}
		} else {
			if self.retransmission_timeout_ms < 20 {
				return Err(Iap2Error::InvalidSyncPayload(
					"retransmission timeout must be 20..=65535 or 0 in zero-ack mode",
				));
			}

			if self.cumulative_ack_timeout_ms < 10
				|| self.cumulative_ack_timeout_ms > (self.retransmission_timeout_ms / 2)
			{
				return Err(Iap2Error::InvalidSyncPayload(
					"cumulative acknowledgement timeout out of range",
				));
			}

			if !(1..=30).contains(&self.max_retransmissions) {
				return Err(Iap2Error::InvalidSyncPayload(
					"maximum retransmissions must be 1..=30 or 0 in zero-ack mode",
				));
			}
		}

		if self.max_cumulative_acks > self.max_outstanding_packets.min(127) {
			return Err(Iap2Error::InvalidSyncPayload(
				"maximum cumulative acknowledgements exceeds allowed value",
			));
		}

		let mut seen = [false; 256];
		for session in &self.sessions {
			if session.identifier == 0 {
				return Err(Iap2Error::InvalidSyncPayload(
					"session identifier 0 is not allowed",
				));
			}

			let index = usize::from(session.identifier);
			if seen[index] {
				return Err(Iap2Error::DuplicateSessionId(session.identifier));
			}
			seen[index] = true;
		}

		Ok(())
	}

	pub fn is_zero_ack_retransmit(&self) -> bool {
		self.retransmission_timeout_ms == 0
			&& self.cumulative_ack_timeout_ms == 0
			&& self.max_retransmissions == 0
			&& self.max_cumulative_acks == 0
	}

	pub fn has_matching_negotiables(&self, peer: &Self) -> bool {
		self.link_version == peer.link_version
			&& self.retransmission_timeout_ms == peer.retransmission_timeout_ms
			&& self.cumulative_ack_timeout_ms == peer.cumulative_ack_timeout_ms
			&& self.max_retransmissions == peer.max_retransmissions
			&& self.max_cumulative_acks == peer.max_cumulative_acks
			&& self.sessions == peer.sessions
	}

	pub fn shared_sessions(&self, peer: &Self) -> Vec<SessionDescriptor> {
		let mut shared = Vec::new();
		for session in &self.sessions {
			if peer.sessions.contains(session) {
				shared.push(*session);
			}
		}

		shared
	}

	pub fn align_to_peer(&self, peer: &Self) -> Result<Self, Iap2Error> {
		peer.validate()?;

		let mut aligned = self.clone();
		aligned.link_version = peer.link_version;
		aligned.retransmission_timeout_ms = peer.retransmission_timeout_ms;
		aligned.cumulative_ack_timeout_ms = peer.cumulative_ack_timeout_ms;
		aligned.max_retransmissions = peer.max_retransmissions;
		aligned.max_cumulative_acks = peer
			.max_cumulative_acks
			.min(aligned.max_outstanding_packets);
		aligned.sessions = self.shared_sessions(peer);
		aligned.validate()?;

		Ok(aligned)
	}
}

#[cfg(test)]
mod tests {
	use super::{LinkSynchronizationPayload, LinkTuning, SessionDescriptor, TransportProfile};

	#[test]
	fn encode_decode_round_trip() {
		let payload = LinkSynchronizationPayload::with_tuning(
			TransportProfile::Bluetooth.recommended_tuning(),
			vec![SessionDescriptor {
				identifier: 1,
				session_type: 0x01,
				session_version: 1,
			}],
		);

		let encoded = payload.encode().expect("encode should work");
		let decoded = LinkSynchronizationPayload::decode(&encoded).expect("decode should work");

		assert_eq!(payload, decoded);
	}

	#[test]
	fn zero_ack_mode_validates() {
		let payload = LinkSynchronizationPayload::with_tuning(
			LinkTuning {
				max_outstanding_packets: 5,
				max_received_packet_length: 2048,
				retransmission_timeout_ms: 0,
				cumulative_ack_timeout_ms: 0,
				max_retransmissions: 0,
				max_cumulative_acks: 0,
			},
			vec![SessionDescriptor {
				identifier: 7,
				session_type: 0x10,
				session_version: 1,
			}],
		);

		assert!(payload.validate().is_ok());
	}

	#[test]
	fn align_to_peer_adopts_peer_negotiables() {
		let local = LinkSynchronizationPayload::with_tuning(
			TransportProfile::UsbHostFullSpeed.recommended_tuning(),
			vec![
				SessionDescriptor {
					identifier: 1,
					session_type: 1,
					session_version: 1,
				},
				SessionDescriptor {
					identifier: 2,
					session_type: 2,
					session_version: 1,
				},
			],
		);

		let peer = LinkSynchronizationPayload::with_tuning(
			TransportProfile::Bluetooth.recommended_tuning(),
			vec![
				SessionDescriptor {
					identifier: 1,
					session_type: 1,
					session_version: 1,
				},
				SessionDescriptor {
					identifier: 3,
					session_type: 3,
					session_version: 1,
				},
			],
		);

		let aligned = local.align_to_peer(&peer).expect("alignment should work");

		assert_eq!(
			aligned.retransmission_timeout_ms,
			peer.retransmission_timeout_ms
		);
		assert_eq!(aligned.sessions.len(), 1);
		assert_eq!(aligned.sessions[0].identifier, 1);
	}
}
