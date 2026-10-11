use std::{os::fd::AsFd, path::Path, sync::Arc, time::Instant};

use anyhow::{Context, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{Interest, unix::AsyncFd};
use v4l2r::{
	Format, bindings,
	controls::{
		AsV4l2ControlSlice, ExtControlTrait, SafeExtControl,
		codec::{
			VideoBFrames, VideoBitrate, VideoForceKeyFrame, VideoH264IPeriod, VideoH264Level,
			VideoH264Profile,
		},
	},
	device::{
		AllocatedQueue, Device, Stream,
		queue::{self, GetFreeCaptureBuffer, GetFreeOutputBuffer, Queue, QueueInit},
	},
	ioctl::{CtrlWhich, PlaneMapping},
	memory::MmapHandle,
};

use super::h264::{self, VuiSettings};
use crate::{
	provider::CarplayProviderScreen,
	v4l2::{CaptureQueue, OutputQueue, VideoParams, dequeue, open_device, validate_mmap_buffers},
};

pub const HEADER_LEN: usize = 128;
pub const TAG_LEN: usize = 16;

/// carlinkit trailer
const CONFIG_TRAILER: [u8; 4] = [0x02, 0x00, 0x00, 0x00];

pub struct EncodedVideoFrame {
	/// has HEADER_LEN + TAG_LEN 0'd
	pub bytes: BytesMut,
	/// len without HEADER_LEN + TAG_LEN
	pub len: u32,
	pub timestamp: Instant,
	pub is_idr: bool,
}

fn validate_output_format(
	pix: bindings::v4l2_pix_format_mplane,
	width: u32,
	height: u32,
	fps: u32,
) -> Result<VideoParams> {
	if pix.pixelformat != u32::from_le_bytes(*b"YU12") {
		bail!("yu12 encoding not supported by device");
	}
	if pix.num_planes != 1 {
		bail!(
			"YU12 output must have one memory plane, got {}",
			pix.num_planes
		);
	}
	if pix.field != bindings::v4l2_field_V4L2_FIELD_NONE {
		bail!("YU12 output must be progressive");
	}
	// Copy packed fields before formatting them in diagnostics.
	let (actual_width, actual_height) = (pix.width, pix.height);
	if actual_width != width || actual_height != height {
		bail!(
			"output dimensions: requested {width}x{height}, driver returned {actual_width}x{actual_height}"
		);
	}
	if pix.colorspace != bindings::v4l2_colorspace_V4L2_COLORSPACE_REC709 {
		bail!("YU12 output must use Rec.709 colorspace");
	}
	if pix.quantization != bindings::v4l2_quantization_V4L2_QUANTIZATION_FULL_RANGE as u8 {
		bail!("YU12 output must use full-range quantization");
	}
	// DEFAULT resolves to the Rec.709 matrix and transfer function for REC709.
	let ycbcr_enc = unsafe { pix.__bindgen_anon_1.ycbcr_enc } as u32;
	if ycbcr_enc != bindings::v4l2_ycbcr_encoding_V4L2_YCBCR_ENC_DEFAULT
		&& ycbcr_enc != bindings::v4l2_ycbcr_encoding_V4L2_YCBCR_ENC_709
	{
		bail!("YU12 output must use the Rec.709 YCbCr matrix");
	}
	if pix.xfer_func as u32 != bindings::v4l2_xfer_func_V4L2_XFER_FUNC_DEFAULT
		&& pix.xfer_func as u32 != bindings::v4l2_xfer_func_V4L2_XFER_FUNC_709
	{
		bail!("YU12 output must use the Rec.709 transfer function");
	}

	let params = VideoParams {
		width,
		height,
		fps,
		bytes_per_line: pix.plane_fmt[0].bytesperline as usize,
		buffer_height: actual_height as usize,
	};
	params
		.validate(pix.plane_fmt[0].sizeimage as usize)
		.context("invalid negotiated YU12 layout")?;
	Ok(params)
}

fn validate_output_frame_interval(parm: &bindings::v4l2_streamparm, fps: u32) -> Result<()> {
	if parm.type_ != bindings::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE {
		bail!("frame interval must describe VIDEO_OUTPUT_MPLANE");
	}
	// VIDEO_OUTPUT_MPLANE selects the output member of the streamparm union.
	let interval = unsafe { parm.parm.output.timeperframe };
	let (numerator, denominator) = (interval.numerator, interval.denominator);
	// Accept equivalent fractions, such as 2/60 for a requested 30 fps.
	if numerator == 0
		|| denominator == 0
		|| u64::from(numerator) * u64::from(fps) != u64::from(denominator)
	{
		bail!(
			"output frame interval: requested 1/{fps}, driver returned {numerator}/{denominator}"
		);
	}
	Ok(())
}

// vibecoded thing from the table
fn level_for(width: u32, height: u32, fps: u32) -> Result<(VideoH264Level, i32)> {
	if width == 0 || height == 0 || fps == 0 {
		bail!("{}x{}@{} is not a usable geometry", width, height, fps);
	}
	if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
		bail!("YU12 visible dimensions must be even");
	}

	// A macroblock is 16x16, and a frame that is not a whole number of them is coded up to
	// the next one and cropped back in the SPS. 1080x678 codes as 68x43 macroblocks.
	let mbs = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
	let mbs_per_second = mbs
		.checked_mul(u64::from(fps))
		.context("macroblocks per second overflow")?;

	// (level, MaxFS, MaxMBPS, wired ceiling from Table 30-12), lowest first. Only the four
	// levels Table 30-12 names are offered; anything needing more is out of scope for
	// CarPlay rather than merely unsupported here.
	const LEVELS: &[(VideoH264Level, u32, u32, i32)] = &[
		(VideoH264Level::L3_1, 3600, 108_000, 17_500_000),
		(VideoH264Level::L3_2, 5120, 216_000, 25_000_000),
		(VideoH264Level::L4_0, 8192, 245_760, 25_000_000),
		(VideoH264Level::L4_2, 8704, 522_240, 25_000_000),
	];

	LEVELS
		.iter()
		.find(|(_, max_fs, max_mbps, _)| {
			mbs <= u64::from(*max_fs) && mbs_per_second <= u64::from(*max_mbps)
		})
		.map(|(level, _, _, ceiling)| (*level, *ceiling))
		.with_context(|| {
			format!(
				"{}x{}@{} needs {mbs} macroblocks at {mbs_per_second}/s, beyond High Profile 4.2",
				width, height, fps
			)
		})
}

struct VideoH264RepeatSeqHeader(pub bool);
impl ExtControlTrait for VideoH264RepeatSeqHeader {
	const ID: u32 = 0x0099_09e2;
	type PAYLOAD = i32;
}
impl From<VideoH264RepeatSeqHeader> for i32 {
	fn from(value: VideoH264RepeatSeqHeader) -> Self {
		value.0 as i32
	}
}

#[repr(C)]
struct V4l2Controls {
	bitrate: SafeExtControl<VideoBitrate>,
	profile: SafeExtControl<VideoH264Profile>,
	level: SafeExtControl<VideoH264Level>,
	b_frames: SafeExtControl<VideoBFrames>,
	i_period: SafeExtControl<VideoH264IPeriod>,
	repeat_seq_header: SafeExtControl<VideoH264RepeatSeqHeader>,
}
impl Clone for V4l2Controls {
	fn clone(&self) -> Self {
		Self {
			bitrate: SafeExtControl::from_value(self.bitrate.value()),
			profile: SafeExtControl::from_value(self.profile.value()),
			level: SafeExtControl::from_value(self.level.value()),
			b_frames: SafeExtControl::from_value(self.b_frames.value()),
			i_period: SafeExtControl::from_value(self.i_period.value()),
			repeat_seq_header: SafeExtControl::from_value(self.repeat_seq_header.value()),
		}
	}
}
impl V4l2Controls {
	fn ensure_matches(&self, expected: &Self) -> Result<()> {
		let comparisons = [
			("bitrate", expected.bitrate.value(), self.bitrate.value()),
			("profile", expected.profile.value(), self.profile.value()),
			("level", expected.level.value(), self.level.value()),
			("b_frames", expected.b_frames.value(), self.b_frames.value()),
			("i_period", expected.i_period.value(), self.i_period.value()),
			(
				"repeat_seq_header",
				expected.repeat_seq_header.value(),
				self.repeat_seq_header.value(),
			),
		];

		for (name, requested, actual) in comparisons {
			if requested != actual {
				bail!("{name}: requested {requested}, driver returned {actual}");
			}
		}

		Ok(())
	}
}
impl AsV4l2ControlSlice for &mut V4l2Controls {
	fn as_v4l2_control_slice(&mut self) -> &mut [v4l2r::bindings::v4l2_ext_control] {
		let ptr = (*self) as *mut V4l2Controls as *mut v4l2r::bindings::v4l2_ext_control;
		unsafe { std::slice::from_raw_parts_mut(ptr, 6) }
	}
}

enum PatchedNal<'a> {
	Sps(Bytes),
	Pps(Bytes),
	Sent(u32, bool, &'a [u8]),
}

struct NegotiatedEncoder {
	device: Arc<Device>,
	output_queue: Queue<queue::direction::Output, QueueInit>,
	capture_queue: Queue<queue::direction::Capture, QueueInit>,
	params: VideoParams,
	output_size: usize,
	capture_size: usize,
}

pub struct VideoEncoder {
	pub params: VideoParams,
	sps: Option<Bytes>,
	pps: Option<Bytes>,
	vui: VuiSettings,

	device: AsyncFd<Arc<Device>>,
	input: OutputQueue,
	output: CaptureQueue,
}
impl VideoEncoder {
	fn process_frame(&mut self, mapping: PlaneMapping) -> Result<(BytesMut, u32, bool)> {
		let nals = h264::split_annex_b(&mapping)
			.map(|x| {
				Ok(match x[0] & 0x1F {
					7 => {
						let patched =
							h264::patch_sps(x, &self.vui).context("failed to patch the SPS VUI")?;
						PatchedNal::Sps(Bytes::from(patched))
					}
					8 => PatchedNal::Pps(Bytes::copy_from_slice(x)),
					kind => PatchedNal::Sent(x.len() as u32, kind == 5, x),
				})
			})
			.collect::<Result<Vec<_>>>()?;

		let len = nals
			.iter()
			.map(|x| {
				if let PatchedNal::Sent(_, _, nal) = x {
					nal.len() + 4
				} else {
					0
				}
			})
			.sum::<usize>();
		let mut buf = BytesMut::with_capacity(HEADER_LEN + len + TAG_LEN);

		buf.put_bytes(0, HEADER_LEN);

		let mut is_idr = false;

		for nal in nals {
			match nal {
				PatchedNal::Sent(len, idr, nal) => {
					is_idr = is_idr || idr;
					buf.put_u32(len);
					buf.put_slice(nal);
				}
				PatchedNal::Sps(x) => {
					self.sps.replace(x);
				}
				PatchedNal::Pps(x) => {
					self.pps.replace(x);
				}
			}
		}

		Ok((buf, len as u32, is_idr))
	}

	pub async fn next_frame(
		&mut self,
		provider: &mut dyn CarplayProviderScreen,
		force_key_frame: impl FnOnce() -> bool,
	) -> Result<Option<EncodedVideoFrame>> {
		let frame_size = self.params.frame_size()?;
		self.output
			.try_get_free_buffer()
			.context("failed to get output buffer")?
			.queue()
			.context("failed to queue output buffer")?;

		let buffer = self
			.input
			.try_get_free_buffer()
			.context("failed to get input buffer")?;
		let mut in_mapping = buffer
			.get_plane_mapping(0)
			.context("failed to get input mapping")?;

		let Some(timestamp) = provider
			.next_frame(&mut in_mapping)
			.await
			.context("failed to get next frame")?
		else {
			return Ok(None);
		};

		if (force_key_frame)() {
			v4l2r::ioctl::s_ext_ctrls(
				&self.device,
				CtrlWhich::Current,
				&mut SafeExtControl::<VideoForceKeyFrame>::from_value(VideoForceKeyFrame.into()),
			)
			.context("failed to set controls")?;
		}

		drop(in_mapping);
		buffer
			.queue(&[frame_size])
			.context("failed to queue input buffer")?;

		// wait?
		dequeue(&self.device, &self.input, Interest::WRITABLE)
			.await
			.context("failed to wait for input buffer")?
			.take_handles()
			.context("failed to take input handles")?;

		let output = dequeue(&self.device, &self.output, Interest::READABLE)
			.await
			.context("failed to wait for output buffer")?;
		let out_mapping = output
			.get_plane_mapping(0)
			.context("failed to get output buffer")?;

		let (processed, len, is_idr) = self
			.process_frame(out_mapping)
			.context("failed to process frame")?;

		Ok(Some(EncodedVideoFrame {
			bytes: processed,
			len,
			timestamp,
			is_idr,
		}))
	}

	pub fn get_config(&self) -> Option<Bytes> {
		let (sps, pps) = (self.sps.as_ref()?, self.pps.as_ref()?);
		if sps.len() < 4 {
			return None;
		}

		let mut out = BytesMut::with_capacity(11 + sps.len() + pps.len() + CONFIG_TRAILER.len());
		out.put_u8(1); // configurationVersion
		out.put_u8(sps[1]); // AVCProfileIndication
		out.put_u8(sps[2]); // profile_compatibility
		out.put_u8(sps[3]); // AVCLevelIndication
		out.put_u8(0xFF); // 6 bits reserved | lengthSizeMinusOne = 3, i.e. 4-byte lengths
		out.put_u8(0xE1); // 3 bits reserved | numOfSequenceParameterSets = 1
		out.put_u16(sps.len() as u16);
		out.put_slice(sps);
		out.put_u8(1); // numOfPictureParameterSets
		out.put_u16(pps.len() as u16);
		out.put_slice(pps);
		out.put_slice(&CONFIG_TRAILER);

		Some(out.freeze())
	}

	fn negotiate(device: &Path, width: u32, height: u32, fps: u32) -> Result<NegotiatedEncoder> {
		let (level, bitrate) =
			level_for(width, height, fps).context("failed to decide encoding level/bitrate")?;

		let mut controls = V4l2Controls {
			bitrate: SafeExtControl::from_value(bitrate),
			profile: SafeExtControl::from_value(VideoH264Profile::High.into()),
			level: SafeExtControl::from_value(level.into()),
			b_frames: SafeExtControl::from_value(VideoBFrames(0).into()),
			i_period: SafeExtControl::from_value(VideoH264IPeriod(fps as i32 * 2).into()),
			repeat_seq_header: SafeExtControl::from_value(VideoH264RepeatSeqHeader(false).into()),
		};

		let device = open_device(device).context("failed to open encoder")?;

		let output_queue =
			Queue::get_output_mplane_queue(device.clone()).context("failed to get output queue")?;
		let capture_queue = Queue::get_capture_mplane_queue(device.clone())
			.context("failed to get capture queue")?;
		let mut device_fd = device.as_fd();

		// v4l2r's Format builder cannot express field, colorspace or quantization.
		let mut capture_planes =
			[bindings::v4l2_plane_pix_format::default(); bindings::VIDEO_MAX_PLANES as usize];
		capture_planes[0].sizeimage = 1024 * 1024;
		let mut capture_request = bindings::v4l2_format {
			type_: capture_queue.get_type() as u32,
			..Default::default()
		};
		capture_request.fmt.pix_mp = bindings::v4l2_pix_format_mplane {
			width,
			height,
			pixelformat: u32::from_le_bytes(*b"H264"),
			field: bindings::v4l2_field_V4L2_FIELD_NONE,
			plane_fmt: capture_planes,
			num_planes: 1,
			..Default::default()
		};
		let capture_format: Format = v4l2r::ioctl::s_fmt(&mut device_fd, capture_request)
			.context("failed to set capture format")?;

		if capture_format.pixelformat != b"H264".into() {
			bail!("h264 encoding not supported by device");
		}
		let mut output_request = bindings::v4l2_format {
			type_: output_queue.get_type() as u32,
			..Default::default()
		};
		output_request.fmt.pix_mp = bindings::v4l2_pix_format_mplane {
			width,
			height,
			pixelformat: u32::from_le_bytes(*b"YU12"),
			field: bindings::v4l2_field_V4L2_FIELD_NONE,
			colorspace: bindings::v4l2_colorspace_V4L2_COLORSPACE_REC709,
			// bcm2835-codec still needs its SPS VUI patched when collecting encoded data.
			quantization: bindings::v4l2_quantization_V4L2_QUANTIZATION_FULL_RANGE as u8,
			num_planes: 1,
			..Default::default()
		};
		let _: bindings::v4l2_format = v4l2r::ioctl::s_fmt(&mut device_fd, output_request)
			.context("failed to set output format")?;

		let mut output_interval = bindings::v4l2_streamparm {
			type_: output_queue.get_type() as u32,
			..Default::default()
		};
		output_interval.parm.output = bindings::v4l2_outputparm {
			timeperframe: bindings::v4l2_fract {
				numerator: 1,
				denominator: fps,
			},
			..Default::default()
		};
		let output_interval: bindings::v4l2_streamparm =
			v4l2r::ioctl::s_parm(device.as_ref(), output_interval)
				.context("failed to set output frame interval")?;
		validate_output_frame_interval(&output_interval, fps)
			.context("s_parm changed output frame interval")?;

		let target_controls = controls.clone();
		v4l2r::ioctl::s_ext_ctrls(&device, CtrlWhich::Current, &mut controls)
			.context("failed to set controls")?;
		controls
			.ensure_matches(&target_controls)
			.context("s_ext_ctrls changed v4l2 controls")?;
		v4l2r::ioctl::g_ext_ctrls(&device, CtrlWhich::Current, &mut controls)
			.context("failed to get controls")?;
		controls
			.ensure_matches(&target_controls)
			.context("driver changed v4l2 controls")?;

		let output_interval: bindings::v4l2_streamparm =
			v4l2r::ioctl::g_parm(device.as_ref(), output_queue.get_type())
				.context("failed to read back output frame interval")?;
		validate_output_frame_interval(&output_interval, fps)
			.context("driver changed output frame interval")?;

		// Validate final formats after both queues and codec controls are configured.
		let output_format: bindings::v4l2_format = output_queue
			.get_format()
			.context("failed to read back output format")?;
		// This queue uses VIDEO_OUTPUT_MPLANE, so pix_mp is the active union member.
		let output_pix = unsafe { output_format.fmt.pix_mp };
		let params = validate_output_format(output_pix, width, height, fps)?;
		let output_size = output_pix.plane_fmt[0].sizeimage as usize;

		let capture_format: Format = capture_queue
			.get_format()
			.context("failed to read back capture format")?;
		if capture_format.pixelformat != b"H264".into() {
			bail!("h264 encoding not supported by device");
		}
		if capture_format.width != params.width || capture_format.height != params.height {
			bail!(
				"capture dimensions: requested {width}x{height}, driver returned {}x{}",
				capture_format.width,
				capture_format.height
			);
		}
		let [capture_plane] = capture_format.plane_fmt.as_slice() else {
			bail!("H264 capture must have one memory plane");
		};
		let capture_size = capture_plane.sizeimage as usize;
		if capture_size == 0 {
			bail!("H264 capture buffer size must be nonzero");
		}

		Ok(NegotiatedEncoder {
			device,
			output_queue,
			capture_queue,
			params,
			output_size,
			capture_size,
		})
	}

	pub fn probe(device: &Path, width: u32, height: u32, fps: u32) -> Result<VideoParams> {
		Ok(Self::negotiate(device, width, height, fps)?.params)
	}

	pub fn new(device: &Path, expected: VideoParams) -> Result<Self> {
		let NegotiatedEncoder {
			device,
			output_queue,
			capture_queue,
			params,
			output_size,
			capture_size,
		} = Self::negotiate(device, expected.width, expected.height, expected.fps)?;
		params.ensure_matches(&expected)?;

		let output_queue = output_queue
			.request_buffers::<Vec<MmapHandle>>(6)
			.context("failed to allocate output buffers")?;
		let capture_queue = capture_queue
			.request_buffers::<Vec<MmapHandle>>(6)
			.context("failed to allocate capture buffers")?;
		validate_mmap_buffers(
			device.as_ref(),
			output_queue.get_type(),
			output_queue.num_buffers(),
			output_size,
		)?;
		validate_mmap_buffers(
			device.as_ref(),
			capture_queue.get_type(),
			capture_queue.num_buffers(),
			capture_size,
		)?;

		output_queue
			.stream_on()
			.context("failed to enable streaming for output")?;
		capture_queue
			.stream_on()
			.context("failed to enable streaming for capture")?;

		Ok(Self {
			params,

			sps: None,
			pps: None,
			vui: VuiSettings::default(),

			device: AsyncFd::with_interest(device.clone(), Interest::READABLE | Interest::WRITABLE)
				.context("failed to create asyncfd")?,
			input: output_queue,
			output: capture_queue,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn output_interval(numerator: u32, denominator: u32) -> bindings::v4l2_streamparm {
		let mut parm = bindings::v4l2_streamparm {
			type_: bindings::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
			..Default::default()
		};
		parm.parm.output = bindings::v4l2_outputparm {
			timeperframe: bindings::v4l2_fract {
				numerator,
				denominator,
			},
			..Default::default()
		};
		parm
	}

	#[test]
	fn output_frame_interval_accepts_equivalent_fractions() {
		for (numerator, denominator, fps) in
			[(1, 30, 30), (1, 60, 60), (2, 60, 30), (1000, 60_000, 60)]
		{
			assert!(
				validate_output_frame_interval(&output_interval(numerator, denominator), fps)
					.is_ok()
			);
		}
	}

	#[test]
	fn output_frame_interval_rejects_a_different_rate() {
		assert!(validate_output_frame_interval(&output_interval(1, 30), 60).is_err());
		assert!(validate_output_frame_interval(&output_interval(1, 60), 30).is_err());
		// The cross-product must not wrap back to a matching denominator.
		assert!(
			validate_output_frame_interval(&output_interval(u32::MAX, u32::MAX - 1), 2).is_err()
		);
	}

	#[test]
	fn output_frame_interval_rejects_zero_values() {
		for (numerator, denominator) in [(0, 30), (1, 0), (0, 0)] {
			assert!(
				validate_output_frame_interval(&output_interval(numerator, denominator), 30)
					.is_err()
			);
		}
	}

	#[test]
	fn output_frame_interval_rejects_the_wrong_queue() {
		let mut parm = output_interval(1, 30);
		parm.type_ = bindings::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
		assert!(validate_output_frame_interval(&parm, 30).is_err());
	}
}
