use std::{net::SocketAddrV6, pin::pin};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
	Request, Response,
	body::{Body, Incoming},
	client::conn::http1 as http1_client,
	header,
	server::conn::http1 as http1_server,
	service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpStream, select};
use tokio_util::either::Either;

use crate::{
	carplay::net::codec::{Transport, secure},
	pairing::{
		auth_setup::{AccessoryAuth, AuthSetup},
		identity::Identity,
		keys::SessionKeys,
		setup::{AccessoryLTPK, CARPLAY_PIN, PairSetup, Pairing},
		verify::PairVerify,
	},
};

pub struct HeadUnitTcpConnection {
	socket: Option<Transport>,
}
impl HeadUnitTcpConnection {
	pub async fn new(addr: SocketAddrV6) -> Result<Self> {
		let stream = TcpStream::connect(addr)
			.await
			.context("failed to connect")?;

		Ok(Self {
			socket: Some(Either::Left(stream)),
		})
	}

	pub fn upgrade(&mut self, keys: SessionKeys) -> Result<()> {
		match self.socket.take() {
			Some(Either::Left(socket)) => {
				self.socket = Some(Either::Right(secure(socket, keys)));
				Ok(())
			}
			Some(already @ Either::Right(_)) => {
				self.socket = Some(already);
				bail!("connection is already encrypted")
			}
			None => bail!("connection was left in a broken state"),
		}
	}

	pub async fn serve_http<F, R>(&mut self, func: F) -> Result<()>
	where
		F: Fn(Request<Incoming>) -> R,
		R: Future<Output = Result<Response<Full<Bytes>>>>,
	{
		let socket = self
			.socket
			.as_mut()
			.context("connection was left in a broken state")?;

		let service = service_fn(func);

		let io = TokioIo::new(socket);
		http1_server::Builder::new()
			.serve_connection(io, service)
			.await
			.context("failed to service connection")
	}

	/// buffered
	pub async fn http_req<B>(&mut self, req: Request<B>) -> Result<Response<Full<Bytes>>>
	where
		B: Body + 'static,
		B::Data: Send,
		B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
	{
		let socket = self
			.socket
			.as_mut()
			.context("connection was left in a broken state")?;
		let io = TokioIo::new(socket);

		let (mut sender, conn) = http1_client::Builder::new()
			.handshake(io)
			.await
			.context("failed to handshake")?;

		let fut = async move {
			let (parts, body) = sender
				.send_request(req)
				.await
				.context("failed to send req")?
				.into_parts();

			let body = body
				.collect()
				.await
				.context("failed to collect body")?
				.to_bytes();

			Ok(Response::from_parts(parts, Full::new(body)))
		};

		let mut fut = pin!(fut);
		let mut conn = pin!(conn);

		select! {
			biased;
			x = &mut fut => x,
			x = &mut conn => {
				// if it hits eof it will return Ok. let it continue then
				x.context("control connection failed")?;
				fut.await
			}
		}
	}

	pub async fn pair_setup(&mut self, identity: &Identity) -> Result<Pairing> {
		let mut setup = PairSetup::new(identity, CARPLAY_PIN);
		let mut post = async |body: Vec<u8>| {
			let res = self
				.http_req(
					Request::post("/pair-setup")
						.header(header::HOST, "carplay")
						.header(header::CONTENT_TYPE, "application/pairing+tlv8")
						.body(Full::new(Bytes::from(body)))
						.context("failed to create req")?,
				)
				.await
				.context("failed to send req")?;
			if !res.status().is_success() {
				bail!("pair-setup returned {}", res.status());
			}
			res.into_body().into_inner().context("failed to get body")
		};

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
		identity: &Identity,
		ltpk: AccessoryLTPK,
	) -> Result<[u8; 32]> {
		let mut verify = PairVerify::new(identity, ltpk);
		let mut post = async |body: Vec<u8>| {
			let res = self
				.http_req(
					Request::post("/pair-verify")
						.header(header::HOST, "carplay")
						.header(header::CONTENT_TYPE, "application/pairing+tlv8")
						.body(Full::new(Bytes::from(body)))
						.context("failed to create req")?,
				)
				.await
				.context("failed to send req")?;
			if !res.status().is_success() {
				bail!("pair-verify returned {}", res.status());
			}
			res.into_body().into_inner().context("failed to get body")
		};

		let m1 = verify.m1();
		let m2 = post(m1).await.context("failed to send m1")?;
		let m3 = verify.m3(&m2).context("failed to parse m2")?;
		let m4 = post(m3).await.context("Failed to send m3")?;

		verify.finish(&m4).context("failed to finish verifying")
	}

	pub async fn mfi_auth(&mut self) -> Result<AccessoryAuth> {
		let auth = AuthSetup::new();

		let res = self
			.http_req(
				Request::post("/auth-setup")
					.header(header::HOST, "carplay")
					.header(header::CONTENT_TYPE, "application/octet-stream")
					.body(Full::new(Bytes::from(auth.request())))
					.context("failed to build request")?,
			)
			.await
			.context("failed to send request")?;

		if !res.status().is_success() {
			bail!("auth-setup returned {}", res.status());
		}

		let body = res.into_body().into_inner().context("failed to get body")?;

		auth.process_response(&body)
			.context("failed to process auth response")
	}
}

#[cfg(test)]
mod tests {
	use std::{
		net::{Ipv6Addr, SocketAddrV6},
		time::Duration,
	};

	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::TcpListener,
	};

	use super::*;

	/// Read one HTTP message off `io`, honouring Content-Length.
	async fn read_message<R: tokio::io::AsyncRead + Unpin>(io: &mut R) -> Result<Vec<u8>> {
		let mut buf = Vec::new();
		let head_end = loop {
			if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
				break at + 4;
			}
			let mut byte = [0u8; 1];
			if io.read(&mut byte).await? == 0 {
				bail!("peer closed mid-header");
			}
			buf.push(byte[0]);
		};

		let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
		let len = head
			.lines()
			.find_map(|l| l.strip_prefix("content-length:"))
			.and_then(|v| v.trim().parse::<usize>().ok())
			.unwrap_or(0);

		let mut body = vec![0u8; len];
		io.read_exact(&mut body).await?;
		buf.extend_from_slice(&body);
		Ok(buf)
	}

	fn response(body: &str) -> Vec<u8> {
		format!(
			"HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
			body.len()
		)
		.into_bytes()
	}

	async fn request(conn: &mut HeadUnitTcpConnection, path: &str) -> Result<String> {
		let res = conn
			.http_req(
				Request::post(path)
					.header(header::HOST, "carplay")
					.body(Full::new(Bytes::from_static(b"ping")))?,
			)
			.await?;
		let body = res.into_body().into_inner().context("no body")?;
		Ok(String::from_utf8_lossy(&body).to_string())
	}

	/// The plaintext phase: pair-setup and pair-verify are six POSTs on one socket, each
	/// built from a fresh hyper handshake. Nothing may be stranded between them.
	#[tokio::test]
	async fn six_requests_share_one_plaintext_connection() {
		let listener = TcpListener::bind("[::1]:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();

		let server = tokio::spawn(async move {
			let (mut io, _) = listener.accept().await.unwrap();
			for i in 0..6 {
				read_message(&mut io).await.unwrap();
				io.write_all(&response(&format!("reply-{i}")))
					.await
					.unwrap();
			}
		});

		let mut conn =
			HeadUnitTcpConnection::new(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0))
				.await
				.unwrap();

		for i in 0..6 {
			let body = request(&mut conn, "/pair-setup")
				.await
				.unwrap_or_else(|e| panic!("request {i} failed: {e:?}"));
			assert_eq!(body, format!("reply-{i}"), "request {i}");
		}

		server.await.unwrap();
	}

	/// The same, but after upgrade(): every byte now crosses the frame codec, which adds
	/// its own buffering underneath hyper.
	#[tokio::test]
	async fn six_requests_survive_the_encrypted_upgrade() {
		let listener = TcpListener::bind("[::1]:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();

		let secret = [0x5Au8; 32];
		let ours = SessionKeys::control(&secret);
		// The accessory's read key is our write key and vice versa.
		let theirs = SessionKeys {
			read: ours.write,
			write: ours.read,
		};

		let server = tokio::spawn(async move {
			let (io, _) = listener.accept().await.unwrap();
			let mut io = secure(io, theirs);
			for i in 0..6 {
				read_message(&mut io).await.unwrap();
				io.write_all(&response(&format!("sealed-{i}")))
					.await
					.unwrap();
				io.flush().await.unwrap();
			}
		});

		let mut conn =
			HeadUnitTcpConnection::new(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0))
				.await
				.unwrap();
		conn.upgrade(ours).unwrap();

		for i in 0..6 {
			let body = request(&mut conn, "/info").await.unwrap();
			assert_eq!(body, format!("sealed-{i}"), "request {i}");
		}

		server.await.unwrap();
	}

	/// A peer that writes ahead is rejected, not silently mis-parsed.
	///
	/// This is the one case where bytes could be stranded: each http_req builds a fresh
	/// hyper handshake and drops it, so anything hyper read past the end of the response
	/// would die with its buffer. In practice hyper notices the extra message and fails
	/// the connection with "received unexpected message from connection", so the failure
	/// is loud rather than silent corruption.
	///
	/// CarPlay's control connection is strict request/response -- the accessory pushes on
	/// the event channel instead -- so this should not arise. The test pins the behaviour
	/// in case a head unit ever does it.
	#[tokio::test]
	async fn a_peer_that_writes_ahead_is_rejected_loudly() {
		let listener = TcpListener::bind("[::1]:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();

		let server = tokio::spawn(async move {
			let (mut io, _) = listener.accept().await.unwrap();
			read_message(&mut io).await.unwrap();

			// Both responses in a single write, so they arrive together.
			let mut both = response("first");
			both.extend_from_slice(&response("second"));
			io.write_all(&both).await.unwrap();

			tokio::time::sleep(Duration::from_secs(2)).await;
		});

		let mut conn =
			HeadUnitTcpConnection::new(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0))
				.await
				.unwrap();

		let err = tokio::time::timeout(Duration::from_millis(500), request(&mut conn, "/one"))
			.await
			.expect("should fail fast rather than hang")
			.expect_err("a pipelined second response must not be accepted");

		assert!(
			format!("{err:#}").contains("unexpected message"),
			"expected hyper to reject the extra message, got: {err:#}"
		);

		server.abort();
	}
}
