//! iAP2 primitives and an iPhone-side state machine for CarPlay bring-up.
//!
//! This module now includes:
//! - Link-layer packet framing/checksums and SYN/ACK/EAK support (`56.2`, p434-446).
//! - Control-session message codec for authentication/identification (`97.1`, `97.2`).
//! - A high-level state machine that models device-side (iPhone role) sequencing for:
//!   link sync -> control session v2 -> authentication + identification -> CarPlay ready.
//!
//! Feature decisions based on the spec:
//! - CarPlay requires control session version 2 support (`56.3.2`, p456).
//! - Authentication and identification ordering is device-chosen in control session v2 (`20.1`, p185).
//! - Identification validation checks CarPlay-related parameters such as
//!   `VehicleInformationComponent`, `USBHostTransportCarPlayInterfaceNumber`,
//!   and `TransportSupportsCarPlay` (`97.2`, p906-911).
//! - For development, the state machine can run with fake auth that always verifies the accessory
//!   challenge response.
//!
//! # Example
//! ```rust,ignore
//! use crate::iap2::{
//!     state_machine::{Iap2Machine, Iap2MachineConfig, MachineOutput},
//! };
//!
//! let mut machine = Iap2Machine::new(Iap2MachineConfig::iphone_default());
//!
//! let outputs = machine.start()?;
//! for output in outputs {
//!     if let MachineOutput::Transmit(wire_bytes) = output {
//!         // send `wire_bytes` over transport
//!     }
//! }
//!
//! // on incoming transport bytes:
//! let incoming = &[0xff, 0x5a /* ... */];
//! let outputs = machine.on_transport_bytes(incoming)?;
//! for output in outputs {
//!     if let MachineOutput::Transmit(wire_bytes) = output {
//!         // send `wire_bytes` over transport
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
#![allow(dead_code)]

pub mod checksum;
pub mod control;
pub mod control_session;
pub mod decoder;
pub mod error;
pub mod link;
pub mod packet;
pub mod state_machine;
pub mod sync;

pub const IAP2_DETECTION_SEQUENCE: [u8; 6] = [0xff, 0x55, 0x02, 0x00, 0xee, 0x10];
pub const IAP2_DETECTION_INTERVAL_MS: u64 = 1000;

pub const IAP1_INCOMPATIBLE_SEQUENCE: [u8; 18] = [
	0xff, 0x55, 0x0e, 0x00, 0x13, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
	0xff, 0xeb,
];

pub fn is_iap2_detection_response(bytes: &[u8]) -> bool {
	bytes == IAP2_DETECTION_SEQUENCE
}

pub fn is_iap1_detection_response(bytes: &[u8]) -> bool {
	matches!(
		bytes,
		[0x55, 0x04, 0x00, 0x02, 0x04, 0xee, 0x08]
			| [0x55, 0x02, 0x00, 0x00, 0xfe]
			| [0xff, 0x55, 0x04, 0x00, 0x02, 0x04, 0xee, 0x08]
			| [0xff, 0x55, 0x02, 0x00, 0x00, 0xfe]
	)
}

#[cfg(test)]
mod tests {
	use super::{IAP2_DETECTION_SEQUENCE, is_iap1_detection_response, is_iap2_detection_response};

	#[test]
	fn identifies_iap2_probe_response() {
		assert!(is_iap2_detection_response(&IAP2_DETECTION_SEQUENCE));
	}

	#[test]
	fn identifies_iap1_probe_responses() {
		assert!(is_iap1_detection_response(&[0x55, 0x02, 0x00, 0x00, 0xfe]));
		assert!(is_iap1_detection_response(&[
			0xff, 0x55, 0x04, 0x00, 0x02, 0x04, 0xee, 0x08,
		]));
	}
}
