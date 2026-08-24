//! The encrypted control channel, as a `tokio-util` codec.
//!
//! §30.3.2.2.1: once pair-verify completes, every control message is split into frames of
//! at most 16384 bytes, each framed as
//!
//! ```text
//! <2 bytes: little endian length of the ciphertext, also the AAD>
//! <n bytes: ChaCha20-Poly1305 ciphertext>
//! <16 bytes: auth tag>
//! ```
//!
//! The frame counters run for the life of the TCP connection and never reset, which is
//! what binds the cipher stream to a single socket: a second connection would have keys
//! but no way to say where in the nonce sequence it sits. That is why the whole control
//! plane has to share one connection.
//!
//! The codec sits underneath hyper, so the `Stream`/`Sink` pair is bridged back into
//! `AsyncRead`/`AsyncWrite` with [`StreamReader`] and [`SinkWriter`].

use std::io;

use bytes::{BufMut, Bytes, BytesMut};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Tag, aead::AeadInPlace};
use tokio::{
	io::{BufWriter, Join, join},
	net::{
		TcpStream,
		tcp::{OwnedReadHalf, OwnedWriteHalf},
	},
};
use tokio_util::{
	codec::{Decoder, Encoder, FramedRead, FramedWrite},
	either::Either,
	io::{SinkWriter, StreamReader},
};

use crate::pairing::keys::{SessionKeys, stream_nonce};

/// The largest ciphertext one frame may carry (§30.3.2.2.1).
///
/// HomeKit's own limit is 1024; CarPlay raises it, so a HAP implementation reused here
/// will interoperate but fragment far more than it needs to.
pub const MAX_FRAME_PAYLOAD: usize = 16384;

/// Length prefix plus auth tag.
const OVERHEAD: usize = 2 + 16;

fn crypto_error(message: &'static str) -> io::Error {
	io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Decrypts inbound frames into a plaintext byte stream.
pub struct ControlDecoder {
	cipher: ChaCha20Poly1305,
	counter: u64,
}

impl ControlDecoder {
	pub fn new(key: &[u8; 32]) -> Self {
		Self {
			cipher: ChaCha20Poly1305::new(key.into()),
			counter: 0,
		}
	}
}

impl Decoder for ControlDecoder {
	type Item = Bytes;
	type Error = io::Error;

	fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
		if src.len() < 2 {
			src.reserve(2 - src.len());
			return Ok(None);
		}

		let payload_len = u16::from_le_bytes([src[0], src[1]]) as usize;
		if payload_len > MAX_FRAME_PAYLOAD {
			return Err(crypto_error("control frame exceeds 16384 bytes"));
		}

		let frame_len = payload_len + OVERHEAD;
		if src.len() < frame_len {
			src.reserve(frame_len - src.len());
			return Ok(None);
		}

		let mut frame = src.split_to(frame_len);
		let aad = [frame[0], frame[1]];
		let mut payload = frame.split_off(2);
		let tag = payload.split_off(payload_len);

		self.cipher
			.decrypt_in_place_detached(
				(&stream_nonce(self.counter)).into(),
				&aad,
				&mut payload,
				Tag::from_slice(&tag),
			)
			// §6.5.2 of HAP: a decryption failure must close the connection immediately.
			// Returning an error here fuses the stream, which is exactly that.
			.map_err(|_| crypto_error("control frame failed authentication"))?;

		self.counter += 1;

		Ok(Some(payload.freeze()))
	}
}

/// Encrypts an outbound plaintext byte stream into frames.
///
/// Implemented over `&[u8]` rather than `Bytes` so that [`SinkWriter`] accepts it
/// directly; its bound is `for<'a> S: Sink<&'a [u8]>`, and going through `Bytes` would
/// need a `CopyToBytes` adapter and an extra copy.
pub struct ControlEncoder {
	cipher: ChaCha20Poly1305,
	counter: u64,
}

impl ControlEncoder {
	pub fn new(key: &[u8; 32]) -> Self {
		Self {
			cipher: ChaCha20Poly1305::new(key.into()),
			counter: 0,
		}
	}
}

impl<'a> Encoder<&'a [u8]> for ControlEncoder {
	type Error = io::Error;

	fn encode(&mut self, item: &'a [u8], dst: &mut BytesMut) -> Result<(), io::Error> {
		for chunk in item.chunks(MAX_FRAME_PAYLOAD) {
			let length = (chunk.len() as u16).to_le_bytes();

			dst.reserve(chunk.len() + OVERHEAD);
			dst.put_slice(&length);

			let start = dst.len();
			dst.put_slice(chunk);

			let tag = self
				.cipher
				.encrypt_in_place_detached(
					(&stream_nonce(self.counter)).into(),
					&length,
					&mut dst[start..],
				)
				.map_err(|_| crypto_error("failed to encrypt control frame"))?;

			dst.put_slice(&tag);
			self.counter += 1;
		}

		Ok(())
	}
}

/// An encrypted control connection, presented as `AsyncRead + AsyncWrite`.
///
/// The [`BufWriter`] is not just an optimisation. Each `poll_write` becomes at least one
/// frame, so without it hyper's separate header and body writes would split a single
/// message across frames. §30.3.2.2.1 describes framing as a message being split into
/// frames, so keeping one message per frame is the conservative reading; buffering to the
/// frame limit and letting hyper's end-of-request flush emit it does that for free.
pub type SecureStream = Join<
	StreamReader<FramedRead<OwnedReadHalf, ControlDecoder>, Bytes>,
	BufWriter<SinkWriter<FramedWrite<OwnedWriteHalf, ControlEncoder>>>,
>;

/// The control connection, before and after pair-verify.
///
/// Pair-setup and pair-verify run in the clear on [`Either::Left`]; everything afterwards
/// runs on [`Either::Right`]. `Either` implements both IO traits when its arms do, so
/// hyper is handed the same type either way.
pub type Transport = Either<TcpStream, SecureStream>;

/// Wrap a socket in the frame codec once pair-verify has produced session keys.
///
/// Splitting into owned halves rather than using `Framed` avoids a `BiLock` and gives each
/// direction its own codec, which is what the protocol wants: the read and write frame
/// counters are independent.
///
/// This must only be called between requests, with nothing buffered above the socket. Any
/// bytes an HTTP layer had already read ahead would be dropped here, and they would be the
/// first encrypted bytes on the wire.
pub fn secure(socket: TcpStream, keys: SessionKeys) -> SecureStream {
	let (read_half, write_half) = socket.into_split();

	let reader = StreamReader::new(FramedRead::new(read_half, ControlDecoder::new(&keys.read)));
	let writer = BufWriter::with_capacity(
		MAX_FRAME_PAYLOAD,
		SinkWriter::new(FramedWrite::new(
			write_half,
			ControlEncoder::new(&keys.write),
		)),
	);

	join(reader, writer)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn keys() -> SessionKeys {
		SessionKeys::control(&[9u8; 32])
	}

	/// The accessory's encoder uses our read key, and vice versa.
	fn peer(keys: SessionKeys) -> (ControlEncoder, ControlDecoder) {
		(
			ControlEncoder::new(&keys.read),
			ControlDecoder::new(&keys.write),
		)
	}

	#[test]
	fn frames_have_the_documented_layout() {
		let keys = keys();
		let mut encoder = ControlEncoder::new(&keys.write);
		let mut buf = BytesMut::new();

		encoder
			.encode(b"RTSP/1.0 200 OK".as_slice(), &mut buf)
			.unwrap();

		assert_eq!(buf.len(), 15 + OVERHEAD);
		assert_eq!(u16::from_le_bytes([buf[0], buf[1]]) as usize, 15);
	}

	#[test]
	fn round_trips_through_the_peer() {
		let keys = keys();
		let mut encoder = ControlEncoder::new(&keys.write);
		let (_, mut decoder) = peer(keys);

		let mut buf = BytesMut::new();
		encoder.encode(b"first".as_slice(), &mut buf).unwrap();
		encoder.encode(b"second".as_slice(), &mut buf).unwrap();

		assert_eq!(decoder.decode(&mut buf).unwrap().unwrap(), &b"first"[..]);
		assert_eq!(decoder.decode(&mut buf).unwrap().unwrap(), &b"second"[..]);
		assert!(decoder.decode(&mut buf).unwrap().is_none());
	}

	#[test]
	fn splits_oversized_writes() {
		let keys = keys();
		let mut encoder = ControlEncoder::new(&keys.write);
		let (_, mut decoder) = peer(keys);

		let message = vec![0xABu8; MAX_FRAME_PAYLOAD + 100];
		let mut buf = BytesMut::new();
		encoder.encode(message.as_slice(), &mut buf).unwrap();

		assert_eq!(buf.len(), message.len() + 2 * OVERHEAD, "two frames");

		let mut received = Vec::new();
		while let Some(chunk) = decoder.decode(&mut buf).unwrap() {
			received.extend_from_slice(&chunk);
		}
		assert_eq!(received, message);
	}

	#[test]
	fn a_partial_frame_yields_nothing_yet() {
		let keys = keys();
		let mut encoder = ControlEncoder::new(&keys.write);
		let (_, mut decoder) = peer(keys);

		let mut buf = BytesMut::new();
		encoder.encode(b"payload".as_slice(), &mut buf).unwrap();

		let mut partial = buf.split_to(buf.len() - 4);
		assert!(decoder.decode(&mut partial).unwrap().is_none());

		partial.unsplit(buf);
		assert_eq!(
			decoder.decode(&mut partial).unwrap().unwrap(),
			&b"payload"[..]
		);
	}

	/// Frames are bound to their position in the stream, so a replayed or reordered frame
	/// must not authenticate.
	#[test]
	fn rejects_a_replayed_frame() {
		let keys = keys();
		let mut encoder = ControlEncoder::new(&keys.write);
		let (_, mut decoder) = peer(keys);

		let mut buf = BytesMut::new();
		encoder.encode(b"first".as_slice(), &mut buf).unwrap();
		let first = buf.clone();
		encoder.encode(b"second".as_slice(), &mut buf).unwrap();

		decoder.decode(&mut buf).unwrap().unwrap();

		let mut replay = first;
		assert!(decoder.decode(&mut replay).is_err());
	}

	#[test]
	fn rejects_a_tampered_tag() {
		let keys = keys();
		let mut encoder = ControlEncoder::new(&keys.write);
		let (_, mut decoder) = peer(keys);

		let mut buf = BytesMut::new();
		encoder.encode(b"payload".as_slice(), &mut buf).unwrap();
		let last = buf.len() - 1;
		buf[last] ^= 0xFF;

		assert!(decoder.decode(&mut buf).is_err());
	}

	#[test]
	fn rejects_an_oversized_length_prefix() {
		let keys = keys();
		let mut decoder = ControlDecoder::new(&keys.read);

		let mut buf = BytesMut::new();
		buf.put_slice(&((MAX_FRAME_PAYLOAD + 1) as u16).to_le_bytes());
		buf.put_slice(&[0u8; 64]);

		assert!(decoder.decode(&mut buf).is_err());
	}
}
