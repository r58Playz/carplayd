//! iAP2 link-layer driver and frame recovery (spec §56.2 / §56.3).
//!
//! [`Link`] runs the whole link handshake for you — it echoes the detection
//! sequence, answers the car's `SYN` with a `SYN+ACK`, detects establishment, and
//! auto-acknowledges inbound data packets — so callers only deal with
//! control-session *payloads*. Feed it the data phase of each EP0 `SET_REPORT` with
//! [`receive`](Link::receive); it hands back HID reports to write and the payloads
//! the car sent. Send your own messages with [`send_control`](Link::send_control).

use super::{
	hid::{wrap_input, Reassembler},
	packet::{Control, LinkPacket, DETECTION},
};

/// One session advertised in a Link Synchronization Payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
	pub id: u8,
	/// 0 = control, 1 = file transfer, 2 = external accessory (§56.3.1.1).
	pub typ: u8,
	pub version: u8,
}

/// Parsed Link Synchronization Payload (LSP, version-1 field layout, §56.2.2).
#[derive(Clone, Debug)]
pub struct LinkSync {
	pub link_version: u8,
	pub max_outstanding: u8,
	pub max_recv_len: u16,
	pub retransmit_timeout: u16,
	pub cumulative_ack_timeout: u16,
	pub max_retransmit: u8,
	pub max_cumulative_ack: u8,
	pub sessions: Vec<Session>,
}

impl LinkSync {
	/// Parse an LSP from a `SYN` packet's payload. Assumes the version-1 field
	/// layout; CarPlay negotiates link version 2 — the fixed header is the same,
	/// but confirm the session table against a real capture if it looks off. Since
	/// [`Link`] echoes the payload verbatim, exact parsing is only needed to
	/// discover the control-session id.
	pub fn parse(p: &[u8]) -> Option<Self> {
		if p.len() < 10 {
			return None;
		}
		let mut sessions = Vec::new();
		let mut i = 10;
		while i + 3 <= p.len() {
			sessions.push(Session { id: p[i], typ: p[i + 1], version: p[i + 2] });
			i += 3;
		}
		Some(Self {
			link_version: p[0],
			max_outstanding: p[1],
			max_recv_len: u16::from_be_bytes([p[2], p[3]]),
			retransmit_timeout: u16::from_be_bytes([p[4], p[5]]),
			cumulative_ack_timeout: u16::from_be_bytes([p[6], p[7]]),
			max_retransmit: p[8],
			max_cumulative_ack: p[9],
			sessions,
		})
	}

	/// The control session's identifier (session type 0), if advertised.
	pub fn control_session(&self) -> Option<u8> {
		self.sessions.iter().find(|s| s.typ == 0).map(|s| s.id)
	}
}

/// A frame recovered from a completed HID report set.
#[derive(Clone, Debug)]
pub enum Frame {
	/// The iAP2 detection sequence (`FF 55 02 00 EE 10`).
	Detection,
	/// A link packet.
	Link(LinkPacket),
}

/// Split a reassembled report set into frames, skipping trailing zero padding.
/// Stops at the first byte that is neither a detection nor a valid link-packet
/// start (e.g. padding), so a set of `[packet][packet][0x00…]` yields both packets.
pub fn parse_set(set: &[u8]) -> Vec<Frame> {
	let mut out = Vec::new();
	let mut i = 0;
	while i < set.len() {
		if set[i] == 0x00 {
			break; // zero padding at the tail of the report set
		}
		if set[i..].starts_with(&DETECTION) {
			out.push(Frame::Detection);
			i += DETECTION.len();
			continue;
		}
		match LinkPacket::parse(&set[i..]) {
			Ok((pkt, used)) => {
				out.push(Frame::Link(pkt));
				i += used;
			}
			Err(_) => break, // cannot make progress on this set
		}
	}
	out
}

/// What one [`Link::receive`] call produced.
#[derive(Default, Debug)]
pub struct LinkOutput {
	/// HID Input reports to write to the interrupt IN endpoint, in order.
	pub reports: Vec<Vec<u8>>,
	/// Control-session (or other) payloads the car sent, as `(session_id, bytes)`.
	pub payloads: Vec<(u8, Vec<u8>)>,
	/// `true` on the single call where the link became established (handshake done).
	/// This is your cue to send the first message the device drives (e.g.
	/// `RequestAuthenticationCertificate`).
	pub established: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
	AwaitingSyn,
	AwaitingEstablish,
	Established,
}

/// iAP2 link-layer driver: handshake, sequence/ack bookkeeping, and HID (de)framing.
///
/// It deliberately omits retransmission, the sliding window, and EAK — over USB the
/// transport is reliable and the handshake keeps at most one packet outstanding, so
/// they are unnecessary. Duplicate `SYN`s (resend the cached `SYN+ACK`) and
/// duplicate data packets (re-ack, don't re-deliver) are handled.
pub struct Link {
	next_seq: u8,
	last_rx: Option<u8>,
	reasm: Reassembler,
	state: State,
	control_session: Option<u8>,
	/// `(car SYN seq, cached SYN+ACK wire bytes)`, for idempotent SYN retransmits.
	cached_synack: Option<(u8, Vec<u8>)>,
	last_payload_seq: Option<u8>,
}

impl Default for Link {
	fn default() -> Self {
		Self::new()
	}
}

impl Link {
	/// Create a fresh link, ready to receive the car's detection and `SYN`.
	pub fn new() -> Self {
		Self {
			next_seq: 0, // any fixed initial PSN works for a device impl
			last_rx: None,
			reasm: Reassembler::new(),
			state: State::AwaitingSyn,
			control_session: None,
			cached_synack: None,
			last_payload_seq: None,
		}
	}

	/// Whether the link handshake has completed.
	pub fn established(&self) -> bool {
		self.state == State::Established
	}

	/// The negotiated control-session id (learned from the car's `SYN`), if known.
	pub fn control_session(&self) -> Option<u8> {
		self.control_session
	}

	/// Feed the data phase of one EP0 `SET_REPORT` (`[LCB][iAP bytes…]` — the report
	/// *without* its report-id byte, which `SET_REPORT` conveys in `wValue`). Drives
	/// the handshake automatically; returns reports to write and payloads received.
	///
	/// If reassembly looks wrong on real hardware, check whether the car prefixes
	/// the report-id in the `SET_REPORT` data phase — if so, strip the first byte
	/// before calling this.
	pub fn receive(&mut self, report_data: &[u8]) -> LinkOutput {
		let mut out = LinkOutput::default();
		let Some(set) = self.reasm.push(report_data) else {
			return out;
		};
		for frame in parse_set(&set) {
			match frame {
				Frame::Detection => out.reports.extend(wrap_input(&DETECTION)),
				Frame::Link(pkt) => self.on_packet(pkt, &mut out),
			}
		}
		out
	}

	/// Frame a payload for the control session into HID reports ready to write to
	/// the interrupt IN endpoint. Returns empty if the control session isn't known
	/// yet (i.e. before the handshake).
	pub fn send_control(&mut self, payload: Vec<u8>) -> Vec<Vec<u8>> {
		match self.control_session {
			Some(session) => self.send_payload(session, payload),
			None => Vec::new(),
		}
	}

	/// Frame a payload for an explicit session into HID reports.
	pub fn send_payload(&mut self, session: u8, payload: Vec<u8>) -> Vec<Vec<u8>> {
		let pkt = self.build(Control::ACK, session, payload);
		wrap_input(&pkt.to_bytes())
	}

	fn on_packet(&mut self, pkt: LinkPacket, out: &mut LinkOutput) {
		if pkt.control.contains(Control::RST) {
			return; // RST is device→accessory only; ignore if ever received
		}
		if pkt.control.contains(Control::SYN) {
			self.on_syn(pkt, out);
			return;
		}

		// Non-SYN packet: the car's ACK of our SYN+ACK establishes the link.
		if self.state == State::AwaitingEstablish {
			self.state = State::Established;
			out.established = true;
		}

		if pkt.has_payload() {
			self.observe(&pkt);
			if self.last_payload_seq != Some(pkt.seq) {
				self.last_payload_seq = Some(pkt.seq);
				out.payloads.push((pkt.session, pkt.payload));
			}
			// Cumulative ACK (§56.2.7.4). Extra bare ACKs are always acceptable, so
			// this is sent even for a retransmit the caller never sees.
			let ack = self.build(Control::ACK, 0, Vec::new());
			out.reports.extend(wrap_input(&ack.to_bytes()));
		}
	}

	fn on_syn(&mut self, pkt: LinkPacket, out: &mut LinkOutput) {
		// Duplicate SYN (same seq we already answered): resend the cached SYN+ACK
		// verbatim so our sequence number doesn't drift.
		if let Some((seq, bytes)) = &self.cached_synack {
			if *seq == pkt.seq {
				out.reports.extend(wrap_input(bytes));
				return;
			}
		}
		if let Some(lsp) = LinkSync::parse(&pkt.payload) {
			self.control_session = lsp.control_session();
		}
		self.observe(&pkt);
		let synack = self.build(Control::SYN | Control::ACK, 0, pkt.payload.clone());
		let bytes = synack.to_bytes();
		self.cached_synack = Some((pkt.seq, bytes.clone()));
		self.state = State::AwaitingEstablish;
		out.reports.extend(wrap_input(&bytes));
	}

	fn ack_num(&self) -> u8 {
		self.last_rx.unwrap_or(0)
	}

	/// Record an incoming packet so subsequent outgoing packets acknowledge it.
	/// Only packets that carry a payload or set `SYN` advance the acknowledgement.
	fn observe(&mut self, pkt: &LinkPacket) {
		if pkt.has_payload() || pkt.control.contains(Control::SYN) {
			self.last_rx = Some(pkt.seq);
		}
	}

	/// Build a packet, stamping the current sequence and acknowledgement numbers.
	/// The sequence number advances only when a payload is present (§56.2.1.4).
	fn build(&mut self, control: Control, session: u8, payload: Vec<u8>) -> LinkPacket {
		let seq = self.next_seq;
		if !payload.is_empty() {
			self.next_seq = self.next_seq.wrapping_add(1);
		}
		LinkPacket::new(control, seq, self.ack_num(), session, payload)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// Wrap iAP bytes as the data phase of a SET_REPORT (LCB-first, no report id).
	fn as_report(iap: &[u8]) -> Vec<u8> {
		let mut v = vec![0x00]; // LCB: single, self-contained set
		v.extend_from_slice(iap);
		v
	}

	fn car_syn(seq: u8, control_session: u8) -> LinkPacket {
		// LSP: link v2, max-out 5, ..., one control session (type 0).
		let lsp = vec![
			0x02, 0x05, 0x10, 0x00, 0, 0, 0, 0, 30, 0, control_session, 0x00, 0x01,
		];
		LinkPacket::new(Control::SYN, seq, 0, 0, lsp)
	}

	fn only_link(set_reports: &[Vec<u8>]) -> Vec<LinkPacket> {
		// Reassemble device→car reports and pull out the link packets.
		let mut re = Reassembler::new();
		let mut pkts = Vec::new();
		for r in set_reports {
			if let Some(set) = re.push(&r[1..]) {
				for f in parse_set(&set) {
					if let Frame::Link(p) = f {
						pkts.push(p);
					}
				}
			}
		}
		pkts
	}

	#[test]
	fn full_handshake() {
		let mut link = Link::new();

		// Detection → echoed.
		let out = link.receive(&as_report(&DETECTION));
		assert!(!out.established);
		assert_eq!(only_link(&out.reports).len(), 0); // detection echo isn't a link packet

		// Car SYN → SYN+ACK, control session learned.
		let out = link.receive(&as_report(&car_syn(200, 0x0A).to_bytes()));
		assert_eq!(link.control_session(), Some(0x0A));
		let synack = &only_link(&out.reports)[0];
		assert!(synack.control.contains(Control::SYN));
		assert!(synack.control.contains(Control::ACK));
		assert_eq!(synack.ack, 200);
		assert!(!out.established);

		// Car's final ACK → established.
		let final_ack = LinkPacket::new(Control::ACK, 201, 0, 0, Vec::new());
		let out = link.receive(&as_report(&final_ack.to_bytes()));
		assert!(out.established);
		assert!(link.established());
	}

	#[test]
	fn delivers_and_acks_payload() {
		let mut link = Link::new();
		link.receive(&as_report(&car_syn(200, 0x0A).to_bytes()));
		link.receive(&as_report(&LinkPacket::new(Control::ACK, 201, 0, 0, vec![]).to_bytes()));

		// Car sends a control-session message.
		let data = LinkPacket::new(Control::ACK, 201, 0, 0x0A, vec![0x40, 0x40, 0x00, 0x06, 0xAA, 0x01]);
		let out = link.receive(&as_report(&data.to_bytes()));
		assert_eq!(out.payloads.len(), 1);
		assert_eq!(out.payloads[0].0, 0x0A); // session id
		assert_eq!(out.payloads[0].1, vec![0x40, 0x40, 0x00, 0x06, 0xAA, 0x01]);
		let ack = &only_link(&out.reports)[0];
		assert!(ack.control.contains(Control::ACK));
		assert_eq!(ack.ack, 201); // acknowledges the data packet
		assert!(!ack.has_payload());
	}

	#[test]
	fn duplicate_data_is_not_redelivered() {
		let mut link = Link::new();
		link.receive(&as_report(&car_syn(200, 0x0A).to_bytes()));
		link.receive(&as_report(&LinkPacket::new(Control::ACK, 201, 0, 0, vec![]).to_bytes()));
		let data = LinkPacket::new(Control::ACK, 201, 0, 0x0A, vec![1, 2, 3]);
		assert_eq!(link.receive(&as_report(&data.to_bytes())).payloads.len(), 1);
		// same seq again → re-acked but not re-delivered
		let out = link.receive(&as_report(&data.to_bytes()));
		assert_eq!(out.payloads.len(), 0);
		assert_eq!(only_link(&out.reports).len(), 1); // still ACKs it
	}

	#[test]
	fn duplicate_syn_resends_cached_synack() {
		let mut link = Link::new();
		let first = link.receive(&as_report(&car_syn(200, 0x0A).to_bytes()));
		let seq_after = link.next_seq;
		let again = link.receive(&as_report(&car_syn(200, 0x0A).to_bytes()));
		assert_eq!(first.reports, again.reports); // identical SYN+ACK bytes
		assert_eq!(link.next_seq, seq_after); // no sequence drift
	}

	#[test]
	fn send_control_frames_to_the_learned_session() {
		let mut link = Link::new();
		link.receive(&as_report(&car_syn(200, 0x0A).to_bytes()));
		link.receive(&as_report(&LinkPacket::new(Control::ACK, 201, 0, 0, vec![]).to_bytes()));
		// RequestAuthenticationCertificate (0xAA00)
		let reports = link.send_control(vec![0x40, 0x40, 0x00, 0x06, 0xAA, 0x00]);
		let pkt = &only_link(&reports)[0];
		assert_eq!(pkt.session, 0x0A);
		// Acks the SYN (200), not the car's pure final ACK — pure ACKs are not
		// themselves acknowledged (§56.2.7.4), so they don't advance the ack field.
		assert_eq!(pkt.ack, 200);
		assert_eq!(pkt.payload, vec![0x40, 0x40, 0x00, 0x06, 0xAA, 0x00]);
	}

	#[test]
	fn lsp_finds_control_session() {
		let lsp = vec![0x02, 0x05, 0x10, 0x00, 0, 0, 0, 0, 30, 0, 0x0A, 0x00, 0x01, 0x0B, 0x02, 0x01];
		let parsed = LinkSync::parse(&lsp).unwrap();
		assert_eq!(parsed.link_version, 2);
		assert_eq!(parsed.control_session(), Some(0x0A));
		assert_eq!(parsed.sessions.len(), 2);
	}
}
