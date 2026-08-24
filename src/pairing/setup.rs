//! Pair-setup, the one-time exchange that registers us with a head unit.
//!
//! §30.3.2.1 describes CarPlay's variant as "rudimentary 'unauthenticated' pair-setup with
//! the fixed PIN provided by the device on behalf of the user", which is HAP §5.6 run with
//! `kTLVType_Method` = 0. No MFi coprocessor challenge appears in M4, so unlike HomeKit
//! there is no accessory certificate to check here.
//!
//! It runs the full M1-M6, not the two-round transient form AirPlay 2 uses: a head unit
//! trace shows three `pair-setup HK, type 0` requests, and the M5/M6 key exchange is what
//! leaves both sides holding long-term keys for later pair-verify runs.

use anyhow::{Context, Result, bail};
use log::debug;

use crate::pairing::{
	identity::{Identity, verify_signature},
	keys::{
		open_sub_tlv, pair_setup_accessory_x, pair_setup_controller_x, pair_setup_session_key,
		pairing_nonce, seal,
	},
	srp::{Session, SrpClient},
	tlv8::{PAIR_SETUP, Tlv8, ty},
};

/// The fixed PIN the CarPlay stack pairs with.
///
/// The specification only says the device supplies a fixed PIN on the user's behalf and
/// never prints the value; this is what the AirPlay/CarPlay implementations use. A wrong
/// PIN shows up as `kTLVError_Authentication` in M4.
pub const CARPLAY_PIN: &str = "3939";

pub type AccessoryLTPK = [u8; 32];
/// What we keep once pair-setup succeeds.
pub struct Pairing {
	/// `AccessoryPairingID`, which should equal the accessory's `pi` TXT value.
	pub accessory_id: String,
	/// `AccessoryLTPK`, needed to authenticate every later pair-verify.
	pub accessory_ltpk: AccessoryLTPK,
}

/// Pair-setup driven one message at a time; the caller does the POSTing.
pub struct PairSetup<'a> {
	identity: &'a Identity,
	client: SrpClient,
	session: Option<Session>,
}

impl<'a> PairSetup<'a> {
	pub fn new(identity: &'a Identity, pin: &str) -> Self {
		Self {
			identity,
			client: SrpClient::new(pin.as_bytes()),
			session: None,
		}
	}

	/// M1, the SRP start request.
	pub fn m1(&self) -> Vec<u8> {
		let mut tlv = Tlv8::new();
		tlv.push_u8(ty::STATE, 1).push_u8(ty::METHOD, PAIR_SETUP);
		tlv.encode()
	}

	/// Consume M2 and produce M3, the SRP verify request.
	pub fn m3(&mut self, m2: &[u8]) -> Result<Vec<u8>> {
		let response = Tlv8::parse(m2).context("failed to parse pair-setup M2")?;
		response.expect_state(2, "pair-setup M2")?;

		let salt = response.require(ty::SALT, "the SRP salt")?;
		let server_public_key = response.require(ty::PUBLIC_KEY, "the accessory SRP public key")?;

		let session = self
			.client
			.process_challenge(salt, server_public_key)
			.context("failed to process the accessory's SRP challenge")?;

		let mut tlv = Tlv8::new();
		tlv.push_u8(ty::STATE, 3)
			.push(ty::PUBLIC_KEY, self.client.public_key())
			.push(ty::PROOF, session.proof.to_vec());

		self.session = Some(session);

		Ok(tlv.encode())
	}

	/// Consume M4 and produce M5, the exchange request carrying our long-term key.
	pub fn m5(&mut self, m4: &[u8]) -> Result<Vec<u8>> {
		let session = self
			.session
			.as_ref()
			.context("pair-setup M4 arrived before M3 was sent")?;

		let response = Tlv8::parse(m4).context("failed to parse pair-setup M4")?;
		response.expect_state(4, "pair-setup M4")?;

		let server_proof = response.require(ty::PROOF, "the accessory SRP proof")?;
		if !session.verify_server_proof(server_proof) {
			bail!("accessory SRP proof did not verify; the head unit is not who it claims to be");
		}

		debug!("pair-setup SRP complete, exchanging long-term keys");

		// iOSDeviceInfo = iOSDeviceX || iOSDevicePairingID || iOSDeviceLTPK
		let controller_x = pair_setup_controller_x(&session.shared_secret);
		let public_key = self.identity.public_key();

		let mut info = controller_x.to_vec();
		info.extend_from_slice(self.identity.pairing_id().as_bytes());
		info.extend_from_slice(&public_key);

		let mut sub_tlv = Tlv8::new();
		sub_tlv
			.push(
				ty::IDENTIFIER,
				self.identity.pairing_id().as_bytes().to_vec(),
			)
			.push(ty::PUBLIC_KEY, public_key.to_vec())
			.push(ty::SIGNATURE, self.identity.sign(&info).to_vec());

		let session_key = pair_setup_session_key(&session.shared_secret);
		let encrypted = seal(
			&session_key,
			&pairing_nonce(b"PS-Msg05"),
			&[],
			&sub_tlv.encode(),
		)
		.context("failed to encrypt the pair-setup M5 sub-TLV")?;

		let mut tlv = Tlv8::new();
		tlv.push_u8(ty::STATE, 5)
			.push(ty::ENCRYPTED_DATA, encrypted);

		Ok(tlv.encode())
	}

	/// Consume M6, yielding the accessory's identity and long-term public key.
	pub fn finish(&self, m6: &[u8]) -> Result<Pairing> {
		let session = self
			.session
			.as_ref()
			.context("pair-setup M6 arrived before M3 was sent")?;

		let response = Tlv8::parse(m6).context("failed to parse pair-setup M6")?;
		response.expect_state(6, "pair-setup M6")?;

		let encrypted = response.require(ty::ENCRYPTED_DATA, "the M6 encrypted data")?;
		let session_key = pair_setup_session_key(&session.shared_secret);
		let plaintext = open_sub_tlv(&session_key, b"PS-Msg06", encrypted)?;

		let sub_tlv = Tlv8::parse(&plaintext).context("failed to parse the M6 sub-TLV")?;

		let accessory_id = sub_tlv.require(ty::IDENTIFIER, "the accessory pairing id")?;
		let accessory_id = std::str::from_utf8(accessory_id)
			.context("accessory pairing id is not valid UTF-8")?
			.to_string();

		let accessory_ltpk: AccessoryLTPK = sub_tlv
			.require(ty::PUBLIC_KEY, "the accessory long-term public key")?
			.try_into()
			.map_err(|_| anyhow::anyhow!("accessory long-term public key is not 32 bytes"))?;

		// AccessoryInfo = AccessoryX || AccessoryPairingID || AccessoryLTPK
		let accessory_x = pair_setup_accessory_x(&session.shared_secret);
		let mut info = accessory_x.to_vec();
		info.extend_from_slice(accessory_id.as_bytes());
		info.extend_from_slice(&accessory_ltpk);

		let signature = sub_tlv.require(ty::SIGNATURE, "the accessory signature")?;
		verify_signature(&accessory_ltpk, &info, signature)
			.context("accessory signature over AccessoryInfo did not verify")?;

		debug!("paired with accessory {accessory_id}");

		Ok(Pairing {
			accessory_id,
			accessory_ltpk,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn m1_requests_unauthenticated_pair_setup() {
		let identity = Identity::generate();
		let setup = PairSetup::new(&identity, CARPLAY_PIN);

		let tlv = Tlv8::parse(&setup.m1()).unwrap();
		assert_eq!(tlv.get_u8(ty::STATE), Some(1));
		assert_eq!(tlv.get_u8(ty::METHOD), Some(PAIR_SETUP));
		assert_eq!(
			tlv.get(ty::FLAGS),
			None,
			"CarPlay sends no pairing flags; transient/split are AirPlay 2 concerns"
		);
	}

	#[test]
	fn m3_rejects_an_error_response() {
		let identity = Identity::generate();
		let mut setup = PairSetup::new(&identity, CARPLAY_PIN);

		let mut m2 = Tlv8::new();
		m2.push_u8(ty::STATE, 2).push_u8(ty::ERROR, 0x07);

		let err = setup.m3(&m2.encode()).unwrap_err().to_string();
		assert!(err.contains("kTLVError_Busy"), "{err}");
	}

	#[test]
	fn m3_rejects_the_wrong_state() {
		let identity = Identity::generate();
		let mut setup = PairSetup::new(&identity, CARPLAY_PIN);

		let mut m2 = Tlv8::new();
		m2.push_u8(ty::STATE, 4)
			.push(ty::SALT, vec![0u8; 16])
			.push(ty::PUBLIC_KEY, vec![1u8; 384]);

		assert!(setup.m3(&m2.encode()).is_err());
	}

	#[test]
	fn m5_needs_m3_first() {
		let identity = Identity::generate();
		let mut setup = PairSetup::new(&identity, CARPLAY_PIN);

		let mut m4 = Tlv8::new();
		m4.push_u8(ty::STATE, 4).push(ty::PROOF, vec![0u8; 64]);

		assert!(setup.m5(&m4.encode()).is_err());
	}
}
