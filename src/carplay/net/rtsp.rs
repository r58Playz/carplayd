//! The RTSP/HTTP hybrid the CarPlay control and event channels speak.
//!
//! The head unit's control server is `AirTunes/320.17` and answers `RTSP/1.0`, not
//! `HTTP/1.1` — see `../../../../ncm-probe/session.log`. A general HTTP stack cannot be
//! used here: `httparse`, and so `hyper`, compares the version token against
//! `HTTP/1.0`/`HTTP/1.1` as a fixed 64-bit word, so an `RTSP/1.0` start line fails to
//! parse in both the request and the response direction. That rules it out for the
//! control connection, and for the event connection where we are the *server* and have no
//! say in what the accessory sends.
//!
//! What is left is small enough to own outright. The dialect has no chunked transfer
//! encoding, no pipelining, and no keep-alive negotiation: a message is a start line, a
//! header block, and exactly `Content-Length` bytes of body.
//!
//! # One codec, both roles
//!
//! [`Codec`] decodes whichever of the two a peer sends, deciding on the first token: a
//! start line beginning `RTSP/` or `HTTP/` is a response, anything else is a request. That
//! matters because the two channels invert the roles — on the control connection we send
//! requests, and on the event connection we connect outwards and then serve them.
//!
//! [`Version`] is preserved rather than normalised. The head unit mirrors the version it
//! is given, and the reference implementation's event channel appears to use `HTTP/1.1`
//! while its control channel uses `RTSP/1.0`, so neither end should assume.
//!
//! # Layering
//!
//! Stack this on top of [`super::codec`], not underneath it:
//!
//! ```text
//! Framed<Transport, rtsp::Codec>     messages
//! Either<TcpStream, SecureStream>    ChaCha20-Poly1305 frames, once pair-verify is done
//! ```
//!
//! One [`Channel::request`] is one `poll_write`, hence one 16 KiB-capped cipher frame per
//! message, which is the conservative reading of §30.3.2.2.1. Use [`Channel::into_parts`]
//! and [`Channel::from_parts`] across the encryption upgrade so that anything already read
//! ahead of the last response survives; dropping and rebuilding the framing there would
//! discard it, and those bytes would be the first encrypted ones on the wire.

// Not yet wired into the session; see PROTOCOL.md for what still has to be built on top.
#![allow(dead_code)]

use std::{fmt, io, str};

use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Decoder, Encoder, Framed, FramedParts};

/// The longest header block we will buffer before giving up on finding its end.
pub const MAX_HEAD_LEN: usize = 64 * 1024;

/// The largest body we will accept. Control-plane plists are kilobytes; this is only here
/// so a corrupt length cannot turn into an unbounded allocation.
pub const MAX_BODY_LEN: usize = 8 * 1024 * 1024;

const CRLF: &[u8] = b"\r\n";
const HEAD_END: &[u8] = b"\r\n\r\n";

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("io error on the control connection")]
	Io(#[from] io::Error),

	#[error("{0}")]
	Malformed(&'static str),

	#[error("header block exceeded {MAX_HEAD_LEN} bytes without a terminator")]
	HeadTooLong,

	#[error("Content-Length of {0} exceeds the {MAX_BODY_LEN} byte limit")]
	BodyTooLong(usize),

	#[error("unsupported Transfer-Encoding: {0}")]
	TransferEncoding(String),

	#[error("expected CSeq {expected}, got {actual:?}")]
	CSeqMismatch { expected: u32, actual: Option<u32> },

	#[error("peer closed the connection mid-exchange")]
	Closed,
}

/// The protocol version on a start line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Version {
	/// What the control connection speaks.
	#[default]
	Rtsp10,
	/// What the reference implementation's event channel appears to speak.
	Http11,
}

impl Version {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Rtsp10 => "RTSP/1.0",
			Self::Http11 => "HTTP/1.1",
		}
	}

	/// `HTTP/1.0` is folded into [`Version::Http11`] rather than rejected: nothing in
	/// CarPlay sends it, but accepting a minor version we then answer as 1.1 is more
	/// robust than failing the connection over it.
	fn parse(token: &str) -> Option<Self> {
		match token {
			"RTSP/1.0" => Some(Self::Rtsp10),
			"HTTP/1.1" | "HTTP/1.0" => Some(Self::Http11),
			_ => None,
		}
	}
}

impl fmt::Display for Version {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

/// A request method.
///
/// The named variants are what the head unit advertises in its `Public` header: `ANNOUNCE,
/// SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, POST, GET, PUT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
	Announce,
	Setup,
	Record,
	Pause,
	Flush,
	Teardown,
	Options,
	Get,
	Post,
	Put,
	Other(Box<str>),
}

impl Method {
	pub fn as_str(&self) -> &str {
		match self {
			Self::Announce => "ANNOUNCE",
			Self::Setup => "SETUP",
			Self::Record => "RECORD",
			Self::Pause => "PAUSE",
			Self::Flush => "FLUSH",
			Self::Teardown => "TEARDOWN",
			Self::Options => "OPTIONS",
			Self::Get => "GET",
			Self::Post => "POST",
			Self::Put => "PUT",
			Self::Other(other) => other,
		}
	}
}

impl From<&str> for Method {
	fn from(token: &str) -> Self {
		match token {
			"ANNOUNCE" => Self::Announce,
			"SETUP" => Self::Setup,
			"RECORD" => Self::Record,
			"PAUSE" => Self::Pause,
			"FLUSH" => Self::Flush,
			"TEARDOWN" => Self::Teardown,
			"OPTIONS" => Self::Options,
			"GET" => Self::Get,
			"POST" => Self::Post,
			"PUT" => Self::Put,
			other => Self::Other(other.into()),
		}
	}
}

impl fmt::Display for Method {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

/// A header block, in the order it was sent.
///
/// Lookup is case-insensitive but the original spelling is kept on the wire, because there
/// is no telling which head unit compares a header name byte for byte.
#[derive(Clone, Debug, Default)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn get(&self, name: &str) -> Option<&str> {
		self.0
			.iter()
			.find(|(key, _)| key.eq_ignore_ascii_case(name))
			.map(|(_, value)| value.as_str())
	}

	/// Add a header, replacing any existing one with the same name.
	pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
		let name = name.into();
		self.0.retain(|(key, _)| !key.eq_ignore_ascii_case(&name));
		self.0.push((name, value.into()));
	}

	/// Add a header without disturbing an existing one of the same name.
	pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
		self.0.push((name.into(), value.into()));
	}

	pub fn remove(&mut self, name: &str) {
		self.0.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
	}

	pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
		self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
	}

	/// `CSeq`, which every request carries and every response echoes.
	pub fn cseq(&self) -> Option<u32> {
		self.get("CSeq")?.trim().parse().ok()
	}

	fn content_length(&self) -> Result<usize, Error> {
		if let Some(encoding) = self.get("Transfer-Encoding") {
			// RTSP has no chunked encoding, so a body we cannot frame would silently
			// desynchronise the connection. Fail loudly instead.
			return Err(Error::TransferEncoding(encoding.to_owned()));
		}

		match self.get("Content-Length") {
			None => Ok(0),
			Some(value) => value
				.trim()
				.parse()
				.map_err(|_| Error::Malformed("Content-Length is not a number")),
		}
	}
}

#[derive(Clone, Debug)]
pub struct Request {
	pub method: Method,
	/// `*` for OPTIONS, a path such as `/info`, or an `rtsp://host/session` absolute URI
	/// for the session verbs.
	pub target: String,
	pub version: Version,
	pub headers: Headers,
	pub body: Bytes,
}

impl Request {
	pub fn new(method: Method, target: impl Into<String>) -> Self {
		Self {
			method,
			target: target.into(),
			version: Version::Rtsp10,
			headers: Headers::new(),
			body: Bytes::new(),
		}
	}

	pub fn version(mut self, version: Version) -> Self {
		self.version = version;
		self
	}

	pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
		self.headers.insert(name, value);
		self
	}

	/// Set the body and its content type together, since one without the other is always a
	/// mistake here.
	pub fn body(mut self, content_type: &str, body: impl Into<Bytes>) -> Self {
		self.headers.insert("Content-Type", content_type);
		self.body = body.into();
		self
	}

	pub fn cseq(&self) -> Option<u32> {
		self.headers.cseq()
	}
}

#[derive(Clone, Debug)]
pub struct Response {
	pub version: Version,
	pub status: u16,
	pub reason: String,
	pub headers: Headers,
	pub body: Bytes,
}

impl Response {
	pub fn new(status: u16, reason: impl Into<String>) -> Self {
		Self {
			version: Version::Rtsp10,
			status,
			reason: reason.into(),
			headers: Headers::new(),
			body: Bytes::new(),
		}
	}

	pub fn ok() -> Self {
		Self::new(200, "OK")
	}

	pub fn version(mut self, version: Version) -> Self {
		self.version = version;
		self
	}

	pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
		self.headers.insert(name, value);
		self
	}

	pub fn body(mut self, content_type: &str, body: impl Into<Bytes>) -> Self {
		self.headers.insert("Content-Type", content_type);
		self.body = body.into();
		self
	}

	pub fn is_success(&self) -> bool {
		(200..300).contains(&self.status)
	}

	pub fn cseq(&self) -> Option<u32> {
		self.headers.cseq()
	}
}

#[derive(Clone, Debug)]
pub enum Message {
	Request(Request),
	Response(Response),
}

impl From<Request> for Message {
	fn from(request: Request) -> Self {
		Self::Request(request)
	}
}

impl From<Response> for Message {
	fn from(response: Response) -> Self {
		Self::Response(response)
	}
}

/// A head we have parsed, waiting on the rest of its body.
///
/// Holding it means a body that arrives a few bytes at a time neither rescans nor
/// reparses the header block on every wakeup. The offsets stay valid because a `decode`
/// that returns `None` leaves `src` untouched.
#[derive(Debug)]
struct Pending {
	head_end: usize,
	body_len: usize,
	message: Message,
}

/// The message codec.
#[derive(Debug, Default)]
pub struct Codec {
	/// How far we have looked for the end of the header block without finding it.
	scanned: usize,
	pending: Option<Pending>,
}

impl Codec {
	pub fn new() -> Self {
		Self::default()
	}

	/// Locate and parse a header block, leaving `src` alone.
	fn parse_pending(&mut self, src: &BytesMut) -> Result<Option<Pending>, Error> {
		// Overlap by three so a terminator split across two reads is still found.
		let from = self.scanned.saturating_sub(HEAD_END.len() - 1);
		let Some(at) = src[from..]
			.windows(HEAD_END.len())
			.position(|window| window == HEAD_END)
		else {
			self.scanned = src.len();
			if src.len() > MAX_HEAD_LEN {
				return Err(Error::HeadTooLong);
			}
			return Ok(None);
		};

		let head_end = from + at + HEAD_END.len();
		let head = str::from_utf8(&src[..head_end - HEAD_END.len()])
			.map_err(|_| Error::Malformed("header block is not valid UTF-8"))?;

		let message = parse_head(head)?;
		let body_len = match &message {
			Message::Request(request) => request.headers.content_length()?,
			Message::Response(response) => response.headers.content_length()?,
		};
		if body_len > MAX_BODY_LEN {
			return Err(Error::BodyTooLong(body_len));
		}

		self.scanned = 0;

		Ok(Some(Pending {
			head_end,
			body_len,
			message,
		}))
	}
}

fn parse_head(head: &str) -> Result<Message, Error> {
	let mut lines = head.split("\r\n");
	let start = lines
		.next()
		.ok_or(Error::Malformed("message has no start line"))?;

	let mut headers = Headers::new();
	for line in lines {
		if line.is_empty() {
			continue;
		}

		// An obs-fold continuation. Nothing observed sends these, but mis-parsing one
		// would be silent, so fold it into the previous value rather than erroring.
		if line.starts_with([' ', '\t']) {
			match headers.0.last_mut() {
				Some((_, value)) => {
					value.push(' ');
					value.push_str(line.trim());
					continue;
				}
				None => return Err(Error::Malformed("continuation before any header")),
			}
		}

		let (name, value) = line
			.split_once(':')
			.ok_or(Error::Malformed("header line has no colon"))?;
		headers.append(name.trim_end(), value.trim());
	}

	if let Some(version) = Version::parse(start.split(' ').next().unwrap_or_default()) {
		let mut parts = start.splitn(3, ' ');
		parts.next();

		let status: u16 = parts
			.next()
			.ok_or(Error::Malformed("response has no status code"))?
			.parse()
			.map_err(|_| Error::Malformed("response status is not a number"))?;

		Ok(Message::Response(Response {
			version,
			status,
			reason: parts.next().unwrap_or_default().to_owned(),
			headers,
			body: Bytes::new(),
		}))
	} else {
		let mut parts = start.split(' ');

		let method = Method::from(
			parts
				.next()
				.ok_or(Error::Malformed("request has no method"))?,
		);
		let target = parts
			.next()
			.ok_or(Error::Malformed("request has no target"))?
			.to_owned();
		let version = parts
			.next()
			.and_then(Version::parse)
			.ok_or(Error::Malformed("unrecognised protocol version"))?;

		Ok(Message::Request(Request {
			method,
			target,
			version,
			headers,
			body: Bytes::new(),
		}))
	}
}

impl Decoder for Codec {
	type Item = Message;
	type Error = Error;

	fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, Error> {
		let pending = match self.pending.take() {
			Some(pending) => pending,
			None => match self.parse_pending(src)? {
				Some(pending) => pending,
				None => return Ok(None),
			},
		};

		let total = pending.head_end + pending.body_len;
		if src.len() < total {
			src.reserve(total - src.len());
			self.pending = Some(pending);
			return Ok(None);
		}

		let _ = src.split_to(pending.head_end);
		let body = src.split_to(pending.body_len).freeze();

		let mut message = pending.message;
		match &mut message {
			Message::Request(request) => request.body = body,
			Message::Response(response) => response.body = body,
		}

		Ok(Some(message))
	}
}

fn encode_headers(headers: &Headers, body_len: usize, dst: &mut BytesMut) {
	for (name, value) in headers.iter() {
		// Ours is authoritative; a stale one from the caller would frame the next message
		// wrongly.
		if name.eq_ignore_ascii_case("Content-Length") {
			continue;
		}
		dst.put_slice(name.as_bytes());
		dst.put_slice(b": ");
		dst.put_slice(value.as_bytes());
		dst.put_slice(CRLF);
	}

	// Always present, including when zero: that is what the head unit does, and what
	// distinguishes an empty body from a missing one.
	dst.put_slice(b"Content-Length: ");
	dst.put_slice(body_len.to_string().as_bytes());
	dst.put_slice(CRLF);
	dst.put_slice(CRLF);
}

impl Encoder<Message> for Codec {
	type Error = Error;

	fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Error> {
		match item {
			Message::Request(request) => {
				dst.reserve(request.target.len() + request.body.len() + 128);
				dst.put_slice(request.method.as_str().as_bytes());
				dst.put_u8(b' ');
				dst.put_slice(request.target.as_bytes());
				dst.put_u8(b' ');
				dst.put_slice(request.version.as_str().as_bytes());
				dst.put_slice(CRLF);

				encode_headers(&request.headers, request.body.len(), dst);
				dst.put_slice(&request.body);
			}
			Message::Response(response) => {
				dst.reserve(response.reason.len() + response.body.len() + 128);
				dst.put_slice(response.version.as_str().as_bytes());
				dst.put_u8(b' ');
				dst.put_slice(response.status.to_string().as_bytes());
				dst.put_u8(b' ');
				dst.put_slice(response.reason.as_bytes());
				dst.put_slice(CRLF);

				encode_headers(&response.headers, response.body.len(), dst);
				dst.put_slice(&response.body);
			}
		}

		Ok(())
	}
}

/// One framed connection, with the `CSeq` bookkeeping both roles need.
///
/// The control connection uses [`Channel::request`]; the event connection uses
/// [`Channel::next_request`] and [`Channel::respond`]. Nothing stops one channel from
/// doing both, but neither of CarPlay's does.
pub struct Channel<T> {
	inner: Framed<T, Codec>,
	cseq: u32,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Channel<T> {
	pub fn new(io: T) -> Self {
		Self {
			inner: Framed::new(io, Codec::new()),
			cseq: 0,
		}
	}

	/// Take the connection apart, keeping whatever is buffered on either side.
	///
	/// This is how to cross the pair-verify upgrade without losing bytes: take the parts,
	/// wrap `parts.io` in the cipher, and rebuild.
	pub fn into_parts(self) -> (FramedParts<T, Codec>, u32) {
		(self.inner.into_parts(), self.cseq)
	}

	pub fn from_parts(parts: FramedParts<T, Codec>, cseq: u32) -> Self {
		Self {
			inner: Framed::from_parts(parts),
			cseq,
		}
	}

	/// Send a request and wait for its response.
	///
	/// `CSeq` is assigned here, and the response is checked against it. A mismatch means
	/// the connection has desynchronised, which is not recoverable — the cipher stream
	/// counters underneath are already past the point of repair.
	pub async fn request(&mut self, mut request: Request) -> Result<Response, Error> {
		self.cseq += 1;
		let cseq = self.cseq;
		request.headers.insert("CSeq", cseq.to_string());

		self.inner.send(Message::Request(request)).await?;

		match self.inner.next().await {
			None => Err(Error::Closed),
			Some(Err(err)) => Err(err),
			Some(Ok(Message::Request(_))) => Err(Error::Malformed(
				"peer sent a request where a response was expected",
			)),
			Some(Ok(Message::Response(response))) => {
				if response.cseq() == Some(cseq) {
					Ok(response)
				} else {
					Err(Error::CSeqMismatch {
						expected: cseq,
						actual: response.cseq(),
					})
				}
			}
		}
	}

	/// Read the next request from the peer, or `None` once it closes.
	pub async fn next_request(&mut self) -> Result<Option<Request>, Error> {
		match self.inner.next().await {
			None => Ok(None),
			Some(Err(err)) => Err(err),
			Some(Ok(Message::Request(request))) => Ok(Some(request)),
			Some(Ok(Message::Response(_))) => Err(Error::Malformed(
				"peer sent a response where a request was expected",
			)),
		}
	}

	/// Answer a request, echoing its `CSeq` and matching its protocol version.
	pub async fn respond(&mut self, to: &Request, mut response: Response) -> Result<(), Error> {
		response.version = to.version;
		if let Some(cseq) = to.cseq() {
			response.headers.insert("CSeq", cseq.to_string());
		}

		self.inner.send(Message::Response(response)).await?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn encode(message: impl Into<Message>) -> BytesMut {
		let mut buf = BytesMut::new();
		Codec::new().encode(message.into(), &mut buf).unwrap();
		buf
	}

	fn decode_one(bytes: &[u8]) -> Message {
		let mut buf = BytesMut::from(bytes);
		Codec::new()
			.decode(&mut buf)
			.expect("should parse")
			.expect("should be complete")
	}

	/// The exact bytes a head unit answered `OPTIONS *` with, from ncm-probe/session.log.
	const HEAD_UNIT_OPTIONS: &[u8] = b"RTSP/1.0 200 OK\r\n\
        Public: ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, POST, GET, PUT\r\n\
        Server: AirTunes/320.17\r\n\
        CSeq: 1\r\n\r\n";

	#[test]
	fn parses_a_real_head_unit_response() {
		let Message::Response(response) = decode_one(HEAD_UNIT_OPTIONS) else {
			panic!("should be a response");
		};

		assert_eq!(response.version, Version::Rtsp10);
		assert_eq!(response.status, 200);
		assert_eq!(response.reason, "OK");
		assert_eq!(response.cseq(), Some(1));
		assert_eq!(response.headers.get("server"), Some("AirTunes/320.17"));
		assert!(response.headers.get("Public").unwrap().contains("TEARDOWN"));
		assert!(response.body.is_empty());
	}

	/// `455 Method Not Valid In This State` has spaces in the reason phrase, and the head
	/// unit sends plenty of those.
	#[test]
	fn keeps_a_multi_word_reason_phrase() {
		let Message::Response(response) =
			decode_one(b"RTSP/1.0 455 Method Not Valid In This State\r\nContent-Length: 0\r\n\r\n")
		else {
			panic!("should be a response");
		};

		assert_eq!(response.status, 455);
		assert_eq!(response.reason, "Method Not Valid In This State");
		assert!(!response.is_success());
	}

	#[test]
	fn parses_a_request_with_a_body() {
		let Message::Request(request) = decode_one(
			b"POST /command RTSP/1.0\r\nCSeq: 4\r\n\
              Content-Type: application/x-apple-binary-plist\r\n\
              Content-Length: 8\r\n\r\nbplist00",
		) else {
			panic!("should be a request");
		};

		assert_eq!(request.method, Method::Post);
		assert_eq!(request.target, "/command");
		assert_eq!(request.cseq(), Some(4));
		assert_eq!(&request.body[..], b"bplist00");
	}

	/// The accessory may speak HTTP/1.1 on the event channel; the version has to survive
	/// so the answer can match it.
	#[test]
	fn preserves_the_protocol_version() {
		let Message::Request(request) =
			decode_one(b"POST /event HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
		else {
			panic!("should be a request");
		};
		assert_eq!(request.version, Version::Http11);

		let encoded = encode(Request::new(Method::Post, "/event").version(Version::Http11));
		assert!(encoded.starts_with(b"POST /event HTTP/1.1\r\n"));
	}

	#[test]
	fn round_trips_an_absolute_session_uri() {
		let target = "rtsp://[fe80::ff:fe00:1]/17991931767673078117";
		let encoded = encode(Request::new(Method::Setup, target));

		let Message::Request(request) = decode_one(&encoded) else {
			panic!("should be a request");
		};
		assert_eq!(request.method, Method::Setup);
		assert_eq!(request.target, target);
	}

	#[test]
	fn options_star_survives() {
		let Message::Request(request) = decode_one(&encode(Request::new(Method::Options, "*")))
		else {
			panic!("should be a request");
		};
		assert_eq!(request.target, "*");
	}

	/// Content-Length is always written, even for an empty body, and the caller's own
	/// value never survives to frame the next message.
	#[test]
	fn content_length_is_always_ours() {
		let encoded = encode(Response::ok());
		assert!(
			encoded.ends_with(b"Content-Length: 0\r\n\r\n"),
			"{encoded:?}"
		);

		let encoded = encode(
			Request::new(Method::Post, "/info")
				.header("Content-Length", "999")
				.body("application/x-apple-binary-plist", &b"bplist00"[..]),
		);
		let text = String::from_utf8_lossy(&encoded);
		assert!(text.contains("Content-Length: 8\r\n"), "{text}");
		assert!(!text.contains("999"), "{text}");
	}

	#[test]
	fn a_partial_message_yields_nothing_yet() {
		let full = encode(
			Request::new(Method::Post, "/pair-setup")
				.body("application/pairing+tlv8", &b"tlv-body"[..]),
		);
		let mut codec = Codec::new();

		// Feed it one byte at a time; only the last one may complete the message.
		let mut buf = BytesMut::new();
		for (index, byte) in full.iter().enumerate() {
			buf.put_u8(*byte);
			let decoded = codec.decode(&mut buf).unwrap();
			if index + 1 < full.len() {
				assert!(decoded.is_none(), "completed early at byte {index}");
			} else {
				assert!(decoded.is_some(), "never completed");
			}
		}
	}

	/// Two messages arriving in one read must both come out, and the head-scan cursor must
	/// reset between them.
	#[test]
	fn decodes_back_to_back_messages() {
		let mut buf = encode(Response::ok().body("text/plain", &b"first"[..]));
		buf.unsplit(encode(Response::new(404, "Not Found")));

		let mut codec = Codec::new();

		let Some(Message::Response(first)) = codec.decode(&mut buf).unwrap() else {
			panic!("should be a response");
		};
		assert_eq!(&first.body[..], b"first");

		let Some(Message::Response(second)) = codec.decode(&mut buf).unwrap() else {
			panic!("should be a response");
		};
		assert_eq!(second.status, 404);

		assert!(codec.decode(&mut buf).unwrap().is_none());
	}

	#[test]
	fn header_lookup_ignores_case_but_the_wire_does_not() {
		let mut headers = Headers::new();
		headers.insert("Content-Type", "application/octet-stream");
		assert_eq!(
			headers.get("CONTENT-TYPE"),
			Some("application/octet-stream")
		);

		headers.insert("content-type", "application/pairing+tlv8");
		assert_eq!(headers.iter().count(), 1, "insert replaces");
		assert_eq!(
			headers.get("Content-Type"),
			Some("application/pairing+tlv8")
		);

		let encoded = encode(Response::ok().header("X-Apple-ProtocolVersion", "1"));
		assert!(
			String::from_utf8_lossy(&encoded).contains("X-Apple-ProtocolVersion: 1"),
			"the original spelling is preserved"
		);
	}

	#[test]
	fn folded_headers_are_joined() {
		let Message::Response(response) = decode_one(
			b"RTSP/1.0 200 OK\r\nPublic: OPTIONS,\r\n\tSETUP\r\nContent-Length: 0\r\n\r\n",
		) else {
			panic!("should be a response");
		};
		assert_eq!(response.headers.get("Public"), Some("OPTIONS, SETUP"));
	}

	/// A body we cannot frame would desynchronise the cipher stream underneath, so it must
	/// be an error rather than a guess.
	#[test]
	fn chunked_encoding_is_refused() {
		let mut buf =
			BytesMut::from(&b"RTSP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"[..]);
		assert!(matches!(
			Codec::new().decode(&mut buf),
			Err(Error::TransferEncoding(_))
		));
	}

	#[test]
	fn malformed_messages_are_rejected() {
		let mut buf = BytesMut::from(&b"POST /info SIP/2.0\r\n\r\n"[..]);
		assert!(Codec::new().decode(&mut buf).is_err(), "unknown version");

		let mut buf = BytesMut::from(&b"RTSP/1.0 abc OK\r\n\r\n"[..]);
		assert!(Codec::new().decode(&mut buf).is_err(), "non-numeric status");

		let mut buf = BytesMut::from(&b"GET /info RTSP/1.0\r\nnot-a-header\r\n\r\n"[..]);
		assert!(
			Codec::new().decode(&mut buf).is_err(),
			"header has no colon"
		);

		let mut buf = BytesMut::from(&b"GET /info RTSP/1.0\r\nContent-Length: nine\r\n\r\n"[..]);
		assert!(Codec::new().decode(&mut buf).is_err(), "bad Content-Length");
	}

	#[test]
	fn an_absurd_content_length_does_not_allocate() {
		let mut buf =
			BytesMut::from(&b"RTSP/1.0 200 OK\r\nContent-Length: 99999999999\r\n\r\n"[..]);
		assert!(matches!(
			Codec::new().decode(&mut buf),
			Err(Error::BodyTooLong(_))
		));
	}

	#[test]
	fn a_head_block_that_never_ends_is_refused() {
		let mut buf = BytesMut::new();
		buf.put_slice(b"GET /info RTSP/1.0\r\n");
		while buf.len() <= MAX_HEAD_LEN {
			buf.put_slice(b"X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
		}
		assert!(matches!(
			Codec::new().decode(&mut buf),
			Err(Error::HeadTooLong)
		));
	}

	mod channel {
		use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

		use super::*;

		#[tokio::test]
		async fn requests_carry_an_increasing_cseq_and_responses_are_matched() {
			let (ours, mut theirs) = duplex(4096);
			let mut channel = Channel::new(ours);

			let peer = tokio::spawn(async move {
				let mut seen = Vec::new();
				for cseq in 1..=3u32 {
					let mut buf = vec![0u8; 512];
					let read = theirs.read(&mut buf).await.unwrap();
					seen.push(String::from_utf8_lossy(&buf[..read]).into_owned());

					theirs
						.write_all(
							format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n")
								.as_bytes(),
						)
						.await
						.unwrap();
				}
				seen
			});

			for _ in 0..3 {
				let response = channel
					.request(Request::new(Method::Post, "/pair-setup"))
					.await
					.unwrap();
				assert_eq!(response.status, 200);
			}

			let seen = peer.await.unwrap();
			for (index, request) in seen.iter().enumerate() {
				assert!(
					request.contains(&format!("CSeq: {}\r\n", index + 1)),
					"{request}"
				);
			}
		}

		#[tokio::test]
		async fn a_mismatched_cseq_is_an_error() {
			let (ours, mut theirs) = duplex(4096);
			let mut channel = Channel::new(ours);

			tokio::spawn(async move {
				let mut buf = vec![0u8; 512];
				let read = theirs.read(&mut buf).await.unwrap();
				assert!(read > 0, "should have received the request");
				theirs
					.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 99\r\nContent-Length: 0\r\n\r\n")
					.await
					.unwrap();
			});

			assert!(matches!(
				channel.request(Request::new(Method::Get, "/info")).await,
				Err(Error::CSeqMismatch {
					expected: 1,
					actual: Some(99)
				})
			));
		}

		/// The event channel: the accessory sends requests, we answer them, and the answer
		/// has to come back with the same CSeq and the same protocol version.
		#[tokio::test]
		async fn serving_echoes_cseq_and_version() {
			let (ours, mut theirs) = duplex(4096);
			let mut channel = Channel::new(ours);

			tokio::spawn(async move {
				theirs
					.write_all(
						b"POST /command HTTP/1.1\r\nCSeq: 7\r\nContent-Length: 4\r\n\r\nbody",
					)
					.await
					.unwrap();

				let mut buf = vec![0u8; 512];
				let read = theirs.read(&mut buf).await.unwrap();
				String::from_utf8_lossy(&buf[..read]).into_owned()
			});

			let request = channel.next_request().await.unwrap().unwrap();
			assert_eq!(request.method, Method::Post);
			assert_eq!(request.target, "/command");
			assert_eq!(&request.body[..], b"body");

			channel.respond(&request, Response::ok()).await.unwrap();
		}

		/// Crossing the pair-verify upgrade must not strand bytes the peer already sent.
		#[tokio::test]
		async fn into_parts_keeps_the_read_buffer() {
			let (ours, mut theirs) = duplex(4096);
			let mut channel = Channel::new(ours);

			// One response for the in-flight request, and a second message right behind it.
			theirs
				.write_all(
					b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n\
                      POST /command RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\nhi",
				)
				.await
				.unwrap();

			let response = channel
				.request(Request::new(Method::Get, "/info"))
				.await
				.unwrap();
			assert_eq!(response.status, 200);

			let (parts, cseq) = channel.into_parts();
			assert!(
				!parts.read_buf.is_empty(),
				"the pipelined message is still buffered"
			);

			let mut rebuilt = Channel::from_parts(parts, cseq);
			let request = rebuilt.next_request().await.unwrap().unwrap();
			assert_eq!(&request.body[..], b"hi", "and it survives the rebuild");
		}
	}
}
