//! iAP2 link layer (spec §56.2.7): the operation record, and a `Stream`/`Sink` over it.
//!
//! [`LinkState`] is the whole protocol as a synchronous state machine — it takes frames
//! and a clock reading, and hands back application events plus frames to send. It holds no
//! I/O, so every rule in §56.2.7 is unit-testable without a car attached.
//!
//! [`Link`] wraps a framed transport (anything that is `Stream<Item = Result<Frame>> +
//! Sink<Frame>`) around that state machine and owns the timers, giving you a
//! `Stream<Item = Result<LinkEvent>> + Sink<(session, payload)>`.

use std::{
	collections::{BTreeSet, HashMap, VecDeque},
	pin::Pin,
	task::{Context, Poll, Waker},
	time::{Duration, Instant},
};

use bytes::Bytes;
use futures::{Sink, Stream, ready};
use log::{debug, trace, warn};
use tokio::time::{Sleep, sleep_until};

use crate::iap2::{
	error::Error,
	packet::{Control, Frame, LinkPacket},
	sync::LinkParams,
};

/// Which side of the connection we are. We impersonate an iPhone, so [`Role::Device`];
/// [`Role::Accessory`] exists for loopback tests and for probing a real device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
	Device,
	Accessory,
}

/// Something the link layer wants the layer above to know about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEvent {
	/// The peer's detection sequence arrived (§56.2.7.2).
	Detected,
	/// The handshake completed; the negotiated parameters are attached.
	Established(LinkParams),
	/// An in-order session payload.
	Payload { session: u8, data: Bytes },
	/// The peer sent `RST`, or we gave up and sent one.
	Reset,
	/// The peer sent `SLP` and is about to cut accessory power (§56.2.6).
	Sleep,
	/// The peer thinks we only speak iAP1 — our detection echo did not land.
	Iap1Detected,
}

/// Where the handshake has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
	/// Accessory role: sending detection at 1 Hz. Device role: waiting for a `SYN`.
	Detecting,
	/// A `SYN` is outstanding (accessory) or answered (device).
	Syncing,
	Established,
	/// The link is broken and needs a fresh transport connection (§56.2.7.7).
	Dead,
}

/// A packet we have sent and not yet seen acknowledged.
#[derive(Clone, Debug)]
struct Unacked {
	packet: LinkPacket,
	sent_at: Instant,
	retries: u8,
}

/// Interval at which an accessory sends the detection sequence (§56.2.7.2).
const DETECTION_INTERVAL: Duration = Duration::from_secs(1);
/// Interval at which an unanswered `SYN` is resent (§56.2.7.3).
const SYN_INTERVAL: Duration = Duration::from_secs(1);
/// Cap on payloads queued behind a full window, so a stalled link cannot exhaust memory.
const MAX_PENDING: usize = 64;

/// The iAP2 link operation record (Table 56-5) and the rules that act on it.
pub struct LinkState {
	role: Role,
	phase: Phase,
	/// Parameters in force: the synchronizing defaults until the peer's LSP arrives.
	params: LinkParams,
	/// What we advertise in our own `SYN` (accessory role only).
	proposal: LinkParams,

	// ── sending ──
	next_sent_psn: u8,
	initial_sent_psn: u8,
	unacked: VecDeque<Unacked>,
	pending: VecDeque<(u8, Bytes)>,

	// ── receiving ──
	initial_received_psn: Option<u8>,
	last_received_in_sequence_psn: Option<u8>,
	received_out_of_sequence: BTreeSet<u8>,
	/// Out-of-sequence payloads held back so delivery stays ordered.
	reorder: HashMap<u8, (u8, Bytes)>,

	// ── acknowledgement pacing (§56.2.7.4) ──
	/// Packets received since we last sent an acknowledgement.
	cumulative_acks: u8,
	/// When an acknowledgement must go out even if nothing else is sent.
	sent_ack_deadline: Option<Instant>,

	// ── handshake ──
	/// Our `SYN` (accessory) or `SYN+ACK` (device), kept for idempotent retransmits.
	handshake_packet: Option<LinkPacket>,
	handshake_sent_at: Option<Instant>,
	/// The peer `SYN` sequence number our cached `SYN+ACK` answers.
	answered_syn_psn: Option<u8>,
	/// Next time to emit the detection sequence (accessory role).
	detection_deadline: Option<Instant>,

	outbox: VecDeque<Frame>,
}

impl LinkState {
	/// A fresh link. `proposal` is what we advertise in a `SYN`; in [`Role::Device`] we
	/// echo the accessory's payload instead, so it is only used for its session table.
	pub fn new(role: Role, proposal: LinkParams) -> Self {
		// §56.2.1.4: both sides pick their initial sequence number at random.
		let initial = random_psn();
		Self {
			role,
			phase: Phase::Detecting,
			params: LinkParams::synchronizing_defaults(),
			proposal,
			next_sent_psn: initial,
			initial_sent_psn: initial,
			unacked: VecDeque::new(),
			pending: VecDeque::new(),
			initial_received_psn: None,
			last_received_in_sequence_psn: None,
			received_out_of_sequence: BTreeSet::new(),
			reorder: HashMap::new(),
			cumulative_acks: 0,
			sent_ack_deadline: None,
			handshake_packet: None,
			handshake_sent_at: None,
			answered_syn_psn: None,
			detection_deadline: None,
			outbox: VecDeque::new(),
		}
	}

	pub fn role(&self) -> Role {
		self.role
	}

	/// Payload-bearing packets we have sent since the link came up. Useful as a health
	/// signal: it should track the number the peer has acknowledged.
	pub fn packets_sent(&self) -> u8 {
		self.next_sent_psn.wrapping_sub(self.initial_sent_psn)
	}

	pub fn established(&self) -> bool {
		self.phase == Phase::Established
	}

	pub fn dead(&self) -> bool {
		self.phase == Phase::Dead
	}

	/// The parameters in force — the synchronizing defaults until the handshake lands.
	pub fn params(&self) -> &LinkParams {
		&self.params
	}

	/// The negotiated control-session identifier, once known.
	pub fn control_session(&self) -> Option<u8> {
		self.params.control_session()
	}

	/// Start the link. An accessory begins detection; a device waits to be found.
	pub fn start(&mut self, now: Instant) {
		if self.role == Role::Accessory {
			self.outbox.push_back(Frame::Detection);
			self.detection_deadline = Some(now + DETECTION_INTERVAL);
		}
	}

	/// Queue a session payload. Fails once the backlog behind a full window is too deep.
	pub fn queue(&mut self, session: u8, payload: Bytes) -> Result<(), Error> {
		if self.phase == Phase::Dead {
			return Err(Error::Reset);
		}
		if self.pending.len() >= MAX_PENDING {
			return Err(Error::Timeout("send queue"));
		}
		self.pending.push_back((session, payload));
		Ok(())
	}

	/// Whether a payload queued now would go straight out rather than waiting for the
	/// send window to open.
	pub fn can_send(&self) -> bool {
		self.phase == Phase::Established
			&& self.pending.is_empty()
			&& self.unacked.len() < usize::from(self.params.max_outstanding)
	}

	/// Whether [`Self::queue`] would accept another payload.
	///
	/// Deliberately independent of the send window: a caller that awaits a `Sink` must not
	/// be made to wait on an acknowledgement it can only receive by polling the `Stream`.
	/// The window is applied when the packet actually goes out.
	pub fn has_queue_room(&self) -> bool {
		self.phase != Phase::Dead && self.pending.len() < MAX_PENDING
	}

	/// Handle one inbound frame.
	pub fn on_frame(&mut self, frame: Frame, now: Instant) -> Vec<LinkEvent> {
		match frame {
			Frame::Detection => {
				// A device echoes the sequence back verbatim (§56.2.7.2); an accessory
				// takes the echo as proof the device speaks iAP2 and sends its SYN.
				match self.role {
					Role::Device => self.outbox.push_back(Frame::Detection),
					Role::Accessory => {
						self.detection_deadline = None;
						if self.phase == Phase::Detecting {
							self.send_syn(now);
						}
					}
				}
				vec![LinkEvent::Detected]
			}
			Frame::Iap1Incompatible => {
				warn!("accessory decided we only speak iAP1; detection echo went astray");
				vec![LinkEvent::Iap1Detected]
			}
			Frame::Packet(pkt) => self.on_packet(pkt, now),
		}
	}

	/// Handle expiry of whatever [`Self::next_deadline`] reported.
	pub fn on_timeout(&mut self, now: Instant) -> Vec<LinkEvent> {
		let mut events = Vec::new();

		if let Some(at) = self.detection_deadline
			&& now >= at
		{
			self.outbox.push_back(Frame::Detection);
			self.detection_deadline = Some(now + DETECTION_INTERVAL);
		}

		// An unanswered SYN is resent with the same payload and sequence number
		// (§56.2.7.3). Only the initiating side does this.
		if self.phase == Phase::Syncing
			&& self.role == Role::Accessory
			&& let Some(sent_at) = self.handshake_sent_at
			&& now.duration_since(sent_at) >= SYN_INTERVAL
			&& let Some(syn) = self.handshake_packet.clone()
		{
			trace!("resending SYN seq={}", syn.seq);
			self.handshake_sent_at = Some(now);
			self.outbox.push_back(Frame::Packet(syn));
		}

		// A zero RetransmitTimeout outside a full ZeroACK configuration is out of range
		// (§56.2.2.4). Honouring it literally would retransmit without pause, so treat it
		// as "no retransmissions" instead.
		if !self.params.zero_ack() && !self.params.retransmit_timeout.is_zero() {
			let rto = self.params.retransmit_timeout;
			let limit = self.params.max_retransmit;
			let mut resend = Vec::new();
			for entry in &mut self.unacked {
				if now.duration_since(entry.sent_at) < rto {
					continue;
				}
				if entry.retries >= limit {
					warn!(
						"packet seq={} unacknowledged after {limit} retransmissions",
						entry.packet.seq
					);
					self.phase = Phase::Dead;
					events.push(LinkEvent::Reset);
					return events;
				}
				entry.retries += 1;
				entry.sent_at = now;
				resend.push(entry.packet.clone());
			}
			for pkt in resend {
				trace!("retransmitting seq={}", pkt.seq);
				self.outbox.push_back(Frame::Packet(pkt));
			}
		}

		if let Some(at) = self.sent_ack_deadline
			&& now >= at
		{
			self.emit_ack();
		}

		events
	}

	/// When [`Self::on_timeout`] next needs to run.
	pub fn next_deadline(&self) -> Option<Instant> {
		let mut next: Option<Instant> = None;
		let mut merge = |t: Option<Instant>| {
			if let Some(t) = t {
				next = Some(next.map_or(t, |n: Instant| n.min(t)));
			}
		};
		merge(self.detection_deadline);
		merge(self.sent_ack_deadline);
		if self.phase == Phase::Syncing && self.role == Role::Accessory {
			merge(self.handshake_sent_at.map(|t| t + SYN_INTERVAL));
		}
		if !self.params.zero_ack() && !self.params.retransmit_timeout.is_zero() {
			let rto = self.params.retransmit_timeout;
			for entry in &self.unacked {
				merge(Some(entry.sent_at + rto));
			}
		}
		next
	}

	/// Take the next frame to write, filling the send window from the queue first.
	pub fn poll_outgoing(&mut self, now: Instant) -> Option<Frame> {
		if let Some(frame) = self.outbox.pop_front() {
			return Some(frame);
		}
		if self.phase != Phase::Established {
			return None;
		}
		// §56.2.7.6: send until the peer's MaximumNumberOfOutstandingPackets is reached.
		if self.unacked.len() >= usize::from(self.params.max_outstanding) {
			return None;
		}
		let (session, payload) = self.pending.pop_front()?;
		let pkt = LinkPacket::new(
			Control::ACK,
			self.take_psn(),
			self.ack_num(),
			session,
			payload,
		);
		// Sending anything with the ACK bit discharges the pending acknowledgement.
		self.clear_ack_debt();
		if !self.params.zero_ack() {
			self.unacked.push_back(Unacked {
				packet: pkt.clone(),
				sent_at: now,
				retries: 0,
			});
		}
		Some(Frame::Packet(pkt))
	}

	/// Ask the peer to restart synchronization (§56.2.5; device → accessory only).
	pub fn reset(&mut self) {
		if self.role == Role::Device {
			self.outbox
				.push_back(Frame::Packet(LinkPacket::control_only(
					Control::RST,
					self.next_sent_psn,
					0,
				)));
		}
		self.phase = Phase::Dead;
	}

	/// Tell the accessory we are about to sleep (§56.2.6; device → accessory only).
	pub fn sleep(&mut self) {
		if self.role == Role::Device {
			self.outbox
				.push_back(Frame::Packet(LinkPacket::control_only(
					Control::SLP,
					self.next_sent_psn,
					0,
				)));
		}
	}

	// ────────────────────────────── inbound packets ──────────────────────────────

	fn on_packet(&mut self, pkt: LinkPacket, now: Instant) -> Vec<LinkEvent> {
		if pkt.control.contains(Control::RST) {
			// Only a device may send RST. As the device we should never see one, but an
			// accessory must re-synchronize within 1 s (§56.2.7.7).
			debug!("received RST");
			self.phase = Phase::Dead;
			if self.role == Role::Accessory {
				self.restart_sync(now);
			}
			return vec![LinkEvent::Reset];
		}
		if pkt.control.contains(Control::SLP) {
			return vec![LinkEvent::Sleep];
		}
		if pkt.control.contains(Control::SYN) {
			return self.on_syn(pkt, now);
		}

		let mut events = Vec::new();
		if pkt.control.contains(Control::ACK) {
			self.retire_acked(pkt.ack);
			// The peer's acknowledgement of our SYN+ACK is what establishes the link.
			if self.phase == Phase::Syncing
				&& let Some(handshake) = &self.handshake_packet
				&& pkt.ack == handshake.seq
			{
				self.phase = Phase::Established;
				debug!("iAP2 link established: {:?}", self.params);
				events.push(LinkEvent::Established(self.params.clone()));
			}
		}

		if pkt.control.contains(Control::EAK) {
			// Out-of-sequence sequence numbers the peer has buffered. These are
			// explicitly *not* acknowledgements (§56.2.4), so the retransmit timers for
			// those packets keep running.
			trace!(
				"peer holds out-of-sequence packets {:02x?}",
				pkt.payload.as_ref()
			);
			return events;
		}

		if pkt.has_payload() {
			self.accept_payload(pkt, now, &mut events);
		}
		events
	}

	fn on_syn(&mut self, pkt: LinkPacket, now: Instant) -> Vec<LinkEvent> {
		let mut events = Vec::new();

		// A device that could not answer immediately may see the same SYN again; answer
		// it with the identical cached packet so no sequence number drifts (§56.2.7.3).
		if self.answered_syn_psn == Some(pkt.seq)
			&& let Some(cached) = self.handshake_packet.clone()
		{
			trace!("duplicate SYN seq={}, resending cached SYN+ACK", pkt.seq);
			self.outbox.push_back(Frame::Packet(cached));
			return events;
		}

		let peer = match LinkParams::decode(&pkt.payload) {
			Ok(p) => p,
			Err(err) => {
				warn!("unusable Link Synchronization Payload: {err}");
				return events;
			}
		};
		if let Err(err) = peer.validate() {
			// We still adopt it — refusing would just stall the link — but an out-of-range
			// parameter is worth knowing about when something later behaves oddly.
			warn!("accessory proposed out-of-range link parameters ({err}): {peer:?}");
		}

		match self.role {
			Role::Device => {
				// A SYN restarts synchronization even from Established (§56.2.7.3), so
				// wipe the receive bookkeeping before adopting the new parameters.
				self.reset_sequencing();
				self.params = peer;
				// A SYN carries Link Synchronization Payload data, so it advances the
				// received sequence (§56.2.1.4) and the accessory's first data packet
				// will be numbered one higher.
				self.observe_psn(pkt.seq);
				self.last_received_in_sequence_psn = Some(pkt.seq);
				// Echoing the payload guarantees "identical values for all negotiable
				// connection parameters", which ends negotiation in one exchange.
				let synack = LinkPacket::new(
					Control::SYN | Control::ACK,
					self.take_psn(),
					pkt.seq,
					0,
					pkt.payload.clone(),
				);
				self.handshake_packet = Some(synack.clone());
				self.handshake_sent_at = Some(now);
				self.answered_syn_psn = Some(pkt.seq);
				self.phase = Phase::Syncing;
				self.outbox.push_back(Frame::Packet(synack));
			}
			Role::Accessory => {
				// SYN+ACK answering our SYN: if the parameters came back unchanged the
				// link is up as soon as we send the final bare ACK. It carries payload
				// data, so it advances the received sequence too.
				self.observe_psn(pkt.seq);
				self.last_received_in_sequence_psn = Some(pkt.seq);
				let ours = self.handshake_packet.as_ref().map(|p| p.payload.clone());
				if ours.as_deref() == Some(pkt.payload.as_ref()) {
					self.params = peer;
					self.phase = Phase::Established;
					self.outbox
						.push_back(Frame::Packet(LinkPacket::control_only(
							Control::ACK,
							self.next_sent_psn,
							pkt.seq,
						)));
					debug!("iAP2 link established: {:?}", self.params);
					events.push(LinkEvent::Established(self.params.clone()));
				} else {
					debug!("device countered our link parameters; adopting {peer:?}");
					self.proposal = peer;
					self.send_syn(now);
				}
			}
		}
		events
	}

	/// Apply §56.2.7.6's acceptance window to a payload-bearing packet.
	fn accept_payload(&mut self, pkt: LinkPacket, now: Instant, events: &mut Vec<LinkEvent>) {
		let expected = self.next_expected_psn();
		let offset = pkt.seq.wrapping_sub(expected);
		let window = self.params.max_outstanding;

		if offset >= window {
			// Either a duplicate of something already delivered — re-acknowledge it,
			// because our previous ACK evidently went missing (§56.2.7.4) — or so far out
			// of the window that the link is unstable and it must be discarded.
			if expected.wrapping_sub(pkt.seq) <= window {
				trace!("duplicate payload seq={}, re-acknowledging", pkt.seq);
				self.cumulative_acks = self.cumulative_acks.saturating_add(1);
				self.emit_ack();
			} else {
				warn!(
					"payload seq={} outside the acceptance window (expected {expected})",
					pkt.seq
				);
			}
			return;
		}

		self.observe_psn(pkt.seq);
		self.cumulative_acks = self.cumulative_acks.saturating_add(1);

		if offset == 0 {
			self.last_received_in_sequence_psn = Some(pkt.seq);
			events.push(LinkEvent::Payload {
				session: pkt.session,
				data: pkt.payload,
			});
			// Anything buffered behind it can now be delivered in order.
			loop {
				let next = self.next_expected_psn();
				let Some((session, data)) = self.reorder.remove(&next) else {
					break;
				};
				self.received_out_of_sequence.remove(&next);
				self.last_received_in_sequence_psn = Some(next);
				events.push(LinkEvent::Payload { session, data });
			}
			self.schedule_ack(now);
		} else {
			// Inside the window but out of order: hold it, and acknowledge with an EAK
			// immediately (§56.2.7.4 requires this without delay).
			trace!(
				"out-of-sequence payload seq={} (expected {expected})",
				pkt.seq
			);
			self.received_out_of_sequence.insert(pkt.seq);
			self.reorder.insert(pkt.seq, (pkt.session, pkt.payload));
			self.emit_eak();
		}
	}

	// ────────────────────────────── outbound helpers ─────────────────────────────

	fn send_syn(&mut self, now: Instant) {
		let syn = LinkPacket::new(Control::SYN, self.take_psn(), 0, 0, self.proposal.encode());
		self.handshake_packet = Some(syn.clone());
		self.handshake_sent_at = Some(now);
		self.phase = Phase::Syncing;
		self.outbox.push_back(Frame::Packet(syn));
	}

	fn restart_sync(&mut self, now: Instant) {
		self.reset_sequencing();
		self.params = LinkParams::synchronizing_defaults();
		self.phase = Phase::Detecting;
		if self.role == Role::Accessory {
			self.send_syn(now);
		}
	}

	fn reset_sequencing(&mut self) {
		self.unacked.clear();
		self.reorder.clear();
		self.received_out_of_sequence.clear();
		self.initial_received_psn = None;
		self.last_received_in_sequence_psn = None;
		self.handshake_packet = None;
		self.handshake_sent_at = None;
		self.answered_syn_psn = None;
		self.clear_ack_debt();
	}

	/// A bare cumulative acknowledgement.
	fn emit_ack(&mut self) {
		if self.params.zero_ack() {
			self.clear_ack_debt();
			return;
		}
		self.outbox
			.push_back(Frame::Packet(LinkPacket::control_only(
				Control::ACK,
				self.next_sent_psn,
				self.ack_num(),
			)));
		self.clear_ack_debt();
	}

	/// An extended acknowledgement listing what arrived out of sequence (§56.2.4).
	fn emit_eak(&mut self) {
		if self.params.zero_ack() {
			self.clear_ack_debt();
			return;
		}
		let payload = Bytes::from_iter(self.received_out_of_sequence.iter().copied());
		self.outbox.push_back(Frame::Packet(LinkPacket::new(
			Control::ACK | Control::EAK,
			self.next_sent_psn,
			self.ack_num(),
			0,
			payload,
		)));
		self.clear_ack_debt();
	}

	/// Note that an acknowledgement is owed, and when it must go out (§56.2.7.4).
	fn schedule_ack(&mut self, now: Instant) {
		if self.params.zero_ack() {
			self.clear_ack_debt();
			return;
		}
		if self.cumulative_acks > self.params.max_cumulative_ack {
			self.emit_ack();
			return;
		}
		let timeout = self.params.cumulative_ack_timeout;
		if timeout.is_zero() {
			self.emit_ack();
		} else if self.sent_ack_deadline.is_none() {
			self.sent_ack_deadline = Some(now + timeout);
		}
	}

	fn clear_ack_debt(&mut self) {
		self.cumulative_acks = 0;
		self.sent_ack_deadline = None;
	}

	/// Drop everything the peer's cumulative acknowledgement covers.
	fn retire_acked(&mut self, ack: u8) {
		while let Some(front) = self.unacked.front() {
			// front.psn <= ack < next_sent_psn, in circular arithmetic.
			let span = self.next_sent_psn.wrapping_sub(front.packet.seq);
			if ack.wrapping_sub(front.packet.seq) < span {
				self.unacked.pop_front();
			} else {
				break;
			}
		}
	}

	fn take_psn(&mut self) -> u8 {
		// §56.2.1.4: the sequence number advances for every packet carrying session or
		// link-synchronization payload data, and only those.
		let psn = self.next_sent_psn;
		self.next_sent_psn = self.next_sent_psn.wrapping_add(1);
		psn
	}

	fn observe_psn(&mut self, psn: u8) {
		if self.initial_received_psn.is_none() {
			self.initial_received_psn = Some(psn);
			// The first packet we see defines the sequence, so the next one expected is
			// the one after it.
			self.last_received_in_sequence_psn = Some(psn.wrapping_sub(1));
		}
	}

	fn next_expected_psn(&self) -> u8 {
		self.last_received_in_sequence_psn
			.map_or(0, |p| p.wrapping_add(1))
	}

	/// The last packet received correctly and in sequence (§56.2.1.5).
	fn ack_num(&self) -> u8 {
		self.last_received_in_sequence_psn.unwrap_or(0)
	}
}

/// A random initial packet sequence number (§56.2.1.4), without pulling in a PRNG crate.
fn random_psn() -> u8 {
	use std::{
		collections::hash_map::RandomState,
		hash::{BuildHasher, Hasher},
	};
	RandomState::new().build_hasher().finish() as u8
}

/// A framed iAP2 link over a transport of [`Frame`]s.
pub struct Link<T> {
	inner: T,
	state: LinkState,
	timer: Pin<Box<Sleep>>,
	timer_deadline: Option<Instant>,
	ready: VecDeque<LinkEvent>,
	/// A frame taken from the state machine that the sink has not accepted yet.
	stalled: Option<Frame>,
	/// A `poll_ready` that parked because the send queue was full.
	send_waker: Option<Waker>,
	closed: bool,
}

impl<T> Link<T>
where
	T: Stream<Item = Result<Frame, Error>> + Sink<Frame, Error = Error> + Unpin,
{
	/// Wrap a transport. Call this as soon as the transport is up: an accessory starts
	/// emitting detection immediately, a device waits for the accessory's.
	pub fn new(inner: T, role: Role, proposal: LinkParams) -> Self {
		let mut state = LinkState::new(role, proposal);
		state.start(Instant::now());
		Self {
			inner,
			state,
			timer: Box::pin(sleep_until(tokio::time::Instant::now())),
			timer_deadline: None,
			ready: VecDeque::new(),
			stalled: None,
			send_waker: None,
			closed: false,
		}
	}

	pub fn established(&self) -> bool {
		self.state.established()
	}

	pub fn params(&self) -> &LinkParams {
		self.state.params()
	}

	pub fn control_session(&self) -> Option<u8> {
		self.state.control_session()
	}

	pub fn state(&self) -> &LinkState {
		&self.state
	}

	fn arm_timer(&mut self) {
		let deadline = self.state.next_deadline();
		if deadline == self.timer_deadline {
			return;
		}
		self.timer_deadline = deadline;
		if let Some(at) = deadline {
			self.timer.as_mut().reset(at.into());
		}
	}

	/// Push everything the state machine wants to send into the transport.
	fn pump_writes(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		loop {
			let frame = match self.stalled.take() {
				Some(frame) => frame,
				None => match self.state.poll_outgoing(Instant::now()) {
					Some(frame) => frame,
					None => return Poll::Ready(Ok(())),
				},
			};
			match Pin::new(&mut self.inner).poll_ready(cx) {
				Poll::Ready(Ok(())) => {}
				Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
				Poll::Pending => {
					self.stalled = Some(frame);
					return Poll::Pending;
				}
			}
			trace!("tx {frame:?}");
			Pin::new(&mut self.inner).start_send(frame)?;
			// Flush eagerly: link packets are small, latency-sensitive, and the
			// accessory's retransmit timer is already running.
			let _ = Pin::new(&mut self.inner).poll_flush(cx)?;
			// Room may have opened up for a parked send.
			if let Some(waker) = self.send_waker.take() {
				waker.wake();
			}
		}
	}
}

impl<T> Stream for Link<T>
where
	T: Stream<Item = Result<Frame, Error>> + Sink<Frame, Error = Error> + Unpin,
{
	type Item = Result<LinkEvent, Error>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		loop {
			if let Some(event) = this.ready.pop_front() {
				return Poll::Ready(Some(Ok(event)));
			}
			if this.closed {
				return Poll::Ready(None);
			}

			// Writing must make progress even when the caller only reads.
			if let Poll::Ready(Err(err)) = this.pump_writes(cx) {
				return Poll::Ready(Some(Err(err)));
			}

			this.arm_timer();
			if this.timer_deadline.is_some() && this.timer.as_mut().poll(cx).is_ready() {
				this.timer_deadline = None;
				this.ready.extend(this.state.on_timeout(Instant::now()));
				continue;
			}

			match ready!(Pin::new(&mut this.inner).poll_next(cx)) {
				Some(Ok(frame)) => {
					trace!("rx {frame:?}");
					this.ready
						.extend(this.state.on_frame(frame, Instant::now()));
				}
				Some(Err(err)) => return Poll::Ready(Some(Err(err))),
				None => {
					this.closed = true;
					return Poll::Ready(None);
				}
			}
		}
	}
}

impl<T> Sink<(u8, Bytes)> for Link<T>
where
	T: Stream<Item = Result<Frame, Error>> + Sink<Frame, Error = Error> + Unpin,
{
	type Error = Error;

	fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		let this = self.get_mut();
		if this.state.dead() {
			return Poll::Ready(Err(Error::Reset));
		}
		// Drain what is already queued before accepting more.
		let _ = this.pump_writes(cx)?;
		if this.state.has_queue_room() {
			return Poll::Ready(Ok(()));
		}
		// The queue only drains when the send window opens, which needs an inbound
		// acknowledgement — so the caller must be polling the stream too. Remember the
		// waker so pumping wakes us rather than leaving the send parked.
		this.send_waker = Some(cx.waker().clone());
		Poll::Pending
	}

	fn start_send(self: Pin<&mut Self>, (session, payload): (u8, Bytes)) -> Result<(), Error> {
		self.get_mut().state.queue(session, payload)
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		let this = self.get_mut();
		ready!(this.pump_writes(cx))?;
		Pin::new(&mut this.inner).poll_flush(cx)
	}

	fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		let this = self.get_mut();
		let _ = this.pump_writes(cx)?;
		Pin::new(&mut this.inner).poll_close(cx)
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use bytes::Bytes;
	use futures::{SinkExt, StreamExt};
	use tokio_util::codec::Framed;

	use super::*;
	use crate::iap2::{
		packet::LinkCodec,
		sync::{SessionInfo, SessionType},
	};

	const CONTROL: u8 = 0x0A;

	fn params(outstanding: u8, rto_ms: u64, cat_ms: u64, retx: u8, cum: u8) -> LinkParams {
		LinkParams {
			link_version: 1,
			max_outstanding: outstanding,
			max_recv_len: 4096,
			retransmit_timeout: Duration::from_millis(rto_ms),
			cumulative_ack_timeout: Duration::from_millis(cat_ms),
			max_retransmit: retx,
			max_cumulative_ack: cum,
			sessions: vec![SessionInfo {
				id: CONTROL,
				typ: SessionType::Control,
				version: 2,
			}],
		}
	}

	/// Immediate acknowledgement, one packet in flight: the simplest configuration.
	fn lockstep() -> LinkParams {
		params(1, 2000, 0, 3, 0)
	}

	fn drain(state: &mut LinkState, now: Instant) -> Vec<LinkPacket> {
		let mut out = Vec::new();
		while let Some(frame) = state.poll_outgoing(now) {
			if let Frame::Packet(p) = frame {
				out.push(p);
			}
		}
		out
	}

	fn drain_frames(state: &mut LinkState, now: Instant) -> Vec<Frame> {
		let mut out = Vec::new();
		while let Some(frame) = state.poll_outgoing(now) {
			out.push(frame);
		}
		out
	}

	fn data(seq: u8, ack: u8, payload: &[u8]) -> Frame {
		Frame::Packet(LinkPacket::new(
			Control::ACK,
			seq,
			ack,
			CONTROL,
			Bytes::copy_from_slice(payload),
		))
	}

	/// Bring a device-role link up against an accessory whose SYN has sequence `syn_seq`.
	/// Returns the state and the sequence number of our SYN+ACK.
	fn established_device(syn_seq: u8, p: LinkParams) -> (LinkState, u8, Instant) {
		let now = Instant::now();
		let mut state = LinkState::new(Role::Device, LinkParams::usb_recommended());
		state.start(now);

		let syn = Frame::Packet(LinkPacket::new(Control::SYN, syn_seq, 0, 0, p.encode()));
		assert!(state.on_frame(syn, now).is_empty());
		let synack = drain(&mut state, now);
		assert_eq!(synack.len(), 1);
		let synack_seq = synack[0].seq;

		let events = state.on_frame(
			Frame::Packet(LinkPacket::control_only(
				Control::ACK,
				syn_seq.wrapping_add(1),
				synack_seq,
			)),
			now,
		);
		assert!(matches!(events.as_slice(), [LinkEvent::Established(_)]));
		assert!(state.established());
		(state, synack_seq, now)
	}

	#[test]
	fn device_handshake_echoes_detection_and_answers_syn() {
		let now = Instant::now();
		let mut state = LinkState::new(Role::Device, LinkParams::usb_recommended());
		state.start(now);
		// A device never initiates; it has nothing to say until spoken to.
		assert!(drain_frames(&mut state, now).is_empty());

		assert_eq!(
			state.on_frame(Frame::Detection, now),
			vec![LinkEvent::Detected]
		);
		assert_eq!(drain_frames(&mut state, now), vec![Frame::Detection]);

		let offered = lockstep();
		let syn = Frame::Packet(LinkPacket::new(Control::SYN, 200, 0, 0, offered.encode()));
		assert!(state.on_frame(syn, now).is_empty());
		let synack = &drain(&mut state, now)[0];
		assert!(synack.control.contains(Control::SYN));
		assert!(synack.control.contains(Control::ACK));
		// Acknowledges the SYN, and echoes the payload so negotiation ends at once.
		assert_eq!(synack.ack, 200);
		assert_eq!(synack.payload, offered.encode());
		assert_eq!(state.control_session(), Some(CONTROL));
		assert!(!state.established());

		// Only an ACK naming our SYN+ACK's sequence establishes the link.
		assert!(
			state
				.on_frame(
					Frame::Packet(LinkPacket::control_only(Control::ACK, 201, 0)),
					now
				)
				.is_empty()
		);
		assert!(!state.established());
		let events = state.on_frame(
			Frame::Packet(LinkPacket::control_only(Control::ACK, 201, synack.seq)),
			now,
		);
		assert!(matches!(events.as_slice(), [LinkEvent::Established(_)]));
	}

	#[test]
	fn accessory_handshake_sends_detection_then_syn() {
		let now = Instant::now();
		let proposal = lockstep();
		let mut state = LinkState::new(Role::Accessory, proposal.clone());
		state.start(now);
		assert_eq!(drain_frames(&mut state, now), vec![Frame::Detection]);

		// Detection is resent at 1 Hz until the device echoes it (§56.2.7.2).
		assert!(drain_frames(&mut state, now).is_empty());
		state.on_timeout(now + Duration::from_millis(1100));
		assert_eq!(
			drain_frames(&mut state, now + Duration::from_millis(1100)),
			vec![Frame::Detection]
		);

		state.on_frame(Frame::Detection, now);
		let syn = &drain(&mut state, now)[0];
		assert_eq!(syn.control, Control::SYN);
		assert_eq!(syn.payload, proposal.encode());
		let syn_seq = syn.seq;

		// The device echoes our payload back, so the parameters match and we finish.
		let events = state.on_frame(
			Frame::Packet(LinkPacket::new(
				Control::SYN | Control::ACK,
				77,
				syn_seq,
				0,
				proposal.encode(),
			)),
			now,
		);
		assert!(matches!(events.as_slice(), [LinkEvent::Established(_)]));
		let final_ack = &drain(&mut state, now)[0];
		assert_eq!(final_ack.control, Control::ACK);
		assert_eq!(final_ack.ack, 77);
		assert!(!final_ack.has_payload());
	}

	#[test]
	fn unanswered_syn_is_resent_with_the_same_sequence() {
		let now = Instant::now();
		let mut state = LinkState::new(Role::Accessory, lockstep());
		state.start(now);
		drain_frames(&mut state, now);
		state.on_frame(Frame::Detection, now);
		let first = drain(&mut state, now)[0].clone();

		let later = now + Duration::from_millis(1100);
		state.on_timeout(later);
		let again = drain(&mut state, later);
		assert_eq!(again.len(), 1);
		assert_eq!(again[0], first);
	}

	#[test]
	fn duplicate_syn_resends_the_cached_synack() {
		let now = Instant::now();
		let mut state = LinkState::new(Role::Device, LinkParams::usb_recommended());
		let syn = Frame::Packet(LinkPacket::new(
			Control::SYN,
			200,
			0,
			0,
			lockstep().encode(),
		));
		state.on_frame(syn.clone(), now);
		let first = drain(&mut state, now);
		state.on_frame(syn, now);
		let again = drain(&mut state, now);
		// Byte-identical, so our own sequence number does not drift.
		assert_eq!(first, again);
	}

	#[test]
	fn delivers_and_acknowledges_a_payload() {
		let (mut state, _, now) = established_device(200, lockstep());
		let events = state.on_frame(data(201, 0, &[1, 2, 3]), now);
		assert_eq!(
			events,
			vec![LinkEvent::Payload {
				session: CONTROL,
				data: Bytes::from_static(&[1, 2, 3])
			}]
		);
		let ack = &drain(&mut state, now)[0];
		assert_eq!(ack.control, Control::ACK);
		assert_eq!(ack.ack, 201);
		assert!(!ack.has_payload());
	}

	#[test]
	fn cumulative_acknowledgement_coalesces_then_fires_on_the_timer() {
		// Up to 3 accumulated acknowledgements, or 22 ms, whichever comes first.
		let (mut state, _, now) = established_device(200, params(5, 2000, 22, 3, 3));

		for (i, seq) in (201..=203).enumerate() {
			state.on_frame(data(seq, 0, &[i as u8]), now);
			assert!(
				drain(&mut state, now).is_empty(),
				"acked too eagerly at {seq}"
			);
		}
		// The fourth exceeds MaximumCumulativeAcknowledgements and must go out at once.
		state.on_frame(data(204, 0, &[4]), now);
		assert_eq!(drain(&mut state, now)[0].ack, 204);

		// And the timer alone is enough when the count is not reached.
		state.on_frame(data(205, 0, &[5]), now);
		assert!(drain(&mut state, now).is_empty());
		let later = now + Duration::from_millis(25);
		state.on_timeout(later);
		assert_eq!(drain(&mut state, later)[0].ack, 205);
	}

	#[test]
	fn out_of_sequence_payload_gets_an_eak_and_is_delivered_in_order() {
		let (mut state, _, now) = established_device(200, params(5, 2000, 22, 3, 3));

		// 202 arrives before 201: hold it back and acknowledge with an EAK.
		assert!(state.on_frame(data(202, 0, b"second"), now).is_empty());
		let eak = &drain(&mut state, now)[0];
		assert!(eak.control.contains(Control::EAK));
		assert!(eak.control.contains(Control::ACK));
		assert_eq!(eak.ack, 200, "EAK still names the last in-sequence packet");
		assert_eq!(eak.payload.as_ref(), &[202]);

		// 201 fills the gap, so both come out, in order.
		let events = state.on_frame(data(201, 0, b"first"), now);
		assert_eq!(
			events,
			vec![
				LinkEvent::Payload {
					session: CONTROL,
					data: Bytes::from_static(b"first")
				},
				LinkEvent::Payload {
					session: CONTROL,
					data: Bytes::from_static(b"second")
				},
			]
		);
	}

	#[test]
	fn payload_outside_the_window_is_discarded() {
		let (mut state, _, now) = established_device(200, params(2, 2000, 0, 3, 0));
		// Expected 201, window covers 201..=202. 240 is neither that nor a duplicate.
		assert!(state.on_frame(data(240, 0, b"way ahead"), now).is_empty());
		assert!(drain(&mut state, now).is_empty());
	}

	#[test]
	fn duplicate_payload_is_reacknowledged_but_not_redelivered() {
		let (mut state, _, now) = established_device(200, lockstep());
		assert_eq!(state.on_frame(data(201, 0, &[1]), now).len(), 1);
		drain(&mut state, now);

		let events = state.on_frame(data(201, 0, &[1]), now);
		assert!(events.is_empty());
		// §56.2.7.4: resend the acknowledgement, since ours evidently went missing.
		assert_eq!(drain(&mut state, now)[0].ack, 201);
	}

	#[test]
	fn a_b_a_retransmission_is_not_redelivered() {
		// The old single-slot duplicate check let this through.
		let (mut state, _, now) = established_device(200, params(5, 2000, 0, 3, 0));
		assert_eq!(state.on_frame(data(201, 0, b"A"), now).len(), 1);
		assert_eq!(state.on_frame(data(202, 0, b"B"), now).len(), 1);
		drain(&mut state, now);
		assert!(state.on_frame(data(201, 0, b"A"), now).is_empty());
	}

	#[test]
	fn retransmits_unacknowledged_packets_then_resets() {
		let (mut state, _, now) = established_device(200, params(1, 100, 0, 2, 0));
		state.queue(CONTROL, Bytes::from_static(b"hello")).unwrap();
		let first = drain(&mut state, now);
		assert_eq!(first.len(), 1);
		let seq = first[0].seq;

		// Each retransmission keeps the original sequence number (§56.2.1.4).
		let t1 = now + Duration::from_millis(150);
		assert!(state.on_timeout(t1).is_empty());
		assert_eq!(drain(&mut state, t1)[0].seq, seq);

		let t2 = t1 + Duration::from_millis(150);
		assert!(state.on_timeout(t2).is_empty());
		assert_eq!(drain(&mut state, t2)[0].seq, seq);

		// MaximumNumberOfRetransmissions is 2, so the third expiry breaks the link.
		let t3 = t2 + Duration::from_millis(150);
		assert_eq!(state.on_timeout(t3), vec![LinkEvent::Reset]);
		assert!(state.dead());
	}

	#[test]
	fn acknowledgement_stops_retransmission() {
		let (mut state, _, now) = established_device(200, params(1, 100, 0, 3, 0));
		state.queue(CONTROL, Bytes::from_static(b"hello")).unwrap();
		let seq = drain(&mut state, now)[0].seq;

		state.on_frame(
			Frame::Packet(LinkPacket::control_only(Control::ACK, 201, seq)),
			now,
		);
		let later = now + Duration::from_millis(500);
		assert!(state.on_timeout(later).is_empty());
		assert!(drain(&mut state, later).is_empty());
		assert!(state.can_send());
	}

	#[test]
	fn send_window_blocks_at_max_outstanding() {
		let (mut state, _, now) = established_device(200, params(2, 2000, 0, 3, 0));
		for i in 0..3u8 {
			state.queue(CONTROL, Bytes::copy_from_slice(&[i])).unwrap();
		}
		let sent = drain(&mut state, now);
		assert_eq!(sent.len(), 2, "MaximumNumberOfOutstandingPackets is 2");
		assert!(!state.can_send());

		// Acknowledging the first frees exactly one slot.
		state.on_frame(
			Frame::Packet(LinkPacket::control_only(Control::ACK, 201, sent[0].seq)),
			now,
		);
		let more = drain(&mut state, now);
		assert_eq!(more.len(), 1);
		assert_eq!(more[0].payload.as_ref(), &[2]);
	}

	#[test]
	fn sequence_numbers_only_advance_on_payload() {
		let (mut state, _, now) = established_device(200, params(5, 2000, 0, 3, 0));
		// Two bare ACKs, provoked by two inbound payloads, reuse the same sequence.
		state.on_frame(data(201, 0, &[1]), now);
		let a = drain(&mut state, now)[0].seq;
		state.on_frame(data(202, 0, &[2]), now);
		let b = drain(&mut state, now)[0].seq;
		assert_eq!(a, b);

		// A payload of our own does advance it.
		state.queue(CONTROL, Bytes::from_static(b"x")).unwrap();
		state.queue(CONTROL, Bytes::from_static(b"y")).unwrap();
		let sent = drain(&mut state, now);
		assert_eq!(sent[0].seq, a);
		assert_eq!(sent[1].seq, a.wrapping_add(1));
	}

	#[test]
	fn zero_ack_sends_neither_acknowledgements_nor_retransmissions() {
		let (mut state, _, now) = established_device(200, params(5, 0, 0, 0, 0));
		assert!(state.params().zero_ack());

		// An inbound payload is delivered but not acknowledged.
		assert_eq!(state.on_frame(data(201, 0, &[1]), now).len(), 1);
		assert!(drain(&mut state, now).is_empty());

		// Our own payload goes out once and is never retried.
		state.queue(CONTROL, Bytes::from_static(b"x")).unwrap();
		assert_eq!(drain(&mut state, now).len(), 1);
		let later = now + Duration::from_secs(30);
		assert!(state.on_timeout(later).is_empty());
		assert!(drain(&mut state, later).is_empty());
		assert_eq!(state.next_deadline(), None);
	}

	#[test]
	fn re_syn_restarts_sequencing() {
		let (mut state, _, now) = established_device(200, params(5, 2000, 0, 3, 0));
		state.on_frame(data(201, 0, &[1]), now);
		state.queue(CONTROL, Bytes::from_static(b"x")).unwrap();
		drain(&mut state, now);

		// A fresh SYN restarts synchronization from scratch (§56.2.7.3).
		let p = params(3, 1000, 0, 3, 0);
		state.on_frame(
			Frame::Packet(LinkPacket::new(Control::SYN, 50, 0, 0, p.encode())),
			now,
		);
		assert!(!state.established());
		let synack = drain(&mut state, now);
		assert_eq!(synack.len(), 1, "the queued payload must not ride along");
		assert_eq!(synack[0].ack, 50);
		assert!(state.unacked.is_empty());
		assert!(state.reorder.is_empty());
		assert_eq!(state.params().max_outstanding, 3);

		// The accessory's next data packet is numbered after its SYN.
		state.on_frame(
			Frame::Packet(LinkPacket::control_only(Control::ACK, 51, synack[0].seq)),
			now,
		);
		assert!(state.established());
		assert_eq!(state.on_frame(data(51, 0, &[9]), now).len(), 1);
	}

	#[test]
	fn reset_and_sleep_are_reported() {
		let (mut state, _, now) = established_device(200, lockstep());
		let mut sleeping = LinkState::new(Role::Device, LinkParams::usb_recommended());
		assert_eq!(
			sleeping.on_frame(
				Frame::Packet(LinkPacket::control_only(Control::SLP, 1, 0)),
				now
			),
			vec![LinkEvent::Sleep]
		);
		assert_eq!(
			state.on_frame(
				Frame::Packet(LinkPacket::control_only(Control::RST, 1, 0)),
				now
			),
			vec![LinkEvent::Reset]
		);
		assert!(state.dead());
		assert!(state.queue(CONTROL, Bytes::from_static(b"x")).is_err());
	}

	#[test]
	fn sequence_numbers_wrap() {
		let (mut state, _, now) = established_device(254, params(5, 2000, 0, 3, 0));
		// The accessory's SYN was 254, so 255 then 0 then 1 are all in sequence.
		for (seq, byte) in [(255u8, 1u8), (0, 2), (1, 3)] {
			let events = state.on_frame(data(seq, 0, &[byte]), now);
			assert_eq!(events.len(), 1, "seq {seq} should be in sequence");
		}
		assert_eq!(drain(&mut state, now).last().unwrap().ack, 1);
	}

	#[test]
	fn iap1_notice_is_surfaced() {
		let now = Instant::now();
		let mut state = LinkState::new(Role::Accessory, lockstep());
		assert_eq!(
			state.on_frame(Frame::Iap1Incompatible, now),
			vec![LinkEvent::Iap1Detected]
		);
	}

	/// The whole thing over a real duplex pipe: two `Link`s, opposite roles, framed with
	/// the same codec, handshaking and exchanging a payload.
	#[tokio::test]
	async fn loopback_handshake_and_exchange() {
		let (a, b) = tokio::io::duplex(4096);
		let proposal = params(5, 2000, 22, 3, 3);

		let mut accessory = Link::new(
			Framed::new(a, LinkCodec::default()),
			Role::Accessory,
			proposal.clone(),
		);
		let mut device = Link::new(
			Framed::new(b, LinkCodec::default()),
			Role::Device,
			proposal.clone(),
		);

		let run = async {
			let mut device_up = false;
			let mut accessory_up = false;
			let echoed;

			loop {
				tokio::select! {
					event = device.next() => match event.unwrap().unwrap() {
						LinkEvent::Established(p) => {
							assert_eq!(p.control_session(), Some(CONTROL));
							device_up = true;
						}
						LinkEvent::Payload { session, data } => {
							assert_eq!(session, CONTROL);
							// Bounce it back so the accessory sees a full round trip.
							device.send((CONTROL, data)).await.unwrap();
						}
						_ => {}
					},
					event = accessory.next() => match event.unwrap().unwrap() {
						LinkEvent::Established(_) => {
							accessory_up = true;
							accessory
								.send((CONTROL, Bytes::from_static(b"ping")))
								.await
								.unwrap();
						}
						LinkEvent::Payload { data, .. } => {
							echoed = data;
							break;
						}
						_ => {}
					},
				}
			}

			assert!(device_up && accessory_up);
			assert_eq!(echoed, Bytes::from_static(b"ping"));
		};

		tokio::time::timeout(Duration::from_secs(5), run)
			.await
			.expect("loopback handshake timed out");
	}
}
