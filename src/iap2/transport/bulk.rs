//! iAP2 over bulk endpoints, the USB Host Mode transport (spec §60.4).
//!
//! After the role switch we are the USB host and the head unit is the USB device. It
//! declares a vendor-specific interface with one bulk IN and one bulk OUT endpoint
//! (Table 60-1: class `0xFF`, subclass `0xF0`, protocol `0x00`), and link packets travel
//! over those pipes raw — no HID reports, no Link Control Byte, just the §56.2 framing.
//!
//! `nusb` already implements [`tokio::io::AsyncRead`]/[`AsyncWrite`](tokio::io::AsyncWrite)
//! on its endpoint readers and writers, so the whole transport is
//! `Framed<Join<…>, LinkCodec>` — a `Stream`/`Sink` of [`Frame`](crate::iap2::packet::Frame)
//! and nothing more.

use anyhow::{Context as _, Result, anyhow};
use log::debug;
use nusb::{
	Device, Interface,
	descriptors::TransferType,
	io::{EndpointRead, EndpointWrite},
	transfer::{Bulk, Direction, In, Out},
};
use tokio::io::Join;
use tokio_util::codec::Framed;

use crate::iap2::packet::LinkCodec;

/// Interface class/subclass/protocol of the accessory's iAP2 interface (Table 60-1).
pub const IAP2_INTERFACE: (u8, u8, u8) = (0xFF, 0xF0, 0x00);

/// USB NCM control interface, which carries the CarPlay session (Table 30-1).
pub const NCM_CONTROL_INTERFACE: (u8, u8, u8) = (0x02, 0x0D, 0x00);

/// USB NCM data interface (Table 30-3).
pub const NCM_DATA_INTERFACE: (u8, u8, u8) = (0x0A, 0x00, 0x01);

/// Transfers to keep queued in each direction. §60.4.4 asks for large packets rather than
/// lockstep request/response, so keep the pipe primed.
const NUM_TRANSFERS: usize = 4;

/// Multiple of the endpoint's maximum packet size to use as a transfer buffer.
const TRANSFER_PACKETS: usize = 8;

/// A framed iAP2 link over the accessory's bulk pipes.
pub type BulkTransport = Framed<Join<EndpointRead<Bulk>, EndpointWrite<Bulk>>, LinkCodec>;

/// Claim the accessory's iAP2 interface and frame its bulk pipes.
///
/// `max_recv_len` bounds the decoder until the accessory's Link Synchronization Payload
/// arrives; pass [`crate::iap2::packet::MAX_PACKET_LEN`] if nothing better is known.
pub async fn open(
	device: &Device,
	interface_number: u8,
	max_recv_len: usize,
) -> Result<BulkTransport> {
	let interface = device
		.claim_interface(interface_number)
		.await
		.with_context(|| format!("failed to claim iAP2 interface {interface_number}"))?;

	let (in_addr, out_addr) = bulk_endpoints(&interface)
		.with_context(|| format!("interface {interface_number} has no bulk IN/OUT pair"))?;

	let ep_in = interface
		.endpoint::<Bulk, In>(in_addr)
		.with_context(|| format!("failed to open bulk IN endpoint {in_addr:#04x}"))?;
	let ep_out = interface
		.endpoint::<Bulk, Out>(out_addr)
		.with_context(|| format!("failed to open bulk OUT endpoint {out_addr:#04x}"))?;

	let in_buf = ep_in.max_packet_size() * TRANSFER_PACKETS;
	let out_buf = ep_out.max_packet_size() * TRANSFER_PACKETS;
	debug!(
		"iAP2 bulk pipes: IN {in_addr:#04x} ({} B/packet), OUT {out_addr:#04x} ({} B/packet)",
		ep_in.max_packet_size(),
		ep_out.max_packet_size()
	);

	let reader = ep_in.reader(in_buf).with_num_transfers(NUM_TRANSFERS);
	let writer = ep_out.writer(out_buf).with_num_transfers(NUM_TRANSFERS);

	Ok(Framed::new(
		tokio::io::join(reader, writer),
		LinkCodec::new(max_recv_len),
	))
}

/// The bulk IN and OUT endpoint addresses of a claimed interface's current alt setting.
fn bulk_endpoints(interface: &Interface) -> Result<(u8, u8)> {
	let descriptor = interface
		.descriptor()
		.ok_or_else(|| anyhow!("no interface descriptor available"))?;

	let mut bulk_in = None;
	let mut bulk_out = None;
	for ep in descriptor.endpoints() {
		if ep.transfer_type() != TransferType::Bulk {
			continue;
		}
		match ep.direction() {
			Direction::In => bulk_in = bulk_in.or(Some(ep.address())),
			Direction::Out => bulk_out = bulk_out.or(Some(ep.address())),
		}
	}

	match (bulk_in, bulk_out) {
		(Some(i), Some(o)) => Ok((i, o)),
		_ => Err(anyhow!(
			"expected 1 bulk IN and 1 bulk OUT endpoint, found IN={bulk_in:?} OUT={bulk_out:?}"
		)),
	}
}
