//! iAP2 over HID reports, the USB Device Mode transport (spec §60.5.6).
//!
//! Link packets travel as vendor HID reports. Each report's data is `[Link Control
//! Byte][iAP bytes…]`, zero-padded to the report's fixed size; the LCB groups reports into
//! *sets*, and a set concatenates to one or more complete link packets.
//!
//! The two directions are not symmetric:
//! - **device → car** (Input reports, interrupt IN endpoint): the report *includes* its
//!   report-id byte first, so [`wrap_input`] output is ready to write.
//! - **car → device** (Output reports, EP0 `SET_REPORT`): the report-id is carried in
//!   `wValue`, so the data phase starts at the LCB.
//!
//! [`HidTransport`] presents both as a `Stream`/`Sink` of [`Frame`], the same shape the
//! bulk transport has, by running the report layer underneath [`LinkCodec`].

use std::{
	pin::Pin,
	task::{Context, Poll},
};

use bytes::BytesMut;
use futures::{Sink, Stream};
use log::warn;
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder};

use crate::iap2::{
	error::Error,
	packet::{Frame, LinkCodec},
};

/// Payload byte counts of the iPhone's Input reports (device → car), indexed by
/// `report_id - 1` (report IDs 1..=12). Each count *includes* the leading LCB byte.
pub const INPUT_REPORT_SIZES: [u16; 12] = [5, 9, 13, 17, 25, 49, 95, 193, 257, 385, 513, 767];

/// Payload byte counts of the iPhone's Output reports (car → device), indexed by
/// `report_id - 13` (report IDs 13..=21). Each count *includes* the leading LCB byte.
pub const OUTPUT_REPORT_SIZES: [u16; 9] = [5, 9, 13, 17, 25, 49, 95, 193, 255];

/// Report id of the first Output report.
pub const FIRST_OUTPUT_REPORT_ID: u8 = 13;

/// Maximum iAP bytes carried by one Input report (largest count minus the LCB).
pub const MAX_INPUT_CHUNK: usize = 766;

/// Cap on a reassembled set. A car that never clears the "more" bit cannot make us
/// allocate without bound; the largest legal link packet is 65535 bytes.
const MAX_SET_LEN: usize = 1 << 17;

// Link Control Byte bits (§60.5.6 Table 60-2).
const LCB_CONTINUATION: u8 = 0x01; // 0 = first report of a set, 1 = a continuation
const LCB_MORE: u8 = 0x02; // 1 = more reports follow in this set

/// The declared data size of an Output report, for validating an inbound `SET_REPORT`.
pub fn output_report_size(report_id: u8) -> Option<usize> {
	report_id
		.checked_sub(FIRST_OUTPUT_REPORT_ID)
		.and_then(|i| OUTPUT_REPORT_SIZES.get(usize::from(i)))
		.map(|n| usize::from(*n))
}

/// The declared data size of an Input report.
pub fn input_report_size(report_id: u8) -> Option<usize> {
	report_id
		.checked_sub(1)
		.and_then(|i| INPUT_REPORT_SIZES.get(usize::from(i)))
		.map(|n| usize::from(*n))
}

/// Smallest Input report `(id, count)` whose data can hold `need` bytes (LCB + chunk).
fn smallest_input_report(need: usize) -> (u8, usize) {
	for (i, &count) in INPUT_REPORT_SIZES.iter().enumerate() {
		if usize::from(count) >= need {
			return ((i + 1) as u8, usize::from(count));
		}
	}
	(
		INPUT_REPORT_SIZES.len() as u8,
		usize::from(INPUT_REPORT_SIZES[INPUT_REPORT_SIZES.len() - 1]),
	)
}

/// Build one Input report: `[report_id][LCB][chunk][zero pad to the report size]`.
fn make_report(lcb: u8, chunk: &[u8]) -> Vec<u8> {
	let (report_id, count) = smallest_input_report(1 + chunk.len());
	let mut r = Vec::with_capacity(1 + count);
	r.push(report_id);
	r.push(lcb);
	r.extend_from_slice(chunk);
	r.resize(1 + count, 0x00); // report-id byte + `count` data bytes
	r
}

/// Wrap iAP bytes into Input reports ready to write to the interrupt IN endpoint.
pub fn wrap_input(iap: &[u8]) -> Vec<Vec<u8>> {
	if iap.len() <= MAX_INPUT_CHUNK {
		return vec![make_report(0x00, iap)];
	}
	let chunks: Vec<&[u8]> = iap.chunks(MAX_INPUT_CHUNK).collect();
	let n = chunks.len();
	chunks
		.into_iter()
		.enumerate()
		.map(|(i, chunk)| {
			let mut lcb = 0u8;
			if i > 0 {
				lcb |= LCB_CONTINUATION;
			}
			if i + 1 < n {
				lcb |= LCB_MORE;
			}
			make_report(lcb, chunk)
		})
		.collect()
}

/// Reassembles Output reports (car → device, arriving as EP0 `SET_REPORT`s) into complete
/// report sets.
#[derive(Default)]
pub struct Reassembler {
	buf: Vec<u8>,
}

impl Reassembler {
	pub fn new() -> Self {
		Self::default()
	}

	/// Feed the data phase of one `SET_REPORT` (`[LCB][iAP bytes…]`, i.e. the report
	/// *without* its report-id byte). Returns the set once it is complete.
	pub fn push(&mut self, report_data: &[u8]) -> Option<Vec<u8>> {
		let (&lcb, data) = report_data.split_first()?;
		if lcb & LCB_CONTINUATION == 0 {
			// Start of a new set; drop any incomplete previous set (§60.5.6).
			self.buf.clear();
		}
		if self.buf.len() + data.len() > MAX_SET_LEN {
			warn!("HID report set exceeded {MAX_SET_LEN} bytes; discarding it");
			self.buf.clear();
			return None;
		}
		self.buf.extend_from_slice(data);
		if lcb & LCB_MORE == 0 {
			Some(std::mem::take(&mut self.buf))
		} else {
			None
		}
	}

	/// Discard any partially accumulated set.
	pub fn reset(&mut self) {
		self.buf.clear();
	}
}

/// A framed iAP2 transport over HID reports.
///
/// Inbound `SET_REPORT` data phases arrive on `reports`; outbound Input reports go out on
/// `input`, which a task owning the interrupt IN endpoint drains.
pub struct HidTransport {
	reports: mpsc::UnboundedReceiver<Vec<u8>>,
	input: mpsc::UnboundedSender<Vec<u8>>,
	reasm: Reassembler,
	codec: LinkCodec,
	rx: BytesMut,
	pending: BytesMut,
}

impl HidTransport {
	pub fn new(
		reports: mpsc::UnboundedReceiver<Vec<u8>>,
		input: mpsc::UnboundedSender<Vec<u8>>,
	) -> Self {
		Self {
			reports,
			input,
			reasm: Reassembler::new(),
			codec: LinkCodec::default(),
			rx: BytesMut::new(),
			pending: BytesMut::new(),
		}
	}

	/// Bound the decoder once the accessory's Link Synchronization Payload is known.
	pub fn set_max_len(&mut self, max_len: usize) {
		self.codec.set_max_len(max_len);
	}
}

impl Stream for HidTransport {
	type Item = Result<Frame, Error>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		loop {
			if let Some(frame) = this.codec.decode(&mut this.rx)? {
				return Poll::Ready(Some(Ok(frame)));
			}
			match this.reports.poll_recv(cx) {
				Poll::Ready(Some(data)) => {
					if let Some(set) = this.reasm.push(&data) {
						this.rx.extend_from_slice(&set);
					}
				}
				Poll::Ready(None) => return Poll::Ready(None),
				Poll::Pending => return Poll::Pending,
			}
		}
	}
}

impl Sink<Frame> for HidTransport {
	type Error = Error;

	fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		// The interrupt IN endpoint is drained by its own task through an unbounded
		// channel, so there is no backpressure to report here.
		Poll::Ready(Ok(()))
	}

	fn start_send(self: Pin<&mut Self>, frame: Frame) -> Result<(), Error> {
		let this = self.get_mut();
		this.pending.clear();
		this.codec.encode(frame, &mut this.pending)?;
		for report in wrap_input(&this.pending) {
			this.input.send(report).map_err(|_| Error::Closed)?;
		}
		Ok(())
	}

	fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		Poll::Ready(Ok(()))
	}

	fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		Poll::Ready(Ok(()))
	}
}

/// A collection of report ids and their sizes, for the gadget's `GET_REPORT` handler.
pub fn zeroed_input_report(report_id: u8, want: usize) -> Option<Vec<u8>> {
	input_report_size(report_id).map(|size| vec![0u8; want.min(size)])
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use futures::{SinkExt, StreamExt};

	use super::*;
	use crate::iap2::packet::{Control, LinkPacket};

	#[test]
	fn single_report_picks_smallest_fitting_id() {
		// 9 iAP bytes need LCB + 9 = 10 report bytes -> report id 3 (count 13).
		let reports = wrap_input(&[0xAB; 9]);
		assert_eq!(reports.len(), 1);
		assert_eq!(reports[0][0], 3);
		assert_eq!(reports[0][1], 0x00); // LCB: single, self-contained set
		assert_eq!(reports[0].len(), 1 + 13);
		assert_eq!(&reports[0][2..2 + 9], &[0xAB; 9]);
		assert_eq!(&reports[0][2 + 9..], &[0x00; 3]);
	}

	#[test]
	fn report_size_tables_match_the_descriptor() {
		assert_eq!(input_report_size(1), Some(5));
		assert_eq!(input_report_size(12), Some(767));
		assert_eq!(input_report_size(13), None);
		assert_eq!(output_report_size(13), Some(5));
		assert_eq!(output_report_size(21), Some(255));
		assert_eq!(output_report_size(22), None);
		assert_eq!(output_report_size(1), None);
	}

	#[test]
	fn zeroed_reports_are_truncated_to_the_request() {
		assert_eq!(zeroed_input_report(1, 64), Some(vec![0u8; 5]));
		assert_eq!(zeroed_input_report(12, 8), Some(vec![0u8; 8]));
		assert_eq!(zeroed_input_report(0, 8), None);
	}

	#[test]
	fn large_packet_splits_and_reassembles() {
		let iap: Vec<u8> = (0..2000u32).map(|x| x as u8).collect();
		let reports = wrap_input(&iap);
		assert!(reports.len() > 1);
		let mut re = Reassembler::new();
		let mut done = None;
		for r in &reports {
			if let Some(set) = re.push(&r[1..]) {
				done = Some(set);
			}
		}
		assert_eq!(
			&done.expect("set completes on the last report")[..2000],
			&iap[..]
		);
	}

	#[test]
	fn reassembly_is_bounded() {
		let mut re = Reassembler::new();
		// A car that never clears the "more" bit must not grow the buffer forever.
		let chunk = vec![LCB_MORE; 1 + MAX_INPUT_CHUNK];
		for _ in 0..(MAX_SET_LEN / MAX_INPUT_CHUNK + 2) {
			assert_eq!(re.push(&chunk), None);
		}
		assert!(re.buf.len() <= MAX_SET_LEN);
	}

	#[tokio::test]
	async fn transport_round_trips_a_packet() {
		let (report_tx, report_rx) = mpsc::unbounded_channel();
		let (input_tx, mut input_rx) = mpsc::unbounded_channel();
		let mut transport = HidTransport::new(report_rx, input_tx);

		let pkt = LinkPacket::new(
			Control::ACK,
			3,
			2,
			0x0A,
			Bytes::from_static(&[0x40, 0x40, 0x00, 0x06, 0xAA, 0x00]),
		);

		// Outbound: one link packet becomes one Input report.
		transport.send(Frame::Packet(pkt.clone())).await.unwrap();
		let report = input_rx.recv().await.unwrap();
		assert_eq!(report[0], 4); // 15 iAP bytes + LCB -> report id 4 (count 17)

		// Inbound: feed that report back minus its report-id byte, as SET_REPORT does.
		report_tx.send(report[1..].to_vec()).unwrap();
		assert_eq!(transport.next().await.unwrap().unwrap(), Frame::Packet(pkt));
	}
}
