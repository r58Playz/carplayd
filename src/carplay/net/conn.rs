use std::{
	net::SocketAddrV6,
	time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use log::warn;
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::TcpStream;
use tokio_util::{codec::FramedParts, either::Either};

use crate::{
	carplay::{
		CARPLAY_DEVICE_NAME, CARPLAY_SOURCE_VERSION,
		net::{
			codec::{Transport, secure},
			proto,
			rtsp::{Channel, Message, Method, Request, Response},
		},
	},
	pairing::{
		auth_setup::{AccessoryAuth, AuthSetup},
		identity::PairIdentity,
		keys::SessionKeys,
		setup::{AccessoryLTPK, CARPLAY_PIN, PairSetup, Pairing},
		verify::PairVerify,
	},
};

const APPLE_EPOCH_OFFSET: u64 = 978_307_200;
fn apple_absolute_time() -> String {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |d| d.as_secs())
		.saturating_sub(APPLE_EPOCH_OFFSET)
		.to_string()
}

const PAIRING_TLV8: &str = "application/pairing+tlv8";
const OCTET_STREAM: &str = "application/octet-stream";

pub struct HeadUnitTcpConnection {
	ua: String,
	channel: Option<Channel<Transport>>,
}

impl HeadUnitTcpConnection {
	pub async fn new(addr: SocketAddrV6) -> Result<Self> {
		let stream = TcpStream::connect(addr)
			.await
			.context("failed to connect")?;

		stream.set_nodelay(true).context("failed to set nodelay")?;

		Ok(Self {
			ua: format!("AirPlay/{CARPLAY_SOURCE_VERSION}"),
			channel: Some(Channel::new(Either::Left(stream))),
		})
	}

	pub fn upgrade(&mut self, keys: SessionKeys) -> Result<()> {
		let channel = self
			.channel
			.take()
			.context("connection was left in a broken state")?;
		let (parts, cseq) = channel.into_parts();

		let upgraded = match parts.io {
			Either::Left(socket) => {
				let mut upgraded =
					FramedParts::new::<Message>(Either::Right(secure(socket, keys)), parts.codec);
				upgraded.read_buf = parts.read_buf;
				upgraded.write_buf = parts.write_buf;

				upgraded
			}
			Either::Right(_) => {
				warn!("attempted to upgrade already upgraded connection");
				self.channel = Some(Channel::from_parts(parts, cseq));
				bail!("connection is already encrypted");
			}
		};

		self.channel = Some(Channel::from_parts(upgraded, cseq));

		Ok(())
	}

	fn channel(&mut self) -> Result<&mut Channel<Transport>> {
		self.channel
			.as_mut()
			.context("connection was left in a broken state")
	}

	pub async fn request(&mut self, request: Request) -> Result<Response> {
		let request = request
			.header("User-Agent", &self.ua)
			.header("X-Apple-ProtocolVersion", "1")
			.header("X-Apple-AbsoluteTime", apple_absolute_time())
			.header("X-Apple-Client-Name", CARPLAY_DEVICE_NAME)
			.header("X-Apple-PD", "1")
			.header("X-Apple-HKP", "2");

		self.channel()?
			.request(request)
			.await
			.context("control request failed")
	}

	pub async fn req_opt<R: DeserializeOwned>(
		&mut self,
		method: Method,
		path: &str,
		body: &impl Serialize,
	) -> Result<Option<R>> {
		let res = self
			.request(Request::new(method, path).body(
				proto::CONTENT_TYPE,
				proto::to_binary(body).context("failed to serialize body")?,
			))
			.await
			.context("failed to send post")?;

		if !res.is_success() {
			bail!("failed to POST {path}");
		}

		(!res.body.is_empty())
			.then(|| proto::from_binary(&res.body))
			.transpose()
	}
	pub async fn req<R: DeserializeOwned>(
		&mut self,
		method: Method,
		path: &str,
		body: &impl Serialize,
	) -> Result<R> {
		self.req_opt(method, path, body)
			.await?
			.with_context(|| format!("no body found for {path:?}"))
	}

	pub async fn next_request(&mut self) -> Result<Option<Request>> {
		self.channel()?
			.next_request()
			.await
			.context("failed to read an accessory request")
	}

	pub async fn respond(&mut self, to: &Request, response: Response) -> Result<()> {
		self.channel()?
			.respond(to, response)
			.await
			.context("failed to answer an accessory request")
	}

	async fn req_post(&mut self, path: &str, content_type: &str, body: Vec<u8>) -> Result<Bytes> {
		let response = self
			.request(Request::new(Method::Post, path).body(content_type, body))
			.await
			.with_context(|| format!("failed to POST {path}"))?;

		if !response.is_success() {
			bail!("{path} returned {} {}", response.status, response.reason);
		}

		Ok(response.body)
	}

	pub async fn pair_setup(&mut self, identity: &PairIdentity) -> Result<Pairing> {
		let mut setup = PairSetup::new(identity, CARPLAY_PIN);
		let mut post = async |body: Vec<u8>| self.req_post("/pair-setup", PAIRING_TLV8, body).await;

		let m1 = setup.m1();
		let m2 = post(m1).await.context("failed to send m1")?;
		let m3 = setup.m3(&m2).context("failed to parse m2")?;
		let m4 = post(m3).await.context("failed to send m3")?;
		let m5 = setup.m5(&m4).context("failed to parse m4")?;
		let m6 = post(m5).await.context("failed to send m5")?;

		setup.finish(&m6).context("failed to finish pairing")
	}

	pub async fn pair_verify(
		&mut self,
		identity: &PairIdentity,
		ltpk: AccessoryLTPK,
	) -> Result<[u8; 32]> {
		let mut verify = PairVerify::new(identity, ltpk);
		let mut post =
			async |body: Vec<u8>| self.req_post("/pair-verify", PAIRING_TLV8, body).await;

		let m1 = verify.m1();
		let m2 = post(m1).await.context("failed to send m1")?;
		let m3 = verify.m3(&m2).context("failed to parse m2")?;
		let m4 = post(m3).await.context("failed to send m3")?;

		verify.finish(&m4).context("failed to finish verifying")
	}

	pub async fn mfi_auth(&mut self) -> Result<AccessoryAuth> {
		let auth = AuthSetup::new();

		let body = self
			.req_post("/auth-setup", OCTET_STREAM, auth.request())
			.await
			.context("failed to exchange auth-setup")?;

		auth.process_response(&body)
			.context("failed to process auth response")
	}
}

#[cfg(test)]
mod tests {
	use std::net::{Ipv6Addr, SocketAddrV6};

	use futures::{SinkExt, StreamExt};
	use tokio::{
		io::{AsyncRead, AsyncWrite},
		net::TcpListener,
	};
	use tokio_util::codec::Framed;

	use super::*;
	use crate::carplay::net::rtsp;

	/// Answer `count` requests with `reply-<n>`, collecting the `CSeq` of each.
	///
	/// Using the codec on both sides is deliberate: it exercises the encoder in the
	/// direction the accessory writes, which is what the event channel depends on.
	async fn scripted_accessory<T: AsyncRead + AsyncWrite + Unpin>(
		io: T,
		count: usize,
	) -> Vec<Option<u32>> {
		let mut framed = Framed::new(io, rtsp::Codec::new());
		let mut seen = Vec::new();

		for index in 0..count {
			let Some(Ok(Message::Request(request))) = framed.next().await else {
				break;
			};
			seen.push(request.cseq());

			let mut response = Response::ok().body("text/plain", format!("reply-{index}"));
			if let Some(cseq) = request.cseq() {
				response.headers.insert("CSeq", cseq.to_string());
			}
			framed.send(Message::Response(response)).await.unwrap();
		}

		seen
	}

	async fn listen() -> (TcpListener, SocketAddrV6) {
		let listener = TcpListener::bind("[::1]:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		(listener, SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0))
	}

	/// Keys as the accessory sees them: its read key is our write key and vice versa.
	fn mirrored(ours: SessionKeys) -> SessionKeys {
		SessionKeys {
			read: ours.write,
			write: ours.read,
		}
	}

	async fn body_of(conn: &mut HeadUnitTcpConnection, path: &str) -> String {
		let body = conn
			.req_post(path, OCTET_STREAM, b"ping".to_vec())
			.await
			.unwrap();
		String::from_utf8_lossy(&body).into_owned()
	}

	fn cseqs(range: std::ops::RangeInclusive<u32>) -> Vec<Option<u32>> {
		range.map(Some).collect()
	}

	/// Pair-setup and pair-verify are six POSTs on one socket. Nothing may be stranded
	/// between them, and each must carry the next `CSeq`.
	#[tokio::test]
	async fn six_requests_share_one_plaintext_connection() {
		let (listener, addr) = listen().await;

		let accessory = tokio::spawn(async move {
			let (io, _) = listener.accept().await.unwrap();
			scripted_accessory(io, 6).await
		});

		let mut conn = HeadUnitTcpConnection::new(addr).await.unwrap();
		for index in 0..6 {
			assert_eq!(
				body_of(&mut conn, "/pair-setup").await,
				format!("reply-{index}")
			);
		}

		assert_eq!(
			accessory.await.unwrap(),
			cseqs(1..=6),
			"CSeq runs for the life of the connection"
		);
	}

	/// The same, but after `upgrade()`: every byte now crosses the frame codec, which adds
	/// its own buffering underneath the message framing.
	#[tokio::test]
	async fn six_requests_survive_the_encrypted_upgrade() {
		let (listener, addr) = listen().await;

		let ours = SessionKeys::control(&[0x5Au8; 32]);
		let theirs = mirrored(ours);

		let accessory = tokio::spawn(async move {
			let (io, _) = listener.accept().await.unwrap();
			scripted_accessory(secure(io, theirs), 6).await
		});

		let mut conn = HeadUnitTcpConnection::new(addr).await.unwrap();
		conn.upgrade(ours).unwrap();

		for index in 0..6 {
			assert_eq!(body_of(&mut conn, "/info").await, format!("reply-{index}"));
		}

		assert_eq!(accessory.await.unwrap(), cseqs(1..=6));
	}

	#[tokio::test]
	async fn upgrading_twice_is_refused_without_losing_the_connection() {
		let (listener, addr) = listen().await;

		let ours = SessionKeys::control(&[0x11u8; 32]);
		let theirs = mirrored(ours);

		let accessory = tokio::spawn(async move {
			let (io, _) = listener.accept().await.unwrap();
			scripted_accessory(secure(io, theirs), 1).await
		});

		let mut conn = HeadUnitTcpConnection::new(addr).await.unwrap();
		conn.upgrade(ours).unwrap();
		assert!(conn.upgrade(ours).is_err(), "already encrypted");

		// The refusal put the channel back, so the connection still works.
		assert_eq!(body_of(&mut conn, "/info").await, "reply-0");
		accessory.await.unwrap();
	}

	/// A peer that writes ahead used to be a hazard: every request built a fresh hyper
	/// handshake and dropped it, so a response hyper had read past the end of the one it
	/// was waiting for died with its buffer. Framing the connection once fixes that — the
	/// second response is simply already there when we ask for it.
	#[tokio::test]
	async fn a_pipelined_response_is_buffered_not_dropped() {
		let (listener, addr) = listen().await;

		tokio::spawn(async move {
			let (io, _) = listener.accept().await.unwrap();
			let mut framed = Framed::new(io, rtsp::Codec::new());

			// Wait for the first request, then answer it *and* the next one at once.
			framed.next().await.unwrap().unwrap();

			let mut first = Response::ok().body("text/plain", "first");
			first.headers.insert("CSeq", "1");
			let mut second = Response::ok().body("text/plain", "second");
			second.headers.insert("CSeq", "2");

			framed.feed(Message::Response(first)).await.unwrap();
			framed.feed(Message::Response(second)).await.unwrap();
			framed.flush().await.unwrap();

			// Hold the connection open, so the client is not woken by an EOF instead.
			std::future::pending::<()>().await;
		});

		let mut conn = HeadUnitTcpConnection::new(addr).await.unwrap();
		assert_eq!(body_of(&mut conn, "/one").await, "first");
		assert_eq!(
			body_of(&mut conn, "/two").await,
			"second",
			"the pipelined response was buffered, not discarded"
		);
	}

	/// The event channel: same type, same framing, requests running the other way.
	#[tokio::test]
	async fn the_same_connection_serves_accessory_requests() {
		let (listener, addr) = listen().await;

		let ours = SessionKeys::event(&[0x77u8; 32]);
		let theirs = mirrored(ours);

		let accessory = tokio::spawn(async move {
			let (io, _) = listener.accept().await.unwrap();
			let mut framed = Framed::new(secure(io, theirs), rtsp::Codec::new());

			let mut request = Request::new(Method::Post, "/command")
				.version(rtsp::Version::Http11)
				.body("application/x-apple-binary-plist", &b"bplist00"[..]);
			request.headers.insert("CSeq", "9");

			framed.send(Message::Request(request)).await.unwrap();

			let Some(Ok(Message::Response(response))) = framed.next().await else {
				panic!("expected a response");
			};
			response
		});

		let mut conn = HeadUnitTcpConnection::new(addr).await.unwrap();
		conn.upgrade(ours).unwrap();

		let request = conn.next_request().await.unwrap().unwrap();
		assert_eq!(request.target, "/command");
		assert_eq!(&request.body[..], b"bplist00");

		conn.respond(&request, Response::ok()).await.unwrap();

		let response = accessory.await.unwrap();
		assert_eq!(response.cseq(), Some(9), "CSeq is echoed");
		assert_eq!(
			response.version,
			rtsp::Version::Http11,
			"and so is the protocol version"
		);
	}
}
