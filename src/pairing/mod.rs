//! CarPlay pairing, from the iOS device side.
//!
//! §30.3.2.1 sends both halves as `application/pairing+tlv8` bodies POSTed to
//! `/pair-setup` and `/pair-verify` on the RTSP control connection, and the payloads are
//! HomeKit's (HAP R13 §5.6 and §5.7) rather than anything CarPlay-specific.
//!
//! [`setup`] runs once per head unit and leaves both sides holding the other's Ed25519
//! long-term key; [`verify`] runs at the start of every session and produces the shared
//! secret that [`keys::SessionKeys`] turns into channel keys.

pub mod auth_setup;
pub mod identity;
pub mod keys;
pub mod setup;
pub mod srp;
pub mod tlv8;
pub mod verify;
