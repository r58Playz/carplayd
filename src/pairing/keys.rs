//! Key derivation and the AEAD helpers shared by pairing and the control channel.
//!
//! Every derivation is HKDF-SHA-512 with a 32-byte output; only the salt, info and input
//! keying material change. Pair-setup keys off the SRP shared secret K, everything after
//! pair-verify keys off the Curve25519 shared secret (§30.3.2.2).

use anyhow::{Context, Result};
use chacha20poly1305::{
	ChaCha20Poly1305, KeyInit,
	aead::{Aead, Payload},
};
use hkdf::Hkdf;
use sha2::Sha512;

/// HKDF-SHA-512 with the 32-byte output every CarPlay and HAP derivation asks for.
pub fn derive(input_key: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
	let mut output = [0u8; 32];

	Hkdf::<Sha512>::new(Some(salt), input_key)
		.expand(info, &mut output)
		.expect("32 bytes is well within HKDF-SHA-512's output limit");

	output
}

/// The 96-bit nonce for a pairing message, whose label is a fixed 8-byte string such as
/// `PS-Msg05`. The leading 32 bits are zero, as they are for the control channel.
pub fn pairing_nonce(label: &[u8; 8]) -> [u8; 12] {
	let mut nonce = [0u8; 12];
	nonce[4..].copy_from_slice(label);
	nonce
}

/// The 96-bit nonce for frame `counter` on an encrypted channel.
///
/// HAP §6.5.2: "The 32-bit fixed-common part of the 96-bit nonce is all zeros", with the
/// remaining 64 bits a little endian counter that runs for the life of the connection.
pub fn stream_nonce(counter: u64) -> [u8; 12] {
	let mut nonce = [0u8; 12];
	nonce[4..].copy_from_slice(&counter.to_le_bytes());
	nonce
}

/// Encrypt, returning the ciphertext with its 16-byte tag appended.
pub fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
	ChaCha20Poly1305::new(key.into())
		.encrypt(
			nonce.into(),
			Payload {
				msg: plaintext,
				aad,
			},
		)
		.map_err(|_| anyhow::anyhow!("ChaCha20-Poly1305 encryption failed"))
}

/// Decrypt a ciphertext that carries its 16-byte tag at the end.
pub fn open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
	ChaCha20Poly1305::new(key.into())
		.decrypt(
			nonce.into(),
			Payload {
				msg: ciphertext,
				aad,
			},
		)
		.map_err(|_| anyhow::anyhow!("ChaCha20-Poly1305 authentication failed"))
}

/// Read and write keys for one encrypted connection.
///
/// Both names are from our point of view, as the device: §30.3.2.2.1 notes the roles are
/// reversed on the accessory. So we decrypt with `read` and encrypt with `write`, and the
/// head unit does the opposite.
#[derive(Clone, Copy)]
pub struct SessionKeys {
	pub read: [u8; 32],
	pub write: [u8; 32],
}

impl SessionKeys {
	/// Keys for the RTSP control connection (§30.3.2.2.1).
	pub fn control(shared_secret: &[u8; 32]) -> Self {
		Self {
			read: derive(
				shared_secret,
				b"Control-Salt",
				b"Control-Read-Encryption-Key",
			),
			write: derive(
				shared_secret,
				b"Control-Salt",
				b"Control-Write-Encryption-Key",
			),
		}
	}

	/// Keys for the event connection (§30.3.2.2.1).
	///
	/// The accessory originates requests on this one, so although the derivation mirrors
	/// the control channel, `read` here carries inbound requests rather than responses.
	pub fn event(shared_secret: &[u8; 32]) -> Self {
		Self {
			read: derive(shared_secret, b"Event-Salt", b"Event-Read-Encryption-Key"),
			write: derive(shared_secret, b"Event-Salt", b"Event-Write-Encryption-Key"),
		}
	}

	/// Keys for one media stream (§30.3.2.2.2).
	///
	/// The salt is `"DataStream-Salt"` with the stream's connection ID, delivered in the
	/// RTSP SETUP response, appended to it.
	pub fn data_stream(shared_secret: &[u8; 32], stream_connection_id: u64) -> Self {
		let mut salt = b"DataStream-Salt".to_vec();
		salt.extend_from_slice(stream_connection_id.to_string().as_bytes());

		Self {
			read: derive(shared_secret, &salt, b"DataStream-Input-Encryption-Key"),
			write: derive(shared_secret, &salt, b"DataStream-Output-Encryption-Key"),
		}
	}
}

/// Derive the pair-setup session key used to encrypt the M5 and M6 sub-TLVs (HAP §5.6.4).
pub fn pair_setup_session_key(srp_shared_secret: &[u8]) -> [u8; 32] {
	derive(
		srp_shared_secret,
		b"Pair-Setup-Encrypt-Salt",
		b"Pair-Setup-Encrypt-Info",
	)
}

/// `iOSDeviceX`, the prefix we sign over in pair-setup M5 (HAP §5.6.5.2 step 2).
pub fn pair_setup_controller_x(srp_shared_secret: &[u8]) -> [u8; 32] {
	derive(
		srp_shared_secret,
		b"Pair-Setup-Controller-Sign-Salt",
		b"Pair-Setup-Controller-Sign-Info",
	)
}

/// `AccessoryX`, the prefix the accessory signs over in pair-setup M6 (HAP §5.6.6.2).
pub fn pair_setup_accessory_x(srp_shared_secret: &[u8]) -> [u8; 32] {
	derive(
		srp_shared_secret,
		b"Pair-Setup-Accessory-Sign-Salt",
		b"Pair-Setup-Accessory-Sign-Info",
	)
}

/// The pair-verify session key protecting the M2 and M3 sub-TLVs (HAP §5.7.2 step 6).
pub fn pair_verify_session_key(shared_secret: &[u8; 32]) -> [u8; 32] {
	derive(
		shared_secret,
		b"Pair-Verify-Encrypt-Salt",
		b"Pair-Verify-Encrypt-Info",
	)
}

/// Decrypt one of the pairing sub-TLVs, naming the message on failure.
pub fn open_sub_tlv(key: &[u8; 32], label: &[u8; 8], ciphertext: &[u8]) -> Result<Vec<u8>> {
	open(key, &pairing_nonce(label), &[], ciphertext).with_context(|| {
		format!(
			"failed to decrypt {} sub-TLV",
			std::str::from_utf8(label).unwrap_or("pairing")
		)
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn nonces_place_the_counter_after_four_zero_bytes() {
		assert_eq!(stream_nonce(0), [0u8; 12]);
		assert_eq!(
			stream_nonce(1),
			[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
			"counter is little endian"
		);
		assert_eq!(&pairing_nonce(b"PS-Msg05")[..4], &[0, 0, 0, 0]);
		assert_eq!(&pairing_nonce(b"PS-Msg05")[4..], b"PS-Msg05");
	}

	#[test]
	fn read_and_write_keys_differ_per_channel() {
		let secret = [7u8; 32];
		let control = SessionKeys::control(&secret);
		let event = SessionKeys::event(&secret);

		assert_ne!(control.read, control.write);
		assert_ne!(control.read, event.read);
		assert_ne!(control.write, event.write);
	}

	#[test]
	fn stream_keys_are_bound_to_the_connection_id() {
		let secret = [7u8; 32];
		assert_ne!(
			SessionKeys::data_stream(&secret, 1).read,
			SessionKeys::data_stream(&secret, 2).read
		);
	}

	#[test]
	fn seal_and_open_round_trip() {
		let key = [3u8; 32];
		let nonce = pairing_nonce(b"PV-Msg02");

		let sealed = seal(&key, &nonce, &[], b"sub-tlv").unwrap();
		assert_eq!(sealed.len(), b"sub-tlv".len() + 16, "tag is appended");
		assert_eq!(open(&key, &nonce, &[], &sealed).unwrap(), b"sub-tlv");

		let mut tampered = sealed.clone();
		tampered[0] ^= 0xFF;
		assert!(open(&key, &nonce, &[], &tampered).is_err());
	}
}
