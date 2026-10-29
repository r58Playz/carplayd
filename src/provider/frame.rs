//! vibecoded
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use v4l2r::ioctl::PlaneMapping;

use crate::v4l2::VideoParams;

/// Layout of a contiguous YU12 mapping. U and V have half the Y stride and rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Yuv420Layout {
	pub width: u32,
	pub height: u32,
	pub bytes_per_line: usize,
	pub buffer_height: usize,
}

impl From<VideoParams> for Yuv420Layout {
	fn from(params: VideoParams) -> Self {
		Self {
			width: params.width,
			height: params.height,
			bytes_per_line: params.bytes_per_line,
			buffer_height: params.buffer_height,
		}
	}
}

impl Yuv420Layout {
	fn params(self) -> VideoParams {
		VideoParams {
			width: self.width,
			height: self.height,
			fps: 1,
			bytes_per_line: self.bytes_per_line,
			buffer_height: self.buffer_height,
		}
	}

	pub(crate) fn validate(self, buffer_len: usize) -> Result<()> {
		self.params().validate(buffer_len)
	}
}

/// Coordinates in pixels. All fields must be even for a YUV 4:2:0 view.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FrameCrop {
	pub left: u32,
	pub top: u32,
	pub width: u32,
	pub height: u32,
}

impl FrameCrop {
	pub(crate) fn validate(self, width: u32, height: u32) -> Result<()> {
		ensure!(self.width != 0 && self.height != 0, "empty YU12 crop");
		ensure!(
			[self.left, self.top, self.width, self.height]
				.into_iter()
				.all(|n| n.is_multiple_of(2)),
			"YU12 crop coordinates and dimensions must be even"
		);
		let right = self
			.left
			.checked_add(self.width)
			.context("crop right overflow")?;
		let bottom = self
			.top
			.checked_add(self.height)
			.context("crop bottom overflow")?;
		ensure!(
			right <= width && bottom <= height,
			"YU12 crop exceeds the frame"
		);
		Ok(())
	}
}

/// Read-only view over a mapped frame. Cropping changes offsets, without copying.
/// The mapping stays borrowed until the view is consumed; capture memory is never modified.
#[derive(Clone, Copy, Debug)]
pub struct Yuv420Frame<'a> {
	data: &'a [u8],
	layout: Yuv420Layout,
	crop: FrameCrop,
}

impl<'a> Yuv420Frame<'a> {
	pub fn from_mapping(mapping: &'a PlaneMapping, layout: Yuv420Layout) -> Result<Self> {
		Self::from_bytes(mapping, layout)
	}

	pub fn from_bytes(data: &'a [u8], layout: Yuv420Layout) -> Result<Self> {
		layout.validate(data.len())?;
		Ok(Self {
			data,
			layout,
			crop: FrameCrop {
				left: 0,
				top: 0,
				width: layout.width,
				height: layout.height,
			},
		})
	}

	pub fn width(self) -> u32 {
		self.crop.width
	}

	pub fn height(self) -> u32 {
		self.crop.height
	}

	/// Crop relative to this view, including when it already has the decoder's visible crop.
	pub fn crop(self, crop: FrameCrop) -> Result<Self> {
		crop.validate(self.width(), self.height())?;
		Ok(Self {
			crop: FrameCrop {
				left: self
					.crop
					.left
					.checked_add(crop.left)
					.context("crop left overflow")?,
				top: self
					.crop
					.top
					.checked_add(crop.top)
					.context("crop top overflow")?,
				..crop
			},
			..self
		})
	}

	fn row(self, plane: usize, row: usize) -> &'a [u8] {
		let y_size = self.layout.bytes_per_line * self.layout.buffer_height;
		let (offset, stride, scale) = match plane {
			0 => (0, self.layout.bytes_per_line, 1),
			1 => (y_size, self.layout.bytes_per_line / 2, 2),
			2 => (y_size + y_size / 4, self.layout.bytes_per_line / 2, 2),
			_ => unreachable!(),
		};
		let left = self.crop.left as usize / scale;
		let top = self.crop.top as usize / scale;
		let width = self.width() as usize / scale;
		let start = offset + (top + row) * stride + left;
		&self.data[start..start + width]
	}

	fn rows(self, plane: usize) -> impl ExactSizeIterator<Item = &'a [u8]> {
		let scale = if plane == 0 { 1 } else { 2 };
		(0..self.height() as usize / scale).map(move |row| self.row(plane, row))
	}

	pub fn y_rows(self) -> impl ExactSizeIterator<Item = &'a [u8]> {
		self.rows(0)
	}

	pub fn u_rows(self) -> impl ExactSizeIterator<Item = &'a [u8]> {
		self.rows(1)
	}

	pub fn v_rows(self) -> impl ExactSizeIterator<Item = &'a [u8]> {
		self.rows(2)
	}

	/// Copy visible Y/U/V rows into the encoder's negotiated layout. Padding in both
	/// mappings is skipped, and destination padding is left untouched. No scaling or
	/// color conversion is performed. Dimension/layout errors are checked before writing.
	pub fn copy_to(self, destination: &mut [u8], params: VideoParams) -> Result<()> {
		params.validate(destination.len())?;
		ensure!(
			self.width() == params.width && self.height() == params.height,
			"YU12 view is {}x{}, destination is {}x{}; crop before copying",
			self.width(),
			self.height(),
			params.width,
			params.height
		);
		self.copy_at(destination, params, 0, 0)
	}

	/// Center this view in the destination, cropping larger dimensions and extending
	/// edge pixels for smaller ones. Offsets round down to even pixels for chroma
	/// alignment, so opposite margins can differ by two pixels. Storage padding is
	/// left untouched; this operates on visible pixels without scaling or allocation.
	pub fn copy_centered_to(self, destination: &mut [u8], params: VideoParams) -> Result<()> {
		params.validate(destination.len())?;
		let crop = FrameCrop {
			left: (self.width().saturating_sub(params.width) / 2) & !1,
			top: (self.height().saturating_sub(params.height) / 2) & !1,
			width: self.width().min(params.width),
			height: self.height().min(params.height),
		};
		let left = (params.width.saturating_sub(self.width()) / 2) & !1;
		let top = (params.height.saturating_sub(self.height()) / 2) & !1;
		self.crop(crop)?
			.copy_at(destination, params, left as usize, top as usize)
	}

	/// The caller validates the destination and ensures that this view fits at the
	/// supplied, chroma-aligned position before any plane is written.
	fn copy_at(
		self,
		destination: &mut [u8],
		params: VideoParams,
		left: usize,
		top: usize,
	) -> Result<()> {
		let y_size = params.bytes_per_line * params.buffer_height;
		let (y, chroma) = destination[..params.frame_size()?].split_at_mut(y_size);
		let (u, v) = chroma.split_at_mut(y_size / 4);
		for (plane, dst, stride, scale) in [
			(0, y, params.bytes_per_line, 1),
			(1, u, params.bytes_per_line / 2, 2),
			(2, v, params.bytes_per_line / 2, 2),
		] {
			let (left, top) = (left / scale, top / scale);
			let width = params.width as usize / scale;
			let height = params.height as usize / scale;
			let source_height = self.height() as usize / scale;
			for (row, dst) in dst.chunks_exact_mut(stride).take(height).enumerate() {
				let source_row = row.saturating_sub(top).min(source_height - 1);
				let src = self.row(plane, source_row);
				let dst = &mut dst[..width];
				dst[..left].fill(src[0]);
				dst[left..left + src.len()].copy_from_slice(src);
				dst[left + src.len()..].fill(src[src.len() - 1]);
			}
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn source() -> (Vec<u8>, Yuv420Layout) {
		let layout = Yuv420Layout {
			width: 8,
			height: 6,
			bytes_per_line: 12,
			buffer_height: 8,
		};
		let mut data = vec![0xee; 144];
		for (offset, stride, width, height, base) in
			[(0, 12, 8, 6, 0), (96, 6, 4, 3, 80), (120, 6, 4, 3, 160)]
		{
			for row in 0..height {
				for col in 0..width {
					data[offset + row * stride + col] = (base + row * width + col) as u8;
				}
			}
		}
		(data, layout)
	}

	#[test]
	fn copy_skips_source_and_destination_padding() {
		let (data, layout) = source();
		let params = VideoParams {
			width: 8,
			height: 6,
			fps: 30,
			bytes_per_line: 10,
			buffer_height: 10,
		};
		let mut dst = vec![0xdd; 153];
		Yuv420Frame::from_bytes(&data, layout)
			.unwrap()
			.copy_to(&mut dst, params)
			.unwrap();
		let mut expected = vec![0xdd; 153];
		for (offset, stride, width, height, base) in
			[(0, 10, 8, 6, 0), (100, 5, 4, 3, 80), (125, 5, 4, 3, 160)]
		{
			for row in 0..height {
				for col in 0..width {
					expected[offset + row * stride + col] = (base + row * width + col) as u8;
				}
			}
		}
		assert_eq!(dst, expected);
	}

	#[test]
	fn crop_is_relative_and_copies_all_three_planes() {
		let (data, layout) = source();
		let frame = Yuv420Frame::from_bytes(&data, layout)
			.unwrap()
			.crop(FrameCrop {
				left: 2,
				top: 2,
				width: 6,
				height: 4,
			})
			.unwrap()
			.crop(FrameCrop {
				left: 2,
				top: 0,
				width: 4,
				height: 2,
			})
			.unwrap();
		// The view still points straight into the original mapping.
		assert_eq!(frame.y_rows().next().unwrap().as_ptr(), data[28..].as_ptr());
		let mut dst = [0; 12];
		frame
			.copy_to(
				&mut dst,
				VideoParams {
					width: 4,
					height: 2,
					fps: 30,
					bytes_per_line: 4,
					buffer_height: 2,
				},
			)
			.unwrap();
		assert_eq!(dst, [20, 21, 22, 23, 28, 29, 30, 31, 86, 87, 166, 167]);
	}

	#[test]
	fn centered_copy_pads_cropped_edges_and_preserves_storage_padding() {
		let (data, layout) = source();
		let frame = Yuv420Frame::from_bytes(&data, layout)
			.unwrap()
			.crop(FrameCrop {
				left: 2,
				top: 2,
				width: 4,
				height: 2,
			})
			.unwrap();
		let params = VideoParams {
			width: 10,
			height: 8,
			fps: 30,
			bytes_per_line: 12,
			buffer_height: 10,
		};
		let mut dst = vec![0xdd; 183];
		frame.copy_centered_to(&mut dst, params).unwrap();
		let mut expected = vec![0xdd; 183];
		// Nominal margins of three round down to two for Y and one for U/V.
		for row in 0..8 {
			let pixels = if row < 3 {
				[18, 18, 18, 19, 20, 21, 21, 21, 21, 21]
			} else {
				[26, 26, 26, 27, 28, 29, 29, 29, 29, 29]
			};
			expected[row * 12..row * 12 + 10].copy_from_slice(&pixels);
		}
		for row in 0..4 {
			expected[120 + row * 6..125 + row * 6].copy_from_slice(&[85, 85, 86, 86, 86]);
			expected[150 + row * 6..155 + row * 6].copy_from_slice(&[165, 165, 166, 166, 166]);
		}
		assert_eq!(dst, expected);
	}

	#[test]
	fn centered_copy_crops_larger_frames_on_chroma_boundaries() {
		let (data, layout) = source();
		let frame = Yuv420Frame::from_bytes(&data, layout).unwrap();
		let mut dst = [0; 12];
		frame
			.copy_centered_to(
				&mut dst,
				VideoParams {
					width: 4,
					height: 2,
					fps: 30,
					bytes_per_line: 4,
					buffer_height: 2,
				},
			)
			.unwrap();
		assert_eq!(dst, [18, 19, 20, 21, 26, 27, 28, 29, 85, 86, 165, 166]);
		// A one-pixel center offset rounds down to zero, rather than misaligning chroma.
		let mut dst = [0; 36];
		frame
			.copy_centered_to(
				&mut dst,
				VideoParams {
					width: 6,
					height: 4,
					fps: 30,
					bytes_per_line: 6,
					buffer_height: 4,
				},
			)
			.unwrap();
		assert_eq!(
			dst,
			[
				0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20, 21, 24, 25, 26, 27, 28,
				29, 80, 81, 82, 84, 85, 86, 160, 161, 162, 164, 165, 166,
			]
		);
	}

	#[test]
	fn centered_copy_can_crop_one_axis_while_padding_the_other() {
		let (data, layout) = source();
		let frame = Yuv420Frame::from_bytes(&data, layout).unwrap();
		let mut dst = [0; 60];
		frame
			.copy_centered_to(
				&mut dst,
				VideoParams {
					width: 4,
					height: 10,
					fps: 30,
					bytes_per_line: 4,
					buffer_height: 10,
				},
			)
			.unwrap();
		assert_eq!(
			dst,
			[
				2, 3, 4, 5, 2, 3, 4, 5, 2, 3, 4, 5, 10, 11, 12, 13, 18, 19, 20, 21, 26, 27, 28, 29,
				34, 35, 36, 37, 42, 43, 44, 45, 42, 43, 44, 45, 42, 43, 44, 45, 81, 82, 81, 82, 85,
				86, 89, 90, 89, 90, 161, 162, 161, 162, 165, 166, 169, 170, 169, 170,
			]
		);
		let mut dst = [0; 30];
		frame
			.copy_centered_to(
				&mut dst,
				VideoParams {
					width: 10,
					height: 2,
					fps: 30,
					bytes_per_line: 10,
					buffer_height: 2,
				},
			)
			.unwrap();
		assert_eq!(
			dst,
			[
				16, 17, 18, 19, 20, 21, 22, 23, 23, 23, 24, 25, 26, 27, 28, 29, 30, 31, 31, 31, 84,
				85, 86, 87, 87, 164, 165, 166, 167, 167,
			]
		);
	}

	#[test]
	fn centered_copy_rejects_invalid_destinations_before_writing() {
		let (data, layout) = source();
		let frame = Yuv420Frame::from_bytes(&data, layout).unwrap();
		let mut dst = [0xdd; 72];
		for params in [
			VideoParams {
				width: 3,
				height: 2,
				fps: 30,
				bytes_per_line: 4,
				buffer_height: 2,
			},
			VideoParams {
				width: 4,
				height: 2,
				fps: 30,
				bytes_per_line: 3,
				buffer_height: 2,
			},
			VideoParams {
				width: 8,
				height: 6,
				fps: 30,
				bytes_per_line: 10,
				buffer_height: 6,
			},
			VideoParams {
				width: 2,
				height: 2,
				fps: 30,
				bytes_per_line: usize::MAX - 1,
				buffer_height: 2,
			},
		] {
			assert!(frame.copy_centered_to(&mut dst, params).is_err());
			assert_eq!(dst, [0xdd; 72]);
		}
	}

	#[test]
	fn rejects_unaligned_empty_out_of_bounds_and_overflowing_crops() {
		let (data, layout) = source();
		let frame = Yuv420Frame::from_bytes(&data, layout).unwrap();
		for crop in [
			FrameCrop {
				left: 1,
				top: 0,
				width: 2,
				height: 2,
			},
			FrameCrop {
				left: 0,
				top: 1,
				width: 2,
				height: 2,
			},
			FrameCrop {
				left: 0,
				top: 0,
				width: 3,
				height: 2,
			},
			FrameCrop {
				left: 0,
				top: 0,
				width: 2,
				height: 3,
			},
			FrameCrop {
				left: 0,
				top: 0,
				width: 0,
				height: 2,
			},
			FrameCrop {
				left: 8,
				top: 0,
				width: 2,
				height: 2,
			},
			FrameCrop {
				left: 0,
				top: 6,
				width: 2,
				height: 2,
			},
			FrameCrop {
				left: u32::MAX - 1,
				top: 0,
				width: 2,
				height: 2,
			},
		] {
			assert!(frame.crop(crop).is_err(), "accepted {crop:?}");
		}
	}

	#[test]
	fn validates_layout_and_destination_before_copying() {
		let (data, layout) = source();
		assert!(Yuv420Frame::from_bytes(&data[..143], layout).is_err());
		assert!(
			Yuv420Frame::from_bytes(
				&data,
				Yuv420Layout {
					bytes_per_line: 6,
					..layout
				}
			)
			.is_err()
		);
		assert!(
			Yuv420Frame::from_bytes(
				&data,
				Yuv420Layout {
					buffer_height: 4,
					..layout
				}
			)
			.is_err()
		);
		assert!(
			Yuv420Frame::from_bytes(
				&data,
				Yuv420Layout {
					bytes_per_line: usize::MAX - 1,
					..layout
				}
			)
			.is_err()
		);
		let frame = Yuv420Frame::from_bytes(&data, layout).unwrap();
		let mut dst = [0xdd; 72];
		for params in [
			VideoParams {
				width: 4,
				height: 6,
				fps: 30,
				bytes_per_line: 4,
				buffer_height: 6,
			},
			VideoParams {
				width: 8,
				height: 6,
				fps: 30,
				bytes_per_line: 10,
				buffer_height: 6,
			},
		] {
			assert!(frame.copy_to(&mut dst, params).is_err());
			assert_eq!(dst, [0xdd; 72]);
		}
	}
}
