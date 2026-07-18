//! Control-session message templates (spec §56.3.2 / §97).
//!
//! Wire format is `40 40 <len16 BE> <msgID16 BE> <param…>`, where each parameter is
//! `<len16 BE> <paramID16 BE> <data>` and every length is inclusive of its own
//! field. These are the payloads that go inside a link packet — hand them to
//! [`Link::send_control`](super::Link::send_control).
//!
//! Only the messages the *device* drives are built here. Authentication is
//! accept-only (we never verify the car's certificate or challenge response), so
//! the crypto-bearing messages carry arbitrary fixed data.

/// Message identifiers (§97). `ID_*` the device sends are listed first, then the
/// ones the accessory (car) sends — match those against [`message_id`] to advance
/// the handshake script.
pub const ID_REQUEST_AUTH_CERTIFICATE: u16 = 0xAA00;
pub const ID_REQUEST_AUTH_CHALLENGE_RESPONSE: u16 = 0xAA02;
pub const ID_AUTHENTICATION_FAILED: u16 = 0xAA04;
pub const ID_AUTHENTICATION_SUCCEEDED: u16 = 0xAA05;
pub const ID_START_IDENTIFICATION: u16 = 0x1D00;
pub const ID_IDENTIFICATION_ACCEPTED: u16 = 0x1D02;
pub const ID_IDENTIFICATION_REJECTED: u16 = 0x1D03;
pub const ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION: u16 = 0x4E0E;

// Accessory → device (the car sends these).
pub const ID_AUTHENTICATION_CERTIFICATE: u16 = 0xAA01;
pub const ID_AUTHENTICATION_RESPONSE: u16 = 0xAA03;
pub const ID_ACCESSORY_AUTH_SERIAL_NUMBER: u16 = 0xAA06;
pub const ID_IDENTIFICATION_INFORMATION: u16 = 0x1D01;

// ── Hardcoded, parameter-free device messages (bytes are `40 40 00 06 <id>`) ──

/// `RequestAuthenticationCertificate` (0xAA00) — starts authentication.
pub const REQUEST_AUTH_CERTIFICATE: [u8; 6] = [0x40, 0x40, 0x00, 0x06, 0xAA, 0x00];
/// `AuthenticationSucceeded` (0xAA05) — sent unconditionally; the device is the
/// verifier and always accepts.
pub const AUTHENTICATION_SUCCEEDED: [u8; 6] = [0x40, 0x40, 0x00, 0x06, 0xAA, 0x05];
/// `StartIdentification` (0x1D00) — asks the accessory for its identification.
pub const START_IDENTIFICATION: [u8; 6] = [0x40, 0x40, 0x00, 0x06, 0x1D, 0x00];
/// `IdentificationAccepted` (0x1D02) — accepts the accessory's identification.
pub const IDENTIFICATION_ACCEPTED: [u8; 6] = [0x40, 0x40, 0x00, 0x06, 0x1D, 0x02];

/// Placeholder authentication challenge. Content and length are irrelevant — the
/// car's coprocessor signs it and we discard the signature.
pub const CHALLENGE: [u8; 20] = [0xA5; 20];

/// Build a control-session message from a message id and `(param_id, data)` list.
pub fn control_message(id: u16, params: &[(u16, &[u8])]) -> Vec<u8> {
	let params_len: usize = params.iter().map(|(_, d)| 4 + d.len()).sum();
	let total = 6 + params_len;
	let mut m = Vec::with_capacity(total);
	m.extend_from_slice(&[0x40, 0x40]);
	m.extend_from_slice(&(total as u16).to_be_bytes());
	m.extend_from_slice(&id.to_be_bytes());
	for (pid, data) in params {
		let plen = 4 + data.len();
		m.extend_from_slice(&(plen as u16).to_be_bytes());
		m.extend_from_slice(&pid.to_be_bytes());
		m.extend_from_slice(data);
	}
	m
}

/// `RequestAuthenticationChallengeResponse` (0xAA02) carrying an
/// `AuthenticationChallenge` (param 0). Pass [`CHALLENGE`] unless you have a reason
/// not to — the value is never validated.
pub fn request_auth_challenge_response(challenge: &[u8]) -> Vec<u8> {
	control_message(ID_REQUEST_AUTH_CHALLENGE_RESPONSE, &[(0, challenge)])
}

/// `DeviceTransportIdentifierNotification` (0x4E0E) with a `USBTransportIdentifier`
/// (param 1, utf8). The car correlates this against any wireless session it has;
/// with none, any string works and it proceeds to the role switch.
pub fn device_transport_identifier(usb_identifier: &str) -> Vec<u8> {
	control_message(
		ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
		&[(1, usb_identifier.as_bytes())],
	)
}

/// The message id of a control-session payload, if it starts with the `40 40`
/// marker and is long enough. Use it to dispatch on the car's inbound messages.
pub fn message_id(payload: &[u8]) -> Option<u16> {
	if payload.len() >= 6 && payload[0] == 0x40 && payload[1] == 0x40 {
		Some(u16::from_be_bytes([payload[4], payload[5]]))
	} else {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fixed_messages_match_the_builder() {
		assert_eq!(control_message(ID_REQUEST_AUTH_CERTIFICATE, &[]), REQUEST_AUTH_CERTIFICATE);
		assert_eq!(control_message(ID_AUTHENTICATION_SUCCEEDED, &[]), AUTHENTICATION_SUCCEEDED);
		assert_eq!(control_message(ID_START_IDENTIFICATION, &[]), START_IDENTIFICATION);
		assert_eq!(control_message(ID_IDENTIFICATION_ACCEPTED, &[]), IDENTIFICATION_ACCEPTED);
	}

	#[test]
	fn challenge_response_layout() {
		let m = request_auth_challenge_response(&[0xA5; 20]);
		// 40 40 | len=0x001E | AA 02 | plen=0x0018 | pid=0x0000 | 20 challenge bytes
		assert_eq!(&m[..10], &[0x40, 0x40, 0x00, 0x1E, 0xAA, 0x02, 0x00, 0x18, 0x00, 0x00]);
		assert_eq!(&m[10..], &[0xA5; 20]);
		assert_eq!(m.len(), 30);
	}

	#[test]
	fn transport_identifier_layout() {
		let m = device_transport_identifier("usb0");
		assert_eq!(
			m,
			vec![0x40, 0x40, 0x00, 0x0E, 0x4E, 0x0E, 0x00, 0x08, 0x00, 0x01, b'u', b's', b'b', b'0']
		);
	}

	#[test]
	fn reads_message_ids() {
		assert_eq!(message_id(&REQUEST_AUTH_CERTIFICATE), Some(0xAA00));
		assert_eq!(message_id(&device_transport_identifier("x")), Some(0x4E0E));
		assert_eq!(message_id(&[0x00, 0x40, 0, 6, 0xAA, 0x00]), None); // wrong marker
		assert_eq!(message_id(&[0x40, 0x40]), None); // too short
	}
}
