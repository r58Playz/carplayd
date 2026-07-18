//! Minimal iAP2 implementation for faking an iPhone (the *device*) to a CarPlay
//! head unit over the USB Device Mode HID transport.
//!
//! This is the mechanical link layer only — the parts that are identical for every
//! connection yet cannot be hardcoded, because they depend on the car's random,
//! per-packet sequence numbers and checksums:
//!
//! - [`packet`]: the link-packet wire codec (framing + checksums) and [`Control`] bits.
//! - [`Link`]: the link-layer driver — runs the detection/`SYN`/`SYN+ACK`/establish
//!   handshake, tracks sequence & acknowledgement numbers, auto-acks inbound data,
//!   and does the HID (de)framing for the USB Device Mode transport.
//! - Lower-level pieces are exported too ([`wrap_input`], [`Reassembler`],
//!   [`parse_set`], [`LinkSync`]) for debugging or bespoke handling.
//!
//! It intentionally omits everything that is *policy*: which control-session
//! messages to send and when, and their contents (authentication is accept-only,
//! identification, `DeviceTransportIdentifierNotification`, …). Drive that on top.
//!
//! # Usage — you only touch payloads
//! Hold one [`Link`] in config 2's task. In the EP0 `SET_REPORT` handler:
//! ```ignore
//! let out = link.receive(&set_report_data); // the recv_all() bytes
//! for report in out.reports {
//!     hid_tx.send_async(report.into()).await?; // detection echo / SYN+ACK / ACKs
//! }
//! if out.established {
//!     // device drives auth: send RequestAuthenticationCertificate first
//!     for report in link.send_control(request_auth_certificate()) {
//!         hid_tx.send_async(report.into()).await?;
//!     }
//! }
//! for (session, payload) in out.payloads {
//!     // your script: pick the next message from what the car sent, then
//!     // link.send_control(next_message()) and write the reports.
//! }
//! ```
#![allow(dead_code, unused_imports)] // the message-script layer that consumes this is written separately

mod hid;
mod link;
pub mod msg;
mod packet;

pub use hid::{INPUT_REPORT_SIZES, MAX_INPUT_CHUNK, Reassembler, wrap_input};
pub use link::{Frame, Link, LinkOutput, LinkSync, Session, parse_set};
pub use packet::{Control, DETECTION, LinkPacket, ParseError, checksum};
