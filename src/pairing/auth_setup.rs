//! MFi-SAP, the `/auth-setup` exchange where the accessory proves it has a real Apple
//! Authentication Coprocessor.
//!
//! A different protocol from HAP pairing, despite sitting next to it in the session: §30.3.2.3
//! specifies Curve25519 for key agreement, RSA for signing, and AES-128 in counter mode for
//! encryption, where [`super::pairing`] uses SRP, Ed25519 and ChaCha20-Poly1305.
//!
//! The exchange is one round trip. We generate an ephemeral Curve25519 key pair and send our
//! public key X; the accessory generates its own pair, derives the shared secret, signs its
//! public key Y concatenated with our X using the coprocessor's RSA key, encrypts that
//! signature under a key derived from the shared secret, and returns Y, its MFi certificate
//! and the encrypted signature.
//!
//! # What this module does and does not check
//!
//! We are emulating the iOS device, so we are the verifier. A real device would validate the
//! certificate chains to the Apple root with a class between 0x00 and 0x0A, then check the RSA
//! signature with the public key inside it. This module parses the response, derives the shared
//! secret, decrypts the signature and structurally sanity-checks the certificate, but does not
//! do either cryptographic check — see [`AccessoryAuth::signed_message`] for what you would
//! feed a verifier if you added one.
//!
//! That is a deliberate limit, not an oversight. It also costs less than it looks: unlike
//! HomeKit, where the coprocessor proof is folded into pair-setup M4, CarPlay keeps MFi-SAP
//! entirely separate, and §30.3.2.2 derives every control, event and media key from the
//! *pair-verify* shared secret. Nothing downstream consumes anything produced here, so a
//! session works whether or not the signature is checked.

use aes::Aes128;
use anyhow::{Context, Result, bail};
use ctr::{
	Ctr128BE,
	cipher::{KeyIvInit, StreamCipher},
};
use log::debug;
use rand::rngs::OsRng;
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey, StaticSecret};

type Aes128Ctr = Ctr128BE<Aes128>;

/// The only method selector this accessory accepts.
///
/// The first body byte selects the MFi-SAP variant. Probing a head unit with 0x00, 0x02 and
/// 0x03 returned `403 Forbidden` for each, while 0x01 returned a full certificate.
pub const METHOD_MFI_SAP: u8 = 0x01;

/// The RSA signature an Authentication Coprocessor produces, in bytes.
const SIGNATURE_LEN: usize = 128;

/// Guard against a malformed length prefix turning into a huge allocation. Observed
/// certificates are ~900 bytes.
const MAX_FIELD_LEN: usize = 8 * 1024;

/// DER for OID 1.2.840.113549.1.7.2, `signedData`, which every MFi certificate blob opens
/// with once past the outer SEQUENCE header.
const PKCS7_SIGNED_DATA_OID: &[u8] = &[0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];

/// Derive the AES-128-CTR key and initial counter block from the Curve25519 shared secret.
///
/// §30.3.2.3 names the algorithm ("AES-128 in counter mode") and says the key is "derived from
/// the Curve25519 shared secret", but does not print the derivation. These two labels are the
/// values the AirPlay/CarPlay implementations use; they are reverse-engineered, not specified.
///
/// Nothing here can tell you whether they are right. The output is a 128-byte RSA signature,
/// which is indistinguishable from noise unless you go on to verify it against the public key
/// in the certificate. If you ever add that check and it fails, suspect these labels before
/// suspecting the signature.
fn derive_aes_params(shared_secret: &[u8; 32]) -> ([u8; 16], [u8; 16]) {
	fn first16(label: &[u8], shared_secret: &[u8; 32]) -> [u8; 16] {
		let mut hasher = Sha512::new();
		hasher.update(label);
		hasher.update(shared_secret);

		let mut out = [0u8; 16];
		out.copy_from_slice(&hasher.finalize()[..16]);
		out
	}

	(
		first16(b"AES-KEY", shared_secret),
		first16(b"AES-IV", shared_secret),
	)
}

/// A weak structural check that a blob looks like an MFi certificate.
///
/// Confirms it is a DER SEQUENCE wrapping a PKCS#7 `signedData`, which is the shape §5.6.4 step
/// (d) of HAP describes the coprocessor as returning. This says nothing about authenticity; it
/// only catches a garbled response early, where the alternative is a confusing failure later.
pub fn looks_like_mfi_certificate(certificate: &[u8]) -> bool {
	certificate.first() == Some(&0x30)
		&& certificate
			.windows(PKCS7_SIGNED_DATA_OID.len())
			.take(16)
			.any(|window| window == PKCS7_SIGNED_DATA_OID)
}

/// What the accessory returned, with the signature decrypted.
pub struct AccessoryAuth {
	/// The accessory's ephemeral Curve25519 public key, Y.
	pub accessory_public_key: [u8; 32],
	/// The MFi certificate, a DER-encoded X.509 inside a PKCS#7 message.
	pub certificate: Vec<u8>,
	/// The RSA signature, after AES-128-CTR decryption. 128 bytes in practice.
	pub signature: Vec<u8>,
	/// Our ephemeral Curve25519 public key, X. Kept because it is half of the signed message.
	pub device_public_key: [u8; 32],
	/// The Curve25519 shared secret.
	///
	/// Exposed for debugging only. CarPlay derives no session keys from it: §30.3.2.2 keys the
	/// control, event and media channels off the pair-verify secret instead.
	pub shared_secret: [u8; 32],
}

/// Sizes and the public keys only; the shared secret stays out of logs.
impl std::fmt::Debug for AccessoryAuth {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("AccessoryAuth")
			.field("accessory_public_key", &hex(&self.accessory_public_key))
			.field("device_public_key", &hex(&self.device_public_key))
			.field("certificate", &format_args!("{} bytes", self.certificate.len()))
			.field("signature", &format_args!("{} bytes", self.signature.len()))
			.field("shared_secret", &"<redacted>")
			.finish()
	}
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl AccessoryAuth {
	/// The bytes the accessory's RSA signature covers.
	///
	/// §30.3.2.3 describes the accessory as sending "an encryption of Signature(Y,X)" and signing
	/// "the two public keys", so the message is Y followed by X — accessory first, ours second.
	pub fn signed_message(&self) -> Vec<u8> {
		let mut message = self.accessory_public_key.to_vec();
		message.extend_from_slice(&self.device_public_key);
		message
	}
}

/// One `/auth-setup` exchange.
pub struct AuthSetup {
	secret: StaticSecret,
	public_key: PublicKey,
}

impl Default for AuthSetup {
	fn default() -> Self {
		Self::new()
	}
}

impl AuthSetup {
	/// Generate the ephemeral key pair for a fresh exchange.
	pub fn new() -> Self {
		let secret = StaticSecret::random_from_rng(OsRng);
		let public_key = PublicKey::from(&secret);

		Self { secret, public_key }
	}

	/// The request body: the method selector followed by our 32-byte public key.
	///
	/// POST it to `/auth-setup` as `application/octet-stream`.
	pub fn request(&self) -> Vec<u8> {
		let mut body = Vec::with_capacity(1 + 32);
		body.push(METHOD_MFI_SAP);
		body.extend_from_slice(self.public_key.as_bytes());
		body
	}

	/// Parse the response and decrypt the signature.
	///
	/// The body is a bare concatenation with no framing beyond two length prefixes:
	///
	/// ```text
	/// <32: accessory Curve25519 public key>
	/// <4: big endian certificate length><n: certificate>
	/// <4: big endian signature length><m: AES-128-CTR encrypted signature>
	/// ```
	///
	/// §30.3.2.3 calls the payload "opaque security data" and does not document this layout; it
	/// comes from a head unit that answers with 32 + 4 + 908 + 4 + 128 = 1076 bytes.
	pub fn process_response(self, body: &[u8]) -> Result<AccessoryAuth> {
		let mut reader = Reader::new(body);

		let accessory_public_key: [u8; 32] = reader
			.take(32)
			.context("response is too short for the accessory public key")?
			.try_into()
			.expect("take(32) yields 32 bytes");

		let certificate = reader.take_length_prefixed("certificate")?.to_vec();
		let mut signature = reader.take_length_prefixed("signature")?.to_vec();

		if !reader.is_empty() {
			bail!(
				"{} trailing byte(s) after the auth-setup signature",
				reader.remaining()
			);
		}

		if !looks_like_mfi_certificate(&certificate) {
			bail!(
				"auth-setup certificate is not a PKCS#7 signedData blob ({} bytes)",
				certificate.len()
			);
		}

		if signature.len() != SIGNATURE_LEN {
			debug!(
				"auth-setup signature is {} bytes, expected {SIGNATURE_LEN}",
				signature.len()
			);
		}

		let shared_secret = self
			.secret
			.diffie_hellman(&PublicKey::from(accessory_public_key))
			.to_bytes();

		let (key, iv) = derive_aes_params(&shared_secret);
		Aes128Ctr::new(&key.into(), &iv.into()).apply_keystream(&mut signature);

		debug!(
			"auth-setup: {} byte certificate, {} byte signature",
			certificate.len(),
			signature.len()
		);

		Ok(AccessoryAuth {
			accessory_public_key,
			certificate,
			signature,
			device_public_key: self.public_key.to_bytes(),
			shared_secret,
		})
	}
}

/// A cursor over the response body.
struct Reader<'a> {
	data: &'a [u8],
	offset: usize,
}

impl<'a> Reader<'a> {
	fn new(data: &'a [u8]) -> Self {
		Self { data, offset: 0 }
	}

	fn remaining(&self) -> usize {
		self.data.len() - self.offset
	}

	fn is_empty(&self) -> bool {
		self.remaining() == 0
	}

	fn take(&mut self, count: usize) -> Option<&'a [u8]> {
		let slice = self.data.get(self.offset..self.offset + count)?;
		self.offset += count;
		Some(slice)
	}

	fn take_length_prefixed(&mut self, field: &str) -> Result<&'a [u8]> {
		let length = self
			.take(4)
			.with_context(|| format!("response is too short for the {field} length"))?;
		let length = u32::from_be_bytes(length.try_into().expect("take(4) yields 4 bytes")) as usize;

		if length > MAX_FIELD_LEN {
			bail!("auth-setup {field} claims {length} bytes, which is implausible");
		}

		self.take(length)
			.with_context(|| format!("response is too short for the {field} body"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The certificate prefix from a real head unit response, enough to satisfy the
	/// structural check.
	fn certificate_stub(len: usize) -> Vec<u8> {
		let mut cert = vec![0x30, 0x82, 0x03, 0x88];
		cert.extend_from_slice(PKCS7_SIGNED_DATA_OID);
		cert.resize(len, 0x41);
		cert
	}

	fn response(accessory_public_key: &[u8; 32], cert: &[u8], signature: &[u8]) -> Vec<u8> {
		let mut body = accessory_public_key.to_vec();
		body.extend_from_slice(&(cert.len() as u32).to_be_bytes());
		body.extend_from_slice(cert);
		body.extend_from_slice(&(signature.len() as u32).to_be_bytes());
		body.extend_from_slice(signature);
		body
	}

	#[test]
	fn request_is_the_method_byte_and_our_key() {
		let auth = AuthSetup::new();
		let request = auth.request();

		assert_eq!(request.len(), 33);
		assert_eq!(request[0], METHOD_MFI_SAP);
		assert_eq!(&request[1..], auth.public_key.as_bytes());
	}

	/// The exact field sizes a head unit answered with: 32 + 4 + 908 + 4 + 128.
	#[test]
	fn parses_a_real_shaped_response() {
		let accessory_secret = StaticSecret::random_from_rng(OsRng);
		let accessory_public = PublicKey::from(&accessory_secret);

		let cert = certificate_stub(908);
		let body = response(accessory_public.as_bytes(), &cert, &[0x5Au8; SIGNATURE_LEN]);
		assert_eq!(body.len(), 1076);

		let auth = AuthSetup::new();
		let device_public = auth.public_key.to_bytes();
		let result = auth.process_response(&body).unwrap();

		assert_eq!(result.certificate.len(), 908);
		assert_eq!(result.signature.len(), SIGNATURE_LEN);
		assert_eq!(result.accessory_public_key, accessory_public.to_bytes());
		assert_eq!(
			result.shared_secret,
			accessory_secret
				.diffie_hellman(&PublicKey::from(device_public))
				.to_bytes(),
			"both sides agree on the Curve25519 secret"
		);
	}

	#[test]
	fn signed_message_is_accessory_key_then_ours() {
		let accessory_public = PublicKey::from(&StaticSecret::random_from_rng(OsRng));
		let body = response(
			accessory_public.as_bytes(),
			&certificate_stub(908),
			&[0u8; SIGNATURE_LEN],
		);

		let auth = AuthSetup::new();
		let device_public = auth.public_key.to_bytes();
		let result = auth.process_response(&body).unwrap();

		let message = result.signed_message();
		assert_eq!(&message[..32], accessory_public.as_bytes());
		assert_eq!(&message[32..], &device_public);
	}

	/// CTR is its own inverse, so decrypting with the peer's view of the secret recovers the
	/// plaintext. This checks the key schedule agrees end to end.
	#[test]
	fn decryption_matches_the_accessorys_encryption() {
		let accessory_secret = StaticSecret::random_from_rng(OsRng);
		let accessory_public = PublicKey::from(&accessory_secret);

		let auth = AuthSetup::new();
		let device_public = auth.public_key.to_bytes();

		let shared = accessory_secret
			.diffie_hellman(&PublicKey::from(device_public))
			.to_bytes();
		let (key, iv) = derive_aes_params(&shared);

		let plaintext = [0xC3u8; SIGNATURE_LEN];
		let mut ciphertext = plaintext;
		Aes128Ctr::new(&key.into(), &iv.into()).apply_keystream(&mut ciphertext);
		assert_ne!(ciphertext, plaintext);

		let body = response(accessory_public.as_bytes(), &certificate_stub(908), &ciphertext);
		let result = auth.process_response(&body).unwrap();

		assert_eq!(result.signature, plaintext);
	}

	#[test]
	fn key_and_iv_are_distinct_and_secret_dependent() {
		let (key_a, iv_a) = derive_aes_params(&[1u8; 32]);
		let (key_b, iv_b) = derive_aes_params(&[2u8; 32]);

		assert_ne!(key_a, iv_a);
		assert_ne!(key_a, key_b);
		assert_ne!(iv_a, iv_b);
	}

	#[test]
	fn rejects_a_truncated_response() {
		let auth = AuthSetup::new();
		assert!(auth.process_response(&[0u8; 20]).is_err());

		let mut body = response(&[0u8; 32], &certificate_stub(908), &[0u8; SIGNATURE_LEN]);
		body.truncate(body.len() - 10);
		assert!(AuthSetup::new().process_response(&body).is_err());
	}

	#[test]
	fn rejects_an_implausible_length_prefix() {
		let mut body = [0u8; 32].to_vec();
		body.extend_from_slice(&u32::MAX.to_be_bytes());
		body.extend_from_slice(&[0u8; 16]);

		let err = AuthSetup::new().process_response(&body).unwrap_err().to_string();
		assert!(err.contains("implausible"), "{err}");
	}

	#[test]
	fn rejects_trailing_bytes() {
		let mut body = response(&[0u8; 32], &certificate_stub(908), &[0u8; SIGNATURE_LEN]);
		body.push(0xFF);

		assert!(AuthSetup::new().process_response(&body).is_err());
	}

	#[test]
	fn rejects_a_certificate_that_is_not_pkcs7() {
		let body = response(&[0u8; 32], &vec![0x41u8; 908], &[0u8; SIGNATURE_LEN]);

		let err = AuthSetup::new().process_response(&body).unwrap_err().to_string();
		assert!(err.contains("PKCS#7"), "{err}");
	}

	#[test]
	fn certificate_check_accepts_the_real_prefix() {
		assert!(looks_like_mfi_certificate(&certificate_stub(908)));
		assert!(!looks_like_mfi_certificate(b"not a certificate"));
		assert!(!looks_like_mfi_certificate(&[]));
	}
}
