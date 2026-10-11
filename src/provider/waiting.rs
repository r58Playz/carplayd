use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use std::time::Instant;
use v4l2r::ioctl::PlaneMapping;

use crate::{provider::CarplayProviderScreen, v4l2::VideoParams};

const WAITING_YUV: &[u8] = include_bytes!("./waiting.yuv");
const WAITING_W: usize = 800;
const WAITING_H: usize = 480;

fn pad_plane(
	src: &[u8],
	sw: usize,
	sh: usize,
	dst: &mut [u8],
	dst_stride: usize,
	dw: usize,
	dh: usize,
	ox: usize,
	oy: usize,
) {
	for y in 0..dh {
		let sy = y.saturating_sub(oy).min(sh - 1);
		let srow = &src[sy * sw..(sy + 1) * sw];
		let drow = &mut dst[y * dst_stride..y * dst_stride + dw];

		drow[..ox].fill(srow[0]);
		drow[ox..ox + sw].copy_from_slice(srow);
		drow[ox + sw..].fill(srow[sw - 1]);
	}
}

pub struct WaitingProvider(VideoParams, Duration, Instant);
impl WaitingProvider {
	pub fn new(params: VideoParams) -> Self {
		let duration = Duration::from_micros(1_000_000 / params.fps as u64);
		Self(params, duration, Instant::now() - duration)
	}

	fn generate_frame(&self, buffer: &mut PlaneMapping) {
		let p = self.0;

		let (dw, dh) = (p.width as usize, p.height as usize);
		let (stride, rows) = (p.bytes_per_line, p.buffer_height);
		assert!(dw >= WAITING_W && dh >= WAITING_H && dw % 2 == 0 && dh % 2 == 0);
		assert!(stride >= dw && rows >= dh);

		let ox = ((dw - WAITING_W) / 2) & !1;
		let oy = ((dh - WAITING_H) / 2) & !1;

		let (cstride, crows) = (stride / 2, rows / 2);

		let y_len = stride * rows;
		let c_len = cstride * crows;
		let (dst_y, rest) = buffer.split_at_mut(y_len);
		let (dst_u, dst_v) = rest.split_at_mut(c_len);

		let sy_len = WAITING_W * WAITING_H;
		let sc_len = sy_len / 4;
		let (src_y, src_u, src_v) = (
			&WAITING_YUV[..sy_len],
			&WAITING_YUV[sy_len..sy_len + sc_len],
			&WAITING_YUV[sy_len + sc_len..],
		);

		pad_plane(src_y, WAITING_W, WAITING_H, dst_y, stride, dw, dh, ox, oy);
		pad_plane(
			src_u,
			WAITING_W / 2,
			WAITING_H / 2,
			dst_u,
			cstride,
			dw / 2,
			dh / 2,
			ox / 2,
			oy / 2,
		);
		pad_plane(
			src_v,
			WAITING_W / 2,
			WAITING_H / 2,
			dst_v,
			cstride,
			dw / 2,
			dh / 2,
			ox / 2,
			oy / 2,
		);
	}
}
#[async_trait]
impl CarplayProviderScreen for WaitingProvider {
	async fn next_frame(&mut self, buffer: &mut PlaneMapping) -> Result<Option<Instant>> {
		tokio::time::sleep_until(self.2.into()).await;
		let now = Instant::now();
		self.2 = now + self.1;

		self.generate_frame(buffer);

		Ok(Some(now))
	}
}
