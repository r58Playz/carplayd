use super::packet::{HEADER_LEN, LinkPacket, START_OF_PACKET};

#[derive(Debug, Default)]
pub struct PacketStreamParser {
	buffer: Vec<u8>,
}

impl PacketStreamParser {
	pub fn push_bytes(&mut self, bytes: &[u8]) {
		self.buffer.extend_from_slice(bytes);
	}

	pub fn clear(&mut self) {
		self.buffer.clear();
	}

	pub fn buffered_len(&self) -> usize {
		self.buffer.len()
	}

	pub fn next_packet(&mut self) -> Option<LinkPacket> {
		loop {
			if self.buffer.len() < 2 {
				return None;
			}

			let start = self
				.buffer
				.windows(2)
				.position(|window| window == START_OF_PACKET);

			let start = match start {
				Some(start) => start,
				None => {
					let keep_last_ff = self.buffer.last().copied() == Some(START_OF_PACKET[0]);
					self.buffer.clear();
					if keep_last_ff {
						self.buffer.push(START_OF_PACKET[0]);
					}
					return None;
				}
			};

			if start > 0 {
				self.buffer.drain(0..start);
			}

			if self.buffer.len() < 4 {
				return None;
			}

			let advertised_len = usize::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]]));
			if advertised_len < HEADER_LEN {
				self.buffer.drain(0..1);
				continue;
			}

			if self.buffer.len() < advertised_len {
				return None;
			}

			match LinkPacket::decode(&self.buffer[..advertised_len]) {
				Ok(packet) => {
					self.buffer.drain(0..advertised_len);
					return Some(packet);
				}
				Err(_) => {
					self.buffer.drain(0..1);
				}
			}
		}
	}

	pub fn drain_packets(&mut self) -> Vec<LinkPacket> {
		let mut packets = Vec::new();
		while let Some(packet) = self.next_packet() {
			packets.push(packet);
		}

		packets
	}
}

#[cfg(test)]
mod tests {
	use super::PacketStreamParser;
	use crate::iap2::packet::LinkPacket;

	#[test]
	fn parses_packets_across_chunk_boundaries() {
		let packet = LinkPacket::session(5, 4, 1, vec![0x01, 0x02, 0x03]).expect("build packet");
		let encoded = packet.encode().expect("encode packet");

		let mut parser = PacketStreamParser::default();
		parser.push_bytes(&encoded[..2]);
		assert!(parser.next_packet().is_none());

		parser.push_bytes(&encoded[2..]);
		let parsed = parser.next_packet().expect("packet should be parsed");
		assert_eq!(parsed, packet);
	}

	#[test]
	fn skips_invalid_frames_and_finds_next_packet() {
		let packet = LinkPacket::ack(7, 6).expect("build ack packet");
		let encoded = packet.encode().expect("encode packet");

		let mut parser = PacketStreamParser::default();
		parser.push_bytes(&[0xff, 0x5a, 0x00, 0x01, 0xaa]);
		parser.push_bytes(&encoded);

		let parsed = parser
			.next_packet()
			.expect("valid packet should still parse");
		assert_eq!(parsed, packet);
	}
}
