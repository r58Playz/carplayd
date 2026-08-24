//! Pair-verify, run at the start of every session to re-authenticate a stored pairing and
//! derive the secret the whole encrypted session hangs off.
//!
//! This is HAP §5.7 unchanged: a Station-to-Station exchange over ephemeral Curve25519
//! keys, with each side signing its own ephemeral key, its own pairing id, and the peer's
//! ephemeral key using the long-term keys swapped during pair-setup.
//!
//! Note the argument order in the signed blob. Both sides sign
//! `(own ephemeral, own id, peer ephemeral)`, so the accessory's M2 signature covers a
//! different byte string than our M3 one. Getting this backwards is the classic way to
//! produce a pair-verify that fails only against real hardware.

use anyhow::{Context, Result};
use log::debug;
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::pairing::{
	identity::{Identity, verify_signature},
	keys::{open_sub_tlv, pair_verify_session_key, pairing_nonce, seal},
	setup::AccessoryLTPK,
	tlv8::{Tlv8, ty},
};

/// Pair-verify driven one message at a time; the caller does the POSTing.
pub struct PairVerify<'a> {
	identity: &'a Identity,
	accessory_ltpk: AccessoryLTPK,
	secret: StaticSecret,
	public_key: PublicKey,
	/// The Curve25519 shared secret, once M2 has been accepted.
	shared_secret: Option<[u8; 32]>,
}

impl<'a> PairVerify<'a> {
	/// Start a verify against an accessory whose long-term key we already stored.
	pub fn new(identity: &'a Identity, accessory_ltpk: AccessoryLTPK) -> Self {
		let secret = StaticSecret::random_from_rng(OsRng);
		let public_key = PublicKey::from(&secret);

		Self {
			identity,
			accessory_ltpk,
			secret,
			public_key,
			shared_secret: None,
		}
	}

	/// M1, offering our ephemeral public key.
	pub fn m1(&self) -> Vec<u8> {
		let mut tlv = Tlv8::new();
		tlv.push_u8(ty::STATE, 1)
			.push(ty::PUBLIC_KEY, self.public_key.as_bytes().to_vec());
		tlv.encode()
	}

	/// Consume M2, authenticate the accessory, and produce M3.
	pub fn m3(&mut self, m2: &[u8]) -> Result<Vec<u8>> {
		let response = Tlv8::parse(m2).context("failed to parse pair-verify M2")?;
		response.expect_state(2, "pair-verify M2")?;

		let accessory_public_key: [u8; 32] = response
			.require(ty::PUBLIC_KEY, "the accessory ephemeral public key")?
			.try_into()
			.map_err(|_| anyhow::anyhow!("accessory ephemeral public key is not 32 bytes"))?;

		let shared_secret = self
			.secret
			.diffie_hellman(&PublicKey::from(accessory_public_key))
			.to_bytes();
		let session_key = pair_verify_session_key(&shared_secret);

		let encrypted = response.require(ty::ENCRYPTED_DATA, "the M2 encrypted data")?;
		let plaintext = open_sub_tlv(&session_key, b"PV-Msg02", encrypted)?;
		let sub_tlv = Tlv8::parse(&plaintext).context("failed to parse the M2 sub-TLV")?;

		let accessory_id = sub_tlv.require(ty::IDENTIFIER, "the accessory pairing id")?;

		// AccessoryInfo = accessory ephemeral || AccessoryPairingID || our ephemeral
		let mut info = accessory_public_key.to_vec();
		info.extend_from_slice(accessory_id);
		info.extend_from_slice(self.public_key.as_bytes());

		let signature = sub_tlv.require(ty::SIGNATURE, "the accessory signature")?;
		verify_signature(&self.accessory_ltpk, &info, signature).context(
			"accessory signature did not verify; the stored long-term key does not match this \
			 head unit, so pair-setup needs to run again",
		)?;

		debug!(
			"pair-verify: authenticated accessory {}",
			String::from_utf8_lossy(accessory_id)
		);

		// iOSDeviceInfo = our ephemeral || iOSDevicePairingID || accessory ephemeral
		let mut info = self.public_key.as_bytes().to_vec();
		info.extend_from_slice(self.identity.pairing_id().as_bytes());
		info.extend_from_slice(&accessory_public_key);

		let mut sub_tlv = Tlv8::new();
		sub_tlv
			.push(
				ty::IDENTIFIER,
				self.identity.pairing_id().as_bytes().to_vec(),
			)
			.push(ty::SIGNATURE, self.identity.sign(&info).to_vec());

		let encrypted = seal(
			&session_key,
			&pairing_nonce(b"PV-Msg03"),
			&[],
			&sub_tlv.encode(),
		)
		.context("failed to encrypt the pair-verify M3 sub-TLV")?;

		self.shared_secret = Some(shared_secret);

		let mut tlv = Tlv8::new();
		tlv.push_u8(ty::STATE, 3)
			.push(ty::ENCRYPTED_DATA, encrypted);

		Ok(tlv.encode())
	}

	/// Consume M4, yielding the shared secret every session key derives from.
	///
	/// M4 carries only `kTLVType_State`; its job is to tell us the accessory accepted our
	/// signature rather than to hand over more material.
	pub fn finish(self, m4: &[u8]) -> Result<[u8; 32]> {
		let shared_secret = self
			.shared_secret
			.context("pair-verify M4 arrived before M3 was sent")?;

		let response = Tlv8::parse(m4).context("failed to parse pair-verify M4")?;
		response.expect_state(4, "pair-verify M4")?;

		Ok(shared_secret)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::host::pairing::keys::SessionKeys;

	#[test]
	fn m1_offers_a_32_byte_public_key() {
		let identity = Identity::generate();
		let verify = PairVerify::new(&identity, [0u8; 32]);

		let tlv = Tlv8::parse(&verify.m1()).unwrap();
		assert_eq!(tlv.get_u8(ty::STATE), Some(1));
		assert_eq!(tlv.get(ty::PUBLIC_KEY).map(<[u8]>::len), Some(32));
	}

	/// Stand in for the accessory across M2 through M4, which exercises the signature
	/// orderings and the fact that both sides land on the same shared secret.
	#[test]
	fn completes_against_a_reference_accessory() {
		let device = Identity::generate();
		let accessory = Identity::generate();
		let accessory_id = accessory.pairing_id().to_string();

		let mut verify = PairVerify::new(&device, accessory.public_key());

		let m1 = Tlv8::parse(&verify.m1()).unwrap();
		let device_ephemeral: [u8; 32] = m1.get(ty::PUBLIC_KEY).unwrap().try_into().unwrap();

		let accessory_secret = StaticSecret::random_from_rng(OsRng);
		let accessory_ephemeral = PublicKey::from(&accessory_secret);
		let shared = accessory_secret
			.diffie_hellman(&PublicKey::from(device_ephemeral))
			.to_bytes();
		let session_key = pair_verify_session_key(&shared);

		let mut info = accessory_ephemeral.as_bytes().to_vec();
		info.extend_from_slice(accessory_id.as_bytes());
		info.extend_from_slice(&device_ephemeral);

		let mut sub_tlv = Tlv8::new();
		sub_tlv
			.push(ty::IDENTIFIER, accessory_id.as_bytes().to_vec())
			.push(ty::SIGNATURE, accessory.sign(&info).to_vec());

		let mut m2 = Tlv8::new();
		m2.push_u8(ty::STATE, 2)
			.push(ty::PUBLIC_KEY, accessory_ephemeral.as_bytes().to_vec())
			.push(
				ty::ENCRYPTED_DATA,
				seal(
					&session_key,
					&pairing_nonce(b"PV-Msg02"),
					&[],
					&sub_tlv.encode(),
				)
				.unwrap(),
			);

		// The accessory checks our M3 the same way we checked its M2.
		let m3 = Tlv8::parse(&verify.m3(&m2.encode()).unwrap()).unwrap();
		let plaintext = open_sub_tlv(
			&session_key,
			b"PV-Msg03",
			m3.get(ty::ENCRYPTED_DATA).unwrap(),
		)
		.unwrap();
		let m3_sub = Tlv8::parse(&plaintext).unwrap();

		let mut expected = device_ephemeral.to_vec();
		expected.extend_from_slice(device.pairing_id().as_bytes());
		expected.extend_from_slice(accessory_ephemeral.as_bytes());

		verify_signature(
			&device.public_key(),
			&expected,
			m3_sub.get(ty::SIGNATURE).unwrap(),
		)
		.expect("accessory should accept our M3 signature");

		let mut m4 = Tlv8::new();
		m4.push_u8(ty::STATE, 4);

		let secret = verify.finish(&m4.encode()).unwrap();
		assert_eq!(secret, shared, "both sides derive the same shared secret");

		// And therefore the same control keys, with the roles swapped.
		let ours = SessionKeys::control(&secret);
		let theirs = SessionKeys::control(&shared);
		assert_eq!(ours.write, theirs.write);
	}

	#[test]
	fn rejects_a_signature_from_the_wrong_accessory() {
		let device = Identity::generate();
		let accessory = Identity::generate();
		let impostor = Identity::generate();

		let mut verify = PairVerify::new(&device, accessory.public_key());
		let m1 = Tlv8::parse(&verify.m1()).unwrap();
		let device_ephemeral: [u8; 32] = m1.get(ty::PUBLIC_KEY).unwrap().try_into().unwrap();

		let accessory_secret = StaticSecret::random_from_rng(OsRng);
		let accessory_ephemeral = PublicKey::from(&accessory_secret);
		let shared = accessory_secret
			.diffie_hellman(&PublicKey::from(device_ephemeral))
			.to_bytes();
		let session_key = pair_verify_session_key(&shared);

		let mut info = accessory_ephemeral.as_bytes().to_vec();
		info.extend_from_slice(impostor.pairing_id().as_bytes());
		info.extend_from_slice(&device_ephemeral);

		let mut sub_tlv = Tlv8::new();
		sub_tlv
			.push(ty::IDENTIFIER, impostor.pairing_id().as_bytes().to_vec())
			.push(ty::SIGNATURE, impostor.sign(&info).to_vec());

		let mut m2 = Tlv8::new();
		m2.push_u8(ty::STATE, 2)
			.push(ty::PUBLIC_KEY, accessory_ephemeral.as_bytes().to_vec())
			.push(
				ty::ENCRYPTED_DATA,
				seal(
					&session_key,
					&pairing_nonce(b"PV-Msg02"),
					&[],
					&sub_tlv.encode(),
				)
				.unwrap(),
			);

		assert!(verify.m3(&m2.encode()).is_err());
	}
}
