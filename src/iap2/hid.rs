//! iAP2-over-HID report framing for USB Device Mode (spec §60.5.6).
//!
//! Link packets are carried as vendor HID reports. Each report's data is
//! `[Link Control Byte][iAP bytes…]`, zero-padded to the report's fixed size. The
//! LCB groups reports into *sets*; a set concatenates to one or more complete link
//! packets, which [`super::parse_set`] then recovers.
//!
//! Direction asymmetry (see the two entry points below):
//! - **device → car** (Input reports, interrupt IN endpoint): the report *includes*
//!   its report-id byte first — [`wrap_input`] produces ready-to-write bytes.
//! - **car → device** (Output reports, EP0 `SET_REPORT`): the report-id is carried
//!   in `wValue`, so the data phase starts at the LCB — feed that to
//!   [`Reassembler::push`].

/// Payload byte counts of the iPhone's Input reports (device → car), indexed by
/// `report_id - 1` (report IDs 1..=12). Each count *includes* the leading LCB byte.
pub const INPUT_REPORT_SIZES: [u16; 12] = [5, 9, 13, 17, 25, 49, 95, 193, 257, 385, 513, 767];

/// Maximum iAP bytes carried by one Input report (largest count minus the LCB).
pub const MAX_INPUT_CHUNK: usize = 766;

// Link Control Byte bits (§60.5.6 Table 60-2).
const LCB_CONTINUATION: u8 = 0x01; // 0 = first report of a set, 1 = a continuation
const LCB_MORE: u8 = 0x02; // 1 = more reports follow in this set

/// Smallest Input report `(id, count)` whose data can hold `need` bytes (LCB + chunk).
fn smallest_input_report(need: usize) -> (u8, usize) {
	for (i, &count) in INPUT_REPORT_SIZES.iter().enumerate() {
		if count as usize >= need {
			return ((i + 1) as u8, count as usize);
		}
	}
	(
		INPUT_REPORT_SIZES.len() as u8,
		*INPUT_REPORT_SIZES.last().unwrap() as usize,
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

/// Wrap iAP bytes (a link packet, or the detection sequence) into one or more Input
/// reports ready to write to the interrupt IN endpoint. Device handshake packets are
/// small and produce a single report (LCB `0x00`); larger data is split across
/// reports with LCB continuation bits.
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

/// Reassembles Output reports (car → device, arriving as EP0 `SET_REPORT`s) into
/// complete report sets.
#[derive(Default)]
pub struct Reassembler {
	buf: Vec<u8>,
}

impl Reassembler {
	pub fn new() -> Self {
		Self::default()
	}

	/// Feed the data phase of one `SET_REPORT` (`[LCB][iAP bytes…]`, i.e. the report
	/// *without* its report-id byte, which `SET_REPORT` conveys in `wValue`). Returns
	/// the concatenated set bytes once the set is complete, else `None`.
	pub fn push(&mut self, report_data: &[u8]) -> Option<Vec<u8>> {
		let (&lcb, data) = report_data.split_first()?;
		if lcb & LCB_CONTINUATION == 0 {
			// Start of a new set; drop any incomplete previous set (§60.5.6).
			self.buf.clear();
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn single_report_picks_smallest_fitting_id() {
		// 9 iAP bytes need LCB + 9 = 10 report bytes → report id 3 (count 13).
		let reports = wrap_input(&[0xAB; 9]);
		assert_eq!(reports.len(), 1);
		assert_eq!(reports[0][0], 3); // report id
		assert_eq!(reports[0][1], 0x00); // LCB: single, self-contained set
		assert_eq!(reports[0].len(), 1 + 13); // id + count
		assert_eq!(&reports[0][2..2 + 9], &[0xAB; 9]);
		assert_eq!(&reports[0][2 + 9..], &[0x00; 3]); // zero padding (13 - 1 LCB - 9 data)
	}

	#[test]
	fn wrap_then_reassemble_roundtrips() {
		let iap: Vec<u8> = (0..20).collect();
		let reports = wrap_input(&iap);
		assert_eq!(reports.len(), 1);
		let mut re = Reassembler::new();
		// strip the report-id byte to mimic what arrives in a SET_REPORT data phase
		let set = re.push(&reports[0][1..]).unwrap();
		// the set is the iAP bytes followed by zero padding
		assert_eq!(&set[..20], &iap[..]);
	}

	#[test]
	fn large_packet_splits_and_reassembles() {
		let iap: Vec<u8> = (0..2000u32).map(|x| x as u8).collect();
		let reports = wrap_input(&iap);
		assert!(reports.len() > 1); // 2000 > MAX_INPUT_CHUNK
		let mut re = Reassembler::new();
		let mut done = None;
		for r in &reports {
			if let Some(set) = re.push(&r[1..]) {
				done = Some(set);
			}
		}
		let set = done.expect("set should complete on the last report");
		assert_eq!(&set[..2000], &iap[..]);
	}
}
