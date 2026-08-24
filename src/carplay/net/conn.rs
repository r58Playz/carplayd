use std::net::SocketAddrV6;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Body, client::conn::http1, header};
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
				bail!("control connection is already encrypted")
			}
			None => bail!("control connection was left in a broken state"),
		}
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
			.context("control connection was left in a broken state")?;
		let io = TokioIo::new(socket);

		let (mut sender, conn) = http1::Builder::new()
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

		select! {
			biased;
			x = fut => { x }
			x = conn => { bail!("connection exited early: {x:?}") }
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
