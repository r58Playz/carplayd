//! CarPlay H.264 SPS/VUI rewriting.
//!
//! The encoder gives us a conformant stream that is nonetheless *not* what AIS R29 asks
//! for, and no V4L2 control can fix it — `bcm2835-codec` exposes no VUI knob at all. So the
//! SPS is rewritten on the way past.
//!
//! §30.2.8.1 asks for two things, both in the VUI:
//!
//! - "Video is encoded using full range Y'CbCr, black is 0, white is 255. The accessory
//!   must support the SPS `video_full_range_flag` to reflect this."
//! - "The accessory must support the SPS Video Usability Information (VUI) header with the
//!   `bitstream_restriction_flag` set, `max_num_reorder_frames` of 0, and
//!   `max_dec_frame_buffering` set to the maximum Decoded Picture Buffer (DPB) size."
//!
//! Rewriting rather than splicing is unavoidable: both fields sit inside variable-length
//! exp-Golomb runs, so everything after the edit shifts by a non-whole number of bits. The
//! whole SPS is therefore parsed, the VUI is rebuilt, and the rest is copied through bit
//! for bit.
//!
//! Syntax references are to ITU-T H.264: §7.3.2.1.1 for the SPS, Annex E.1.1 for the VUI,
//! and Table A-1 for the DPB sizes.

use anyhow::{Context, Result, bail};

/// Split an Annex-B bitstream into NAL units, dropping the start codes.
pub(super) fn split_annex_b(data: &[u8]) -> impl Iterator<Item = &[u8]> {
	let mut starts = Vec::new();
	let mut i = 0;
	while i + 3 <= data.len() {
		if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
			starts.push((i, 3));
			i += 3;
		} else if i + 4 <= data.len()
			&& data[i] == 0
			&& data[i + 1] == 0
			&& data[i + 2] == 0
			&& data[i + 3] == 1
		{
			starts.push((i, 4));
			i += 4;
		} else {
			i += 1;
		}
	}

	(0..starts.len()).filter_map(move |n| {
		let (offset, code) = starts[n];
		let begin = offset + code;
		let end = starts.get(n + 1).map_or(data.len(), |(next, _)| *next);
		(begin < end).then(|| &data[begin..end])
	})
}

/// Strip `emulation_prevention_three_byte`s: the `0x03` the encoder inserts after any
/// `00 00` that would otherwise look like a start code (§7.4.1.1).
fn unescape(ebsp: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(ebsp.len());
	let mut zeros = 0;

	for &byte in ebsp {
		if zeros == 2 && byte == 3 {
			zeros = 0;
			continue;
		}
		out.push(byte);
		zeros = if byte == 0 { zeros + 1 } else { 0 };
	}

	out
}

/// The inverse of [`unescape`].
fn escape(rbsp: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(rbsp.len() + 4);
	let mut zeros = 0;

	for &byte in rbsp {
		if zeros == 2 && byte <= 3 {
			out.push(3);
			zeros = 0;
		}
		out.push(byte);
		zeros = if byte == 0 { zeros + 1 } else { 0 };
	}

	out
}

struct BitReader<'a> {
	data: &'a [u8],
	bit: usize,
}

impl<'a> BitReader<'a> {
	fn new(data: &'a [u8]) -> Self {
		Self { data, bit: 0 }
	}

	/// Bits consumed so far — the anchor for copying a prefix through verbatim.
	fn position(&self) -> usize {
		self.bit
	}

	fn read_bit(&mut self) -> Result<u32> {
		let byte = self.bit / 8;
		if byte >= self.data.len() {
			bail!("ran off the end of the bitstream");
		}
		let value = (self.data[byte] >> (7 - self.bit % 8)) & 1;
		self.bit += 1;
		Ok(value as u32)
	}

	fn read_bits(&mut self, count: u32) -> Result<u32> {
		let mut value = 0;
		for _ in 0..count {
			value = (value << 1) | self.read_bit()?;
		}
		Ok(value)
	}

	fn read_flag(&mut self) -> Result<bool> {
		Ok(self.read_bit()? == 1)
	}

	/// Unsigned exp-Golomb, `ue(v)` (§9.1).
	fn read_ue(&mut self) -> Result<u32> {
		let mut leading = 0;
		while self.read_bit()? == 0 {
			leading += 1;
			if leading > 31 {
				bail!("exp-Golomb code longer than 31 leading zeros");
			}
		}
		if leading == 0 {
			return Ok(0);
		}

		let value = (1u64 << leading) - 1 + self.read_bits(leading)? as u64;
		value
			.try_into()
			.context("exp-Golomb value does not fit in 32 bits")
	}

	/// Signed exp-Golomb, `se(v)` (§9.1.1).
	fn read_se(&mut self) -> Result<i32> {
		let k = self.read_ue()?;
		Ok(if k % 2 == 1 {
			((k + 1) / 2) as i32
		} else {
			-((k / 2) as i32)
		})
	}
}

#[derive(Default)]
struct BitWriter {
	data: Vec<u8>,
	/// Bits already written into the byte under construction.
	used: u32,
}

impl BitWriter {
	fn new() -> Self {
		Self::default()
	}

	fn write_bit(&mut self, value: u32) {
		if self.used == 0 {
			self.data.push(0);
		}
		if value & 1 == 1 {
			let last = self.data.len() - 1;
			self.data[last] |= 1 << (7 - self.used);
		}
		self.used = (self.used + 1) % 8;
	}

	fn write_bits(&mut self, value: u32, count: u32) {
		for i in (0..count).rev() {
			self.write_bit((value >> i) & 1);
		}
	}

	fn write_flag(&mut self, value: bool) {
		self.write_bit(value as u32);
	}

	fn write_ue(&mut self, value: u32) {
		let coded = value as u64 + 1;
		let bits = 64 - coded.leading_zeros();
		for _ in 0..bits - 1 {
			self.write_bit(0);
		}
		for i in (0..bits).rev() {
			self.write_bit(((coded >> i) & 1) as u32);
		}
	}

	/// Copy `count` bits straight across, so untouched syntax survives byte for byte.
	fn copy_bits(&mut self, reader: &mut BitReader, count: usize) -> Result<()> {
		for _ in 0..count {
			let bit = reader.read_bit()?;
			self.write_bit(bit);
		}
		Ok(())
	}

	/// `rbsp_trailing_bits()` (§7.3.2.11): a one, then zeros to the byte boundary.
	fn trailing_bits(&mut self) {
		self.write_bit(1);
		while self.used != 0 {
			self.write_bit(0);
		}
	}

	fn finish(self) -> Vec<u8> {
		self.data
	}
}

/// What we need from the SPS to rebuild its tail.
struct SpsInfo {
	level_idc: u8,
	max_num_ref_frames: u32,
	width_mbs: u32,
	/// `FrameHeightInMbs`, which is doubled for field-coded streams.
	height_mbs: u32,
	/// Bit offset of `vui_parameters_present_flag` inside the RBSP.
	vui_present_bit: usize,
	vui_present: bool,
}

/// Profiles whose SPS carries the extra chroma and scaling-matrix syntax (§7.3.2.1.1).
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

fn parse_sps(rbsp: &[u8]) -> Result<SpsInfo> {
	let mut r = BitReader::new(rbsp);

	let profile_idc = r.read_bits(8)? as u8;
	r.read_bits(8)?; // constraint flags and reserved bits
	let level_idc = r.read_bits(8)? as u8;
	r.read_ue()?; // seq_parameter_set_id

	if HIGH_PROFILES.contains(&profile_idc) {
		let chroma_format_idc = r.read_ue()?;
		if chroma_format_idc == 3 {
			r.read_flag()?; // separate_colour_plane_flag
		}
		r.read_ue()?; // bit_depth_luma_minus8
		r.read_ue()?; // bit_depth_chroma_minus8
		r.read_flag()?; // qpprime_y_zero_transform_bypass_flag

		if r.read_flag()? {
			// seq_scaling_matrix_present_flag
			let lists = if chroma_format_idc != 3 { 8 } else { 12 };
			for i in 0..lists {
				if r.read_flag()? {
					skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
				}
			}
		}
	}

	r.read_ue()?; // log2_max_frame_num_minus4

	let pic_order_cnt_type = r.read_ue()?;
	if pic_order_cnt_type == 0 {
		r.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
	} else if pic_order_cnt_type == 1 {
		r.read_flag()?; // delta_pic_order_always_zero_flag
		r.read_se()?; // offset_for_non_ref_pic
		r.read_se()?; // offset_for_top_to_bottom_field
		let cycle = r.read_ue()?;
		for _ in 0..cycle {
			r.read_se()?; // offset_for_ref_frame[i]
		}
	} else if pic_order_cnt_type != 2 {
		bail!("pic_order_cnt_type {pic_order_cnt_type} is not a thing");
	}

	let max_num_ref_frames = r.read_ue()?;
	r.read_flag()?; // gaps_in_frame_num_value_allowed_flag
	let width_mbs = r.read_ue()? + 1;
	let height_map_units = r.read_ue()? + 1;
	let frame_mbs_only = r.read_flag()?;
	if !frame_mbs_only {
		r.read_flag()?; // mb_adaptive_frame_field_flag
	}
	r.read_flag()?; // direct_8x8_inference_flag

	// Skip cropping syntax; the entire prefix is copied unchanged when patching.
	if r.read_flag()? {
		r.read_ue()?; // frame_crop_left_offset
		r.read_ue()?; // frame_crop_right_offset
		r.read_ue()?; // frame_crop_top_offset
		r.read_ue()?; // frame_crop_bottom_offset
	}

	let vui_present_bit = r.position();
	let vui_present = r.read_flag()?;

	Ok(SpsInfo {
		level_idc,
		max_num_ref_frames,
		width_mbs,
		height_mbs: if frame_mbs_only {
			height_map_units
		} else {
			2 * height_map_units
		},
		vui_present_bit,
		vui_present,
	})
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Result<()> {
	let mut last_scale = 8i32;
	let mut next_scale = 8i32;

	for _ in 0..size {
		if next_scale != 0 {
			next_scale = (last_scale + r.read_se()? + 256) % 256;
		}
		if next_scale != 0 {
			last_scale = next_scale;
		}
	}

	Ok(())
}

/// `hrd_parameters()` (Annex E.1.2), kept whole so an existing one survives the rewrite.
#[derive(Clone, Default)]
struct Hrd {
	cpb_cnt_minus1: u32,
	bit_rate_scale: u32,
	cpb_size_scale: u32,
	cpb: Vec<(u32, u32, bool)>,
	initial_cpb_removal_delay_length_minus1: u32,
	cpb_removal_delay_length_minus1: u32,
	dpb_output_delay_length_minus1: u32,
	time_offset_length: u32,
}

impl Hrd {
	fn read(r: &mut BitReader) -> Result<Self> {
		let cpb_cnt_minus1 = r.read_ue()?;
		if cpb_cnt_minus1 > 31 {
			bail!("cpb_cnt_minus1 {cpb_cnt_minus1} is out of range");
		}
		let bit_rate_scale = r.read_bits(4)?;
		let cpb_size_scale = r.read_bits(4)?;

		let mut cpb = Vec::with_capacity(cpb_cnt_minus1 as usize + 1);
		for _ in 0..=cpb_cnt_minus1 {
			cpb.push((r.read_ue()?, r.read_ue()?, r.read_flag()?));
		}

		Ok(Self {
			cpb_cnt_minus1,
			bit_rate_scale,
			cpb_size_scale,
			cpb,
			initial_cpb_removal_delay_length_minus1: r.read_bits(5)?,
			cpb_removal_delay_length_minus1: r.read_bits(5)?,
			dpb_output_delay_length_minus1: r.read_bits(5)?,
			time_offset_length: r.read_bits(5)?,
		})
	}

	fn write(&self, w: &mut BitWriter) {
		w.write_ue(self.cpb_cnt_minus1);
		w.write_bits(self.bit_rate_scale, 4);
		w.write_bits(self.cpb_size_scale, 4);
		for &(bit_rate, cpb_size, cbr) in &self.cpb {
			w.write_ue(bit_rate);
			w.write_ue(cpb_size);
			w.write_flag(cbr);
		}
		w.write_bits(self.initial_cpb_removal_delay_length_minus1, 5);
		w.write_bits(self.cpb_removal_delay_length_minus1, 5);
		w.write_bits(self.dpb_output_delay_length_minus1, 5);
		w.write_bits(self.time_offset_length, 5);
	}
}

/// `vui_parameters()` (Annex E.1.1).
#[derive(Clone, Default)]
struct Vui {
	/// `aspect_ratio_idc`, plus `sar_width`/`sar_height` when it is `Extended_SAR`.
	aspect_ratio: Option<(u32, Option<(u32, u32)>)>,
	overscan_appropriate: Option<bool>,
	/// `video_format`, `video_full_range_flag`, and the optional colour description.
	video_signal: Option<(u32, bool, Option<(u32, u32, u32)>)>,
	chroma_loc: Option<(u32, u32)>,
	timing: Option<(u32, u32, bool)>,
	nal_hrd: Option<Hrd>,
	vcl_hrd: Option<Hrd>,
	low_delay_hrd: Option<bool>,
	pic_struct_present: bool,
	/// `motion_vectors_over_pic_boundaries_flag`, `max_bytes_per_pic_denom`,
	/// `max_bits_per_mb_denom`, the two `log2_max_mv_length`s, `max_num_reorder_frames`
	/// and `max_dec_frame_buffering`.
	bitstream_restriction: Option<(bool, u32, u32, u32, u32, u32, u32)>,
}

impl Vui {
	fn read(r: &mut BitReader) -> Result<Self> {
		let mut vui = Self::default();

		if r.read_flag()? {
			let idc = r.read_bits(8)?;
			// Extended_SAR
			let sar =
				(idc == 255).then(|| -> Result<_> { Ok((r.read_bits(16)?, r.read_bits(16)?)) });
			vui.aspect_ratio = Some((idc, sar.transpose()?));
		}
		if r.read_flag()? {
			vui.overscan_appropriate = Some(r.read_flag()?);
		}
		if r.read_flag()? {
			let video_format = r.read_bits(3)?;
			let full_range = r.read_flag()?;
			let colour = if r.read_flag()? {
				Some((r.read_bits(8)?, r.read_bits(8)?, r.read_bits(8)?))
			} else {
				None
			};
			vui.video_signal = Some((video_format, full_range, colour));
		}
		if r.read_flag()? {
			vui.chroma_loc = Some((r.read_ue()?, r.read_ue()?));
		}
		if r.read_flag()? {
			vui.timing = Some((r.read_bits(32)?, r.read_bits(32)?, r.read_flag()?));
		}
		if r.read_flag()? {
			vui.nal_hrd = Some(Hrd::read(r)?);
		}
		if r.read_flag()? {
			vui.vcl_hrd = Some(Hrd::read(r)?);
		}
		if vui.nal_hrd.is_some() || vui.vcl_hrd.is_some() {
			vui.low_delay_hrd = Some(r.read_flag()?);
		}
		vui.pic_struct_present = r.read_flag()?;
		if r.read_flag()? {
			vui.bitstream_restriction = Some((
				r.read_flag()?,
				r.read_ue()?,
				r.read_ue()?,
				r.read_ue()?,
				r.read_ue()?,
				r.read_ue()?,
				r.read_ue()?,
			));
		}

		Ok(vui)
	}

	fn write(&self, w: &mut BitWriter) {
		match self.aspect_ratio {
			Some((idc, sar)) => {
				w.write_flag(true);
				w.write_bits(idc, 8);
				if let Some((width, height)) = sar {
					w.write_bits(width, 16);
					w.write_bits(height, 16);
				}
			}
			None => w.write_flag(false),
		}

		match self.overscan_appropriate {
			Some(appropriate) => {
				w.write_flag(true);
				w.write_flag(appropriate);
			}
			None => w.write_flag(false),
		}

		match self.video_signal {
			Some((video_format, full_range, colour)) => {
				w.write_flag(true);
				w.write_bits(video_format, 3);
				w.write_flag(full_range);
				match colour {
					Some((primaries, transfer, matrix)) => {
						w.write_flag(true);
						w.write_bits(primaries, 8);
						w.write_bits(transfer, 8);
						w.write_bits(matrix, 8);
					}
					None => w.write_flag(false),
				}
			}
			None => w.write_flag(false),
		}

		match self.chroma_loc {
			Some((top, bottom)) => {
				w.write_flag(true);
				w.write_ue(top);
				w.write_ue(bottom);
			}
			None => w.write_flag(false),
		}

		match self.timing {
			Some((units_in_tick, time_scale, fixed)) => {
				w.write_flag(true);
				w.write_bits(units_in_tick, 32);
				w.write_bits(time_scale, 32);
				w.write_flag(fixed);
			}
			None => w.write_flag(false),
		}

		for hrd in [&self.nal_hrd, &self.vcl_hrd] {
			match hrd {
				Some(hrd) => {
					w.write_flag(true);
					hrd.write(w);
				}
				None => w.write_flag(false),
			}
		}
		if let Some(low_delay) = self.low_delay_hrd {
			w.write_flag(low_delay);
		}

		w.write_flag(self.pic_struct_present);

		match self.bitstream_restriction {
			Some((mv_over_boundaries, bytes_denom, bits_denom, mv_h, mv_v, reorder, dpb)) => {
				w.write_flag(true);
				w.write_flag(mv_over_boundaries);
				w.write_ue(bytes_denom);
				w.write_ue(bits_denom);
				w.write_ue(mv_h);
				w.write_ue(mv_v);
				w.write_ue(reorder);
				w.write_ue(dpb);
			}
			None => w.write_flag(false),
		}
	}
}

/// What §30.2.8.1 wants stated in the VUI.
#[derive(Clone, Copy, Debug)]
pub(super) struct VuiSettings {
	/// `video_full_range_flag`.
	///
	/// The spec says CarPlay content is always full range, so this defaults to `true`.
	/// It is a knob because a transcoded Android Auto stream arrives *limited* range, and
	/// asserting full range over limited-range samples lifts the blacks and clips the
	/// whites. Whether a real head unit honours the flag or assumes full range regardless
	/// is untested — if it honours it, clear this for AA content; if it does not, the
	/// pixels need converting instead.
	pub(super) full_range: bool,
}

impl Default for VuiSettings {
	fn default() -> Self {
		Self { full_range: true }
	}
}

/// `video_format`: 5 is "Unspecified" (Table E-2), the value to use when we are not
/// claiming to have come from a particular broadcast standard.
const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;

/// `colour_primaries`, `transfer_characteristics`, `matrix_coefficients` exactly as a real
/// iPhone signals them: BT.709 primaries and transfer with the BT.601 matrix. The mix looks
/// wrong and is what shipped, so it is what we send. **[V]**, from the 20-byte SPS in the
/// U2AW trace.
const REFERENCE_COLOUR: (u32, u32, u32) = (1, 1, 6);

// Values the decoder already infers when `bitstream_restriction_flag` is 0 (Annex E.2.1).
// Re-stating them means turning the flag on asserts nothing new beyond the two fields the
// spec actually asks for.
const DEFAULT_MV_OVER_PIC_BOUNDARIES: bool = true;
const DEFAULT_MAX_BYTES_PER_PIC_DENOM: u32 = 2;
const DEFAULT_MAX_BITS_PER_MB_DENOM: u32 = 1;
const DEFAULT_LOG2_MAX_MV_LENGTH: u32 = 15;

/// `MaxDpbMbs` from Table A-1, for the levels Table 30-12 can ask for.
fn max_dpb_mbs(level_idc: u8) -> Option<u32> {
	Some(match level_idc {
		30 => 8100,
		31 => 18000,
		32 => 20480,
		40 | 41 => 32768,
		42 => 34816,
		_ => return None,
	})
}

/// `MaxDpbFrames` = `Min(MaxDpbMbs / (PicWidthInMbs * FrameHeightInMbs), 16)`, per §A.3.1.
fn max_dpb_frames(level_idc: u8, width_mbs: u32, height_mbs: u32) -> Option<u32> {
	let macroblocks = width_mbs.checked_mul(height_mbs)?;
	if macroblocks == 0 {
		return None;
	}
	Some((max_dpb_mbs(level_idc)? / macroblocks).min(16))
}

/// Rewrite an SPS NAL so its VUI says what §30.2.8.1 requires.
///
/// Everything ahead of `vui_parameters_present_flag` is copied bit for bit; only the VUI
/// is rebuilt. Aspect ratio and other display metadata survive, while timing and HRD
/// are removed to keep the CarPlay configuration record compact.
pub(super) fn patch_sps(nal: &[u8], settings: &VuiSettings) -> Result<Vec<u8>> {
	let (&header, payload) = nal.split_first().context("empty SPS NAL")?;
	if header & 0x1F != 7 {
		bail!("not an SPS NAL: header byte {header:#04x}");
	}

	let rbsp = unescape(payload);
	let info = parse_sps(&rbsp).context("failed to parse the SPS")?;

	let mut vui = if info.vui_present {
		let mut r = BitReader::new(&rbsp);
		// Walk back to the VUI; the prefix parse is cheap and keeps one source of truth
		// for the syntax order.
		for _ in 0..=info.vui_present_bit {
			r.read_bit()?;
		}
		Vui::read(&mut r).context("failed to parse the existing VUI")?
	} else {
		Vui::default()
	};

	// "The accessory must support the SPS video_full_range_flag to reflect this."
	let (video_format, colour) = match vui.video_signal {
		Some((format, _, colour)) => (format, colour),
		None => (VIDEO_FORMAT_UNSPECIFIED, None),
	};
	vui.video_signal = Some((
		video_format,
		settings.full_range,
		colour.or(Some(REFERENCE_COLOUR)),
	));

	// Everything a real device leaves out, left out.
	//
	// The bcm2835 driver emits its own VUI carrying timing info and a full NAL HRD block,
	// and round-tripping those pushed the SPS to 38 bytes against the 20 and 22 seen from
	// real iPhones — a 58-byte `AVCDecoderConfigurationRecord` where every capture is 39
	// or 41. The receiver has an explicit `### Screen avcc data length too long: %d` path,
	// so an oversized record is a thing it is known to reject outright, and a decoder that
	// never gets configured shows a black screen while consuming every frame we send
	// without a word of complaint. **[V]** for the string, **[I]** for the bound.
	//
	// Neither field is a loss. Both are optional and informational: the frame rate is
	// negotiated out of band through the accessory's `maxFps`, and the HRD is a buffering
	// model no decoder needs to play the stream. Apple's own encoder emits neither.
	vui.timing = None;
	vui.nal_hrd = None;
	vui.vcl_hrd = None;
	vui.low_delay_hrd = None;

	// "bitstream_restriction_flag set, max_num_reorder_frames of 0, and
	// max_dec_frame_buffering set to the maximum DPB size."
	let dpb = max_dpb_frames(info.level_idc, info.width_mbs, info.height_mbs)
		.with_context(|| format!("no Table A-1 entry for level_idc {}", info.level_idc))?
		// The DPB can never be smaller than the reference frames the stream actually uses.
		.max(info.max_num_ref_frames);
	let existing = vui.bitstream_restriction;
	vui.bitstream_restriction = Some((
		existing.map_or(DEFAULT_MV_OVER_PIC_BOUNDARIES, |r| r.0),
		existing.map_or(DEFAULT_MAX_BYTES_PER_PIC_DENOM, |r| r.1),
		existing.map_or(DEFAULT_MAX_BITS_PER_MB_DENOM, |r| r.2),
		existing.map_or(DEFAULT_LOG2_MAX_MV_LENGTH, |r| r.3),
		existing.map_or(DEFAULT_LOG2_MAX_MV_LENGTH, |r| r.4),
		0, // max_num_reorder_frames — no B-frames, so nothing is ever reordered
		dpb,
	));

	let mut w = BitWriter::new();
	let mut r = BitReader::new(&rbsp);
	w.copy_bits(&mut r, info.vui_present_bit)
		.context("failed to copy the SPS prefix")?;
	w.write_flag(true); // vui_parameters_present_flag
	vui.write(&mut w);
	w.trailing_bits();

	let mut out = Vec::with_capacity(nal.len() + 8);
	out.push(header);
	out.extend_from_slice(&escape(&w.finish()));

	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	// Captured from a live Toyota/Kenwood 15CY session at 1080x678, with cropping,
	// timing information and NAL HRD.
	const DRIVER_SPS: &[u8] = &[
		0x27, 0x64, 0x00, 0x1F, 0xAC, 0x2B, 0x40, 0x22, 0x02, 0xBF, 0x2C, 0xD3, 0x64, 0x00, 0x00,
		0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF3, 0x82, 0x00, 0x00, 0x86, 0x47, 0x00, 0x01,
		0xAB, 0x3F, 0xBD, 0xEE, 0x01, 0xE2, 0x44, 0xCF,
	];

	/// A real 800x480 High 3.1 SPS with no VUI, as `bcm2835-codec` emits it.
	fn sample_sps() -> Vec<u8> {
		// Built with the writer so the test does not depend on a captured blob, then
		// checked below against an independent parse.
		let mut w = BitWriter::new();
		w.write_bits(100, 8); // profile_idc = High
		w.write_bits(0, 8); // constraint flags + reserved
		w.write_bits(31, 8); // level_idc = 3.1
		w.write_ue(0); // seq_parameter_set_id
		w.write_ue(1); // chroma_format_idc = 4:2:0
		w.write_ue(0); // bit_depth_luma_minus8
		w.write_ue(0); // bit_depth_chroma_minus8
		w.write_flag(false); // qpprime_y_zero_transform_bypass_flag
		w.write_flag(false); // seq_scaling_matrix_present_flag
		w.write_ue(4); // log2_max_frame_num_minus4
		w.write_ue(2); // pic_order_cnt_type
		w.write_ue(1); // max_num_ref_frames
		w.write_flag(false); // gaps_in_frame_num_value_allowed_flag
		w.write_ue(49); // pic_width_in_mbs_minus1 -> 800
		w.write_ue(29); // pic_height_in_map_units_minus1 -> 480
		w.write_flag(true); // frame_mbs_only_flag
		w.write_flag(true); // direct_8x8_inference_flag
		w.write_flag(false); // frame_cropping_flag
		w.write_flag(false); // vui_parameters_present_flag
		w.trailing_bits();

		let mut nal = vec![0x67];
		nal.extend_from_slice(&escape(&w.finish()));
		nal
	}

	#[test]
	fn annex_b_splits_on_both_start_code_lengths() {
		let stream = [
			0, 0, 0, 1, 0x67, 0xAA, // 4-byte start code, SPS
			0, 0, 1, 0x68, 0xBB, // 3-byte start code, PPS
			0, 0, 0, 1, 0x65, 0xCC, 0xDD, // IDR
		];
		let nals: Vec<_> = split_annex_b(&stream).collect();

		assert_eq!(nals.len(), 3);
		assert_eq!(nals[0], &[0x67, 0xAA]);
		assert_eq!(nals[1], &[0x68, 0xBB]);
		assert_eq!(nals[2], &[0x65, 0xCC, 0xDD]);
	}

	#[test]
	fn an_empty_bitstream_yields_nothing() {
		assert_eq!(split_annex_b(&[]).count(), 0);
		assert_eq!(split_annex_b(&[0, 0, 0, 1]).count(), 0);
	}

	#[test]
	fn exp_golomb_round_trips() {
		for value in [0, 1, 2, 3, 8, 49, 255, 65535, 1_000_000] {
			let mut w = BitWriter::new();
			w.write_ue(value);
			w.trailing_bits();
			assert_eq!(BitReader::new(&w.finish()).read_ue().unwrap(), value);
		}
		// se(v) only ever appears in the SPS prefix, which is copied through bit for bit,
		// so there is nothing that writes one — read side only.
		for (coded, expected) in [(0, 0), (1, 1), (2, -1), (3, 2), (4, -2)] {
			let mut w = BitWriter::new();
			w.write_ue(coded);
			w.trailing_bits();
			assert_eq!(BitReader::new(&w.finish()).read_se().unwrap(), expected);
		}
	}

	#[test]
	fn emulation_prevention_round_trips() {
		let rbsp = vec![
			0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x03, 0xFF, 0x00, 0x00, 0x02,
		];
		let escaped = escape(&rbsp);
		assert!(
			escaped.len() > rbsp.len(),
			"should have inserted 0x03 bytes"
		);
		assert_eq!(unescape(&escaped), rbsp);
		// No sequence in the escaped form can be mistaken for a start code.
		assert!(!escaped.windows(3).any(|w| w == [0, 0, 1]));
	}

	#[test]
	fn geometry_and_level_are_read_back() {
		let nal = sample_sps();
		let info = parse_sps(&unescape(&nal[1..])).unwrap();

		assert_eq!(info.level_idc, 31);
		assert_eq!(info.width_mbs, 50, "800 / 16");
		assert_eq!(info.height_mbs, 30, "480 / 16");
		assert!(!info.vui_present);
	}

	#[test]
	fn dpb_follows_table_a_1() {
		// 800x480 at level 3.1: 18000 / (50 * 30) = 12.
		assert_eq!(max_dpb_frames(31, 50, 30), Some(12));
		// 1280x720 at level 3.1: 18000 / (80 * 45) = 5.
		assert_eq!(max_dpb_frames(31, 80, 45), Some(5));
		// 1920x720 at level 4.0: 32768 / (120 * 45) = 6.
		assert_eq!(max_dpb_frames(40, 120, 45), Some(6));
		// Capped at 16 however small the picture.
		assert_eq!(max_dpb_frames(40, 2, 2), Some(16));
		assert_eq!(max_dpb_frames(50, 80, 45), None, "level not in our table");
	}

	#[test]
	fn patching_adds_the_vui_the_spec_asks_for() {
		let patched = patch_sps(&sample_sps(), &VuiSettings::default()).unwrap();

		let rbsp = unescape(&patched[1..]);
		let info = parse_sps(&rbsp).unwrap();
		assert!(info.vui_present, "a VUI must now be present");

		let mut r = BitReader::new(&rbsp);
		for _ in 0..=info.vui_present_bit {
			r.read_bit().unwrap();
		}
		let vui = Vui::read(&mut r).unwrap();

		let (_, full_range, _) = vui.video_signal.expect("video_signal_type_present_flag");
		assert!(full_range, "§30.2.8.1 wants full range");

		let restriction = vui
			.bitstream_restriction
			.expect("bitstream_restriction_flag");
		assert_eq!(restriction.5, 0, "max_num_reorder_frames");
		assert_eq!(
			restriction.6, 12,
			"max_dec_frame_buffering for 800x480 at 3.1"
		);
	}

	#[test]
	fn patching_preserves_everything_ahead_of_the_vui() {
		for original in [sample_sps(), DRIVER_SPS.to_vec()] {
			let patched = patch_sps(&original, &VuiSettings::default()).unwrap();
			let original_rbsp = unescape(&original[1..]);
			let patched_rbsp = unescape(&patched[1..]);
			let before = parse_sps(&original_rbsp).unwrap();
			let after = parse_sps(&patched_rbsp).unwrap();

			assert_eq!(patched[0], original[0], "NAL header byte");
			assert_eq!(after.level_idc, before.level_idc);
			assert_eq!(after.width_mbs, before.width_mbs);
			assert_eq!(after.height_mbs, before.height_mbs);
			assert_eq!(after.max_num_ref_frames, before.max_num_ref_frames);
			assert_eq!(after.vui_present_bit, before.vui_present_bit);

			// Compare the entire prefix, including cropping and syntax we only skip.
			let mut original_bits = BitReader::new(&original_rbsp);
			let mut patched_bits = BitReader::new(&patched_rbsp);
			for bit in 0..before.vui_present_bit {
				assert_eq!(
					original_bits.read_bit().unwrap(),
					patched_bits.read_bit().unwrap(),
					"SPS prefix bit {bit} changed"
				);
			}
		}
	}

	#[test]
	fn patching_is_idempotent() {
		let once = patch_sps(&sample_sps(), &VuiSettings::default()).unwrap();
		let twice = patch_sps(&once, &VuiSettings::default()).unwrap();
		assert_eq!(once, twice, "re-patching an already-patched SPS is a no-op");
	}

	/// The real driver SPS off the wire, and the size of the configuration record built
	/// from it.
	///
	/// `bcm2835-codec` emits its own VUI with timing info and a full NAL HRD block, which
	/// round-tripped into a 38-byte SPS and a 58-byte `AVCDecoderConfigurationRecord`.
	/// Every capture of a real iPhone's record is 39 or 41 bytes, and the receiver has an
	/// explicit `### Screen avcc data length too long: %d` path — so this is not a place to
	/// be 45% over the reference and hope.
	#[test]
	fn the_configuration_record_stays_near_the_reference_size() {
		let pps_len = 5;

		let patched = patch_sps(DRIVER_SPS, &VuiSettings::default()).unwrap();

		// 11 bytes of record header and counts, the two parameter sets, and the four
		// trailing bytes every capture carries.
		let record = 11 + patched.len() + pps_len + 4;
		assert!(
			record <= 48,
			"the record is {record} bytes from a {} byte SPS; the reference is 39 to 41",
			patched.len()
		);

		// And the patch still did its actual job.
		let rbsp = unescape(&patched[1..]);
		let info = parse_sps(&rbsp).unwrap();
		let mut r = BitReader::new(&rbsp);
		for _ in 0..=info.vui_present_bit {
			r.read_bit().unwrap();
		}
		let vui = Vui::read(&mut r).unwrap();
		assert!(vui.video_signal.unwrap().1, "full range");
		assert_eq!(vui.bitstream_restriction.unwrap().5, 0, "no reordering");
		assert_eq!(vui.timing, None);
		assert!(vui.nal_hrd.is_none());
	}

	#[test]
	fn an_existing_vui_keeps_its_other_fields() {
		// Patch once to get a VUI, hand-set an aspect ratio and timing, then patch again.
		// Everything but the timing and HRD must survive the round trip.
		let patched = patch_sps(&sample_sps(), &VuiSettings::default()).unwrap();
		let rbsp = unescape(&patched[1..]);
		let info = parse_sps(&rbsp).unwrap();

		let mut r = BitReader::new(&rbsp);
		for _ in 0..=info.vui_present_bit {
			r.read_bit().unwrap();
		}
		let mut vui = Vui::read(&mut r).unwrap();
		vui.aspect_ratio = Some((255, Some((4, 3))));
		vui.timing = Some((1000, 60000, true));

		let mut w = BitWriter::new();
		let mut copy = BitReader::new(&rbsp);
		w.copy_bits(&mut copy, info.vui_present_bit).unwrap();
		w.write_flag(true);
		vui.write(&mut w);
		w.trailing_bits();
		let mut rebuilt = vec![patched[0]];
		rebuilt.extend_from_slice(&escape(&w.finish()));

		// Now patch that, with the opposite range, and check the extras survived.
		let again = patch_sps(&rebuilt, &VuiSettings { full_range: false }).unwrap();
		let rbsp = unescape(&again[1..]);
		let info = parse_sps(&rbsp).unwrap();
		let mut r = BitReader::new(&rbsp);
		for _ in 0..=info.vui_present_bit {
			r.read_bit().unwrap();
		}
		let vui = Vui::read(&mut r).unwrap();

		assert_eq!(vui.aspect_ratio, Some((255, Some((4, 3)))));
		assert_eq!(vui.video_signal.unwrap().1, false, "range override applied");
		assert_eq!(vui.bitstream_restriction.unwrap().5, 0);

		// Timing and the HRD are the exception: they go, however they got there. A real
		// device sends neither, and carrying the driver's copies made the configuration
		// record half again as long as any capture of one.
		assert_eq!(vui.timing, None, "timing info is dropped");
		assert_eq!(vui.nal_hrd.is_none(), true, "NAL HRD is dropped");
		assert_eq!(vui.vcl_hrd.is_none(), true, "VCL HRD is dropped");
	}

	#[test]
	fn a_non_sps_nal_is_rejected() {
		assert!(patch_sps(&[0x68, 0xAA], &VuiSettings::default()).is_err());
		assert!(patch_sps(&[], &VuiSettings::default()).is_err());
	}
}
