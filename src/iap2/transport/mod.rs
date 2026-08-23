//! Transports the iAP2 link runs over.
//!
//! Both present the same shape — `Stream<Item = Result<Frame>> + Sink<Frame>` — so
//! [`Link`](crate::iap2::link::Link) is generic over which one is in use:
//!
//! - [`bulk`]: USB Host Mode (§60.4). What we use after the role switch, when we are the
//!   host and the head unit is the device. Link packets go over the bulk pipes raw.
//! - [`hid`]: USB Device Mode (§60.5.6). What we use before the role switch, while we are
//!   pretending to be an iPhone. Link packets are carried inside vendor HID reports.

pub mod bulk;
pub mod hid;
