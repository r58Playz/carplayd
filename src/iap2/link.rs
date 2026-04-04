use std::{
	collections::{BTreeSet, VecDeque},
	time::{Duration, Instant},
};

use super::{
	error::Iap2Error,
	packet::{LinkPacket, PacketKind},
};

#[derive(Debug, Clone)]
pub struct LinkRecord {
	next_sent_psn: u8,
	last_received_in_sequence_psn: u8,
	has_received_sequence: bool,
	received_out_of_sequence_psns: BTreeSet<u8>,
}

impl LinkRecord {
	pub fn new(initial_sent_psn: u8) -> Self {
		Self {
			next_sent_psn: initial_sent_psn,
			last_received_in_sequence_psn: 0,
			has_received_sequence: false,
			received_out_of_sequence_psns: BTreeSet::new(),
		}
	}

	pub fn next_sequence_for_payload(&mut self) -> u8 {
		let sequence = self.next_sent_psn;
		self.next_sent_psn = self.next_sent_psn.wrapping_add(1);
		sequence
	}

	pub fn current_sequence_without_increment(&self) -> u8 {
		self.next_sent_psn
	}

	pub fn last_received_in_sequence(&self) -> u8 {
		self.last_received_in_sequence_psn
	}

	pub fn set_last_received_in_sequence(&mut self, sequence: u8) {
		self.last_received_in_sequence_psn = sequence;
		self.has_received_sequence = true;
	}

	pub fn window_left_edge(&self) -> u8 {
		self.last_received_in_sequence_psn.wrapping_add(1)
	}

	pub fn window_right_edge(&self, max_outstanding_packets: u8) -> u8 {
		self.last_received_in_sequence_psn
			.wrapping_add(max_outstanding_packets)
	}

	pub fn sequence_in_window(&self, sequence: u8, max_outstanding_packets: u8) -> bool {
		let left = self.window_left_edge();
		let distance = sequence.wrapping_sub(left);
		distance < max_outstanding_packets
	}

	pub fn on_sequence_received(
		&mut self,
		sequence: u8,
		max_outstanding_packets: u8,
	) -> Result<ReceiveAction, Iap2Error> {
		if !(1..=127).contains(&max_outstanding_packets) {
			return Err(Iap2Error::InvalidSyncPayload(
				"maximum outstanding packets must be 1..=127",
			));
		}

		if !self.has_received_sequence {
			self.set_last_received_in_sequence(sequence);
			return Ok(ReceiveAction::Ack {
				cumulative_ack: self.last_received_in_sequence_psn,
			});
		}

		let expected = self.window_left_edge();
		if sequence == expected {
			self.last_received_in_sequence_psn = sequence;

			while self
				.received_out_of_sequence_psns
				.remove(&self.last_received_in_sequence_psn.wrapping_add(1))
			{
				self.last_received_in_sequence_psn =
					self.last_received_in_sequence_psn.wrapping_add(1);
			}

			return Ok(ReceiveAction::Ack {
				cumulative_ack: self.last_received_in_sequence_psn,
			});
		}

		if self.sequence_in_window(sequence, max_outstanding_packets) {
			if !self.received_out_of_sequence_psns.insert(sequence) {
				return Ok(ReceiveAction::Duplicate {
					cumulative_ack: self.last_received_in_sequence_psn,
				});
			}

			return Ok(ReceiveAction::ExtendedAck {
				cumulative_ack: self.last_received_in_sequence_psn,
				out_of_sequence: self.ordered_out_of_sequence(max_outstanding_packets),
			});
		}

		Err(Iap2Error::SequenceOutsideWindow {
			sequence,
			left_edge: self.window_left_edge(),
			right_edge: self.window_right_edge(max_outstanding_packets),
		})
	}

	fn ordered_out_of_sequence(&self, max_outstanding_packets: u8) -> Vec<u8> {
		let left = self.window_left_edge();
		let mut values: Vec<u8> = self.received_out_of_sequence_psns.iter().copied().collect();
		values.sort_by_key(|sequence| sequence.wrapping_sub(left));
		values.truncate(usize::from(max_outstanding_packets));
		values
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveAction {
	Ack {
		cumulative_ack: u8,
	},
	ExtendedAck {
		cumulative_ack: u8,
		out_of_sequence: Vec<u8>,
	},
	Duplicate {
		cumulative_ack: u8,
	},
}

#[derive(Debug, Clone)]
pub struct InFlightPacket {
	pub packet: LinkPacket,
	pub retransmission_count: u8,
	pub last_sent_at: Instant,
}

#[derive(Debug)]
pub struct LinkTransmitWindow {
	peer_max_outstanding_packets: u8,
	retransmission_timeout: Duration,
	max_retransmissions: u8,
	in_flight: VecDeque<InFlightPacket>,
}

impl LinkTransmitWindow {
	pub fn new(
		peer_max_outstanding_packets: u8,
		retransmission_timeout: Duration,
		max_retransmissions: u8,
	) -> Result<Self, Iap2Error> {
		if !(1..=127).contains(&peer_max_outstanding_packets) {
			return Err(Iap2Error::InvalidSyncPayload(
				"peer maximum outstanding packets must be 1..=127",
			));
		}

		Ok(Self {
			peer_max_outstanding_packets,
			retransmission_timeout,
			max_retransmissions,
			in_flight: VecDeque::new(),
		})
	}

	pub fn can_send(&self) -> bool {
		self.in_flight.len() < usize::from(self.peer_max_outstanding_packets)
	}

	pub fn outstanding_packets(&self) -> usize {
		self.in_flight.len()
	}

	pub fn in_flight_sequences(&self) -> Vec<u8> {
		self.in_flight
			.iter()
			.map(|tracked| tracked.packet.sequence_number)
			.collect()
	}

	pub fn track_sent(&mut self, packet: LinkPacket, sent_at: Instant) -> Result<(), Iap2Error> {
		if !packet_requires_ack(&packet) {
			return Ok(());
		}

		if !self.can_send() {
			return Err(Iap2Error::FlowControlWindowFull);
		}

		self.in_flight.push_back(InFlightPacket {
			packet,
			retransmission_count: 0,
			last_sent_at: sent_at,
		});

		Ok(())
	}

	pub fn acknowledge(&mut self, cumulative_ack: u8) {
		let Some(oldest_sequence) = self
			.in_flight
			.front()
			.map(|tracked| tracked.packet.sequence_number)
		else {
			return;
		};

		let ack_distance = cumulative_ack.wrapping_sub(oldest_sequence);

		while let Some(front) = self.in_flight.front() {
			let seq_distance = front.packet.sequence_number.wrapping_sub(oldest_sequence);
			if seq_distance <= ack_distance {
				self.in_flight.pop_front();
			} else {
				break;
			}
		}
	}

	pub fn retransmissions_due(&mut self, now: Instant) -> Result<Vec<LinkPacket>, Iap2Error> {
		if self.retransmission_timeout.is_zero() || self.max_retransmissions == 0 {
			return Ok(Vec::new());
		}

		let mut due = Vec::new();
		for tracked in &mut self.in_flight {
			if now.saturating_duration_since(tracked.last_sent_at) < self.retransmission_timeout {
				continue;
			}

			if tracked.retransmission_count >= self.max_retransmissions {
				return Err(Iap2Error::MaxRetransmissionsReached {
					sequence: tracked.packet.sequence_number,
					attempts: tracked.retransmission_count,
					max_attempts: self.max_retransmissions,
				});
			}

			tracked.retransmission_count += 1;
			tracked.last_sent_at = now;
			due.push(tracked.packet.clone());
		}

		Ok(due)
	}
}

fn packet_requires_ack(packet: &LinkPacket) -> bool {
	matches!(
		packet.packet_kind(),
		PacketKind::Syn | PacketKind::SynAck | PacketKind::Session
	)
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use super::{LinkRecord, LinkTransmitWindow, ReceiveAction};
	use crate::iap2::packet::LinkPacket;

	#[test]
	fn receive_window_generates_eak_for_out_of_sequence_packet() {
		let mut record = LinkRecord::new(10);

		let first = record
			.on_sequence_received(5, 4)
			.expect("first packet should be accepted");
		assert_eq!(first, ReceiveAction::Ack { cumulative_ack: 5 });

		let eak = record
			.on_sequence_received(7, 4)
			.expect("out-of-sequence packet should be accepted");
		assert_eq!(
			eak,
			ReceiveAction::ExtendedAck {
				cumulative_ack: 5,
				out_of_sequence: vec![7],
			}
		);

		let ack = record
			.on_sequence_received(6, 4)
			.expect("missing packet closes the gap");
		assert_eq!(ack, ReceiveAction::Ack { cumulative_ack: 7 });
	}

	#[test]
	fn transmit_window_enforces_flow_control_and_acks() {
		let mut window = LinkTransmitWindow::new(2, Duration::from_millis(200), 3)
			.expect("window should be valid");
		let now = Instant::now();

		let p1 = LinkPacket::session(1, 0, 1, vec![0x01]).expect("build packet 1");
		let p2 = LinkPacket::session(2, 0, 1, vec![0x02]).expect("build packet 2");
		let p3 = LinkPacket::session(3, 0, 1, vec![0x03]).expect("build packet 3");

		window.track_sent(p1, now).expect("track packet 1");
		window.track_sent(p2, now).expect("track packet 2");
		assert!(window.track_sent(p3, now).is_err());

		window.acknowledge(1);
		assert_eq!(window.outstanding_packets(), 1);
	}

	#[test]
	fn transmit_window_retransmits_when_timer_expires() {
		let mut window = LinkTransmitWindow::new(3, Duration::from_millis(50), 2)
			.expect("window should be valid");
		let now = Instant::now();

		let packet = LinkPacket::session(1, 0, 1, vec![0x99]).expect("build packet");
		window
			.track_sent(packet.clone(), now)
			.expect("track packet");

		let due = window
			.retransmissions_due(now + Duration::from_millis(60))
			.expect("retransmission check should succeed");

		assert_eq!(due, vec![packet]);
	}
}
