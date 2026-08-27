use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use log::{debug, trace, warn};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::{net::TcpListener, sync::mpsc::UnboundedSender, task::JoinSet};

use crate::carplay::{CARPLAY_SOURCE_VERSION, MacAddr, format_mac_upper};

pub struct CarplayCtrlServer {
	pub conn_tx: UnboundedSender<MacAddr>,
	/// Advertised as `id` (Table 30-22). The spec calls this the device's Bluetooth MAC,
	/// which we do not have — nothing over NCM resolves it as one, and the accessory's own
	/// `connect` request carries its MAC rather than ours. What it does have to be is stable
	/// and unique per box, since a head unit may key its device list off it.
	pub device_id: MacAddr,
	pub mdns: ServiceDaemon,
	pub ip: Ipv6Addr,
	pub scope_id: u32,
	pub port: u16,
}
impl CarplayCtrlServer {
	async fn service(
		conn_channel: UnboundedSender<MacAddr>,
		req: Request<Incoming>,
	) -> Result<Response<Full<Bytes>>> {
		trace!("carplay-ctrl req: {req:?}");
		let Some(command) = req.uri().path().strip_prefix("/ctrl-int/1/") else {
			return Response::builder()
				.status(404)
				.body(Full::new(Bytes::from("Not Found")))
				.context("failed to create response");
		};

		match command {
			"connect" => {
				let Some(mac) = req
					.headers()
					.get("AirPlay-Receiver-Device-ID")
					.and_then(|x| x.to_str().ok())
					.and_then(|x| x.parse::<u64>().ok())
					.filter(|&x| x <= 0xFF_FF_FF_FF_FF_FF)
				else {
					return Response::builder()
						.status(400)
						.body(Full::new(Bytes::from(
							"Bad Request (invalid airplay receiver device id)",
						)))
						.context("failed to create response");
				};

				let mac_addr = mac.to_be_bytes()[2..].try_into().unwrap();
				match conn_channel.send(mac_addr) {
					Ok(()) => {
						debug!("received carplay-ctrl connection request for {mac_addr:?}");
						Response::builder()
							.status(200)
							.body(Full::new(Bytes::from("OK")))
							.context("failed to create response")
					}
					Err(err) => {
						warn!(
							"failed to forward carplay-ctrl conn request for {mac_addr:?}: {err:?}"
						);
						Response::builder()
							.status(500)
							.body(Full::new(Bytes::from("Internal Server Error")))
							.context("failed to create response")
					}
				}
			}
			cmd => Response::builder()
				.status(404)
				.body(Full::new(Bytes::from(format!("{cmd} command not found"))))
				.context("failed to create response"),
		}
	}

	pub async fn run(self) -> Result<()> {
		let device_id = format_mac_upper(self.device_id);
		let params: &[(&str, &str)] = &[("id", &device_id), ("srcvers", CARPLAY_SOURCE_VERSION)];
		let service_info = ServiceInfo::new(
			"_carplay-ctrl._tcp",
			"carplayd-ctrl",
			"carplayd",
			IpAddr::V6(self.ip),
			self.port,
			params,
		)
		.context("failed to create service info")?;

		let mut set = JoinSet::new();

		self.mdns
			.register(service_info.clone())
			.context("failed to register")?;
		let ret: Result<()> = async {
			let socket = TcpListener::bind(SocketAddr::V6(SocketAddrV6::new(
				self.ip,
				self.port,
				0,
				self.scope_id,
			)))
			.await
			.context("failed to bind to addr")?;

			debug!("listening on carplay-ctrl");
			while let Ok((conn, addr)) = socket.accept().await {
				debug!("accepted new carplay-ctrl conn from {addr:?}");

				let conn_channel = self.conn_tx.clone();
				set.spawn(async move {
					if let Err(err) = http1::Builder::new()
						.serve_connection(
							TokioIo::new(conn),
							service_fn(|x| Self::service(conn_channel.clone(), x)),
						)
						.await
					{
						warn!("carplay-ctrl conn for {addr:?} failed: {err:?}");
					}
				});
			}

			Ok(())
		}
		.await;

		set.abort_all();
		self.mdns
			.unregister(service_info.get_fullname())
			.context("failed to unregister")?;
		ret
	}
}
