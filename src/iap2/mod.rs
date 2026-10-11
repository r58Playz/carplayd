//! iAP2, from the wire up, for impersonating an iPhone to a CarPlay head unit.
//!
//! The stack is four layers, each a `Stream`/`Sink` over the one below, so the transport
//! is interchangeable — the same link and session code runs over HID reports before the
//! USB role switch and over bulk pipes after it:
//!
//! ```text
//! policy::Policy            what to send and when: authenticate, identify, answer
//! session::ControlSession   Stream<Item = ControlEvent> + Sink<Message>   (§56.3.2)
//! link::Link                Stream<Item = LinkEvent>   + Sink<(session, payload)> (§56.2)
//! transport::{bulk, hid}    Stream<Item = Frame>       + Sink<Frame>
//! ```
//!
//! - [`packet`] is the §56.2 wire codec: [`LinkPacket`], [`Control`], and [`LinkCodec`],
//!   which resynchronizes to the next start-of-packet marker instead of failing.
//! - [`sync`] is the Link Synchronization Payload and the parameters it negotiates.
//! - [`link`] is the §56.2.7 operation record — sequence numbers, the send window,
//!   retransmission, cumulative and extended acknowledgements — as a pure state machine
//!   plus a `Stream`/`Sink` wrapper that owns the timers.
//! - [`msg`] decodes and encodes control-session messages, including group parameters,
//!   and carries the generated §97 identifier table.
//! - [`session`] filters the link down to the control session.
//! - [`policy`] is the device half of the handshake: accept-only authentication,
//!   identification, and the notifications a device sends unprompted. The mechanical part
//!   is [`Handshake`]; what to answer above it is the [`Policy`] trait, whose methods all
//!   have defaults so each side implements only what it needs.
//!
//! # Usage
//! ```ignore
//! let transport = transport::bulk::open(&device, iap2_interface, MAX_PACKET_LEN).await?;
//! let link = Link::new(transport, Role::Device, LinkParams::usb_recommended());
//! let mut session = ControlSession::new(link);
//! let mut handshake = Handshake::new(DeviceProfile::default());
//! let params = policy::drive_until_ready(&mut session, &mut handshake).await?;
//! info!("iAP2 ready over {params:?}; CarPlay interface {:?}", handshake.carplay_interface());
//! policy::drive(&mut session, &mut handshake).await
//! ```

pub mod error;
pub mod link;
pub mod msg;
pub mod packet;
pub mod policy;
pub mod session;
pub mod sync;
pub mod transport;

pub use error::Error;
pub use link::{Link, Role};
pub use packet::MAX_PACKET_LEN;
pub use policy::{DeviceProfile, Handshake};
pub use session::ControlSession;
pub use sync::LinkParams;
