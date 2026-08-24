//! Our long-term pairing identity, and the accessory keys we learn from pair-setup.
//!
//! Both halves must outlive the process. The accessory persists our pairing (HAP §5.6.6.1
//! step 6) and has a bounded table it answers `kTLVError_MaxPeers` from, so regenerating
//! an identity every run would slowly fill the head unit up with dead entries. Coming back
//! with the same identity is also what lets a reconnect skip pair-setup and go straight to
//! pair-verify.

use std::{
	collections::HashMap,
	fs,
	path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use log::{debug, info};
use rand::{RngCore, rngs::OsRng};

/// Format 16 random bytes the way iOS formats its pairing identifier.
fn random_uuid() -> String {
	let mut bytes = [0u8; 16];
	OsRng.fill_bytes(&mut bytes);

	// Version 4, variant 1, matching what a real device advertises.
	bytes[6] = (bytes[6] & 0x0F) | 0x40;
	bytes[8] = (bytes[8] & 0x3F) | 0x80;

	let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
	format!(
		"{}-{}-{}-{}-{}",
		&hex[0..8],
		&hex[8..12],
		&hex[12..16],
		&hex[16..20],
		&hex[20..32]
	)
}

fn to_hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Result<Vec<u8>> {
	if !text.len().is_multiple_of(2) {
		bail!("odd-length hex string");
	}

	(0..text.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(&text[i..i + 2], 16).context("invalid hex digit"))
		.collect()
}

/// The identity we present as the iOS device: `iOSDevicePairingID` plus the Ed25519
/// long-term key pair behind it.
pub struct Identity {
	pairing_id: String,
	signing_key: SigningKey,
}

impl Identity {
	pub fn generate() -> Self {
		Self {
			pairing_id: random_uuid(),
			signing_key: SigningKey::generate(&mut OsRng),
		}
	}

	/// `iOSDevicePairingID`, sent in pair-setup M5 and pair-verify M3.
	pub fn pairing_id(&self) -> &str {
		&self.pairing_id
	}

	/// `iOSDeviceLTPK`.
	pub fn public_key(&self) -> [u8; 32] {
		self.signing_key.verifying_key().to_bytes()
	}

	/// Sign with `iOSDeviceLTSK`.
	pub fn sign(&self, message: &[u8]) -> [u8; 64] {
		self.signing_key.sign(message).to_bytes()
	}

	/// Load from disk, generating and saving a fresh identity when the file is absent.
	pub fn load_or_generate(path: &Path) -> Result<Self> {
		match fs::read_to_string(path) {
			Ok(contents) => {
				let identity = Self::parse(&contents)
					.with_context(|| format!("failed to parse identity at {}", path.display()))?;
				debug!("loaded pairing identity {}", identity.pairing_id);
				Ok(identity)
			}
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
				let identity = Self::generate();
				identity.save(path)?;
				info!("generated pairing identity {}", identity.pairing_id);
				Ok(identity)
			}
			Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
		}
	}

	fn parse(contents: &str) -> Result<Self> {
		let mut lines = contents.lines();
		let pairing_id = lines.next().context("missing pairing id line")?.trim();
		let seed_hex = lines.next().context("missing signing key line")?.trim();

		let seed: [u8; 32] = from_hex(seed_hex)?
			.try_into()
			.map_err(|_| anyhow::anyhow!("signing key is not 32 bytes"))?;

		Ok(Self {
			pairing_id: pairing_id.to_string(),
			signing_key: SigningKey::from_bytes(&seed),
		})
	}

	fn save(&self, path: &Path) -> Result<()> {
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)
				.with_context(|| format!("failed to create {}", parent.display()))?;
		}

		let contents = format!(
			"{}\n{}\n",
			self.pairing_id,
			to_hex(self.signing_key.as_bytes())
		);

		fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
	}
}

/// Accessory long-term public keys, keyed by `AccessoryPairingID`.
///
/// The ID is the accessory's `pi` TXT value where it advertises one; CarPlay's Bonjour
/// record carries no `pk`, unlike plain AirPlay 2, so the key itself can only come out of
/// pair-setup M6.
pub struct PairingStore {
	path: PathBuf,
	accessories: HashMap<String, [u8; 32]>,
}

impl PairingStore {
	/// Load the store, treating a missing file as empty.
	pub fn load(path: &Path) -> Result<Self> {
		let mut accessories = HashMap::new();

		match fs::read_to_string(path) {
			Ok(contents) => {
				for (number, line) in contents.lines().enumerate() {
					let line = line.trim();
					if line.is_empty() {
						continue;
					}

					let (id, key_hex) = line
						.split_once(' ')
						.with_context(|| format!("malformed pairing on line {}", number + 1))?;

					let key: [u8; 32] = from_hex(key_hex)?.try_into().map_err(|_| {
						anyhow::anyhow!("accessory key on line {} is not 32 bytes", number + 1)
					})?;

					accessories.insert(id.to_string(), key);
				}
			}
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
			Err(err) => {
				return Err(err).with_context(|| format!("failed to read {}", path.display()));
			}
		}

		debug!("loaded {} known accessory pairing(s)", accessories.len());

		Ok(Self {
			path: path.to_path_buf(),
			accessories,
		})
	}

	/// `AccessoryLTPK` for an accessory we have already paired with.
	pub fn get(&self, accessory_id: &str) -> Option<[u8; 32]> {
		self.accessories.get(accessory_id).copied()
	}

	/// Record a pairing from pair-setup M6 and flush the store.
	pub fn insert(&mut self, accessory_id: &str, ltpk: [u8; 32]) -> Result<()> {
		self.accessories.insert(accessory_id.to_string(), ltpk);
		self.save()
	}

	fn save(&self) -> Result<()> {
		if let Some(parent) = self.path.parent() {
			fs::create_dir_all(parent)
				.with_context(|| format!("failed to create {}", parent.display()))?;
		}

		let mut contents = String::new();
		for (id, key) in &self.accessories {
			contents.push_str(&format!("{id} {}\n", to_hex(key)));
		}

		fs::write(&self.path, contents)
			.with_context(|| format!("failed to write {}", self.path.display()))
	}
}

/// Check an Ed25519 signature against a raw 32-byte public key.
pub fn verify_signature(public_key: &[u8; 32], message: &[u8], signature: &[u8]) -> Result<()> {
	let signature: [u8; 64] = signature
		.try_into()
		.map_err(|_| anyhow::anyhow!("signature is {} bytes, want 64", signature.len()))?;

	VerifyingKey::from_bytes(public_key)
		.context("accessory public key is not a valid Ed25519 point")?
		.verify(message, &Signature::from_bytes(&signature))
		.context("Ed25519 signature verification failed")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn uuid_has_the_expected_shape() {
		let id = random_uuid();
		assert_eq!(id.len(), 36);
		assert_eq!(id.as_bytes()[14], b'4');
		assert_eq!(id.match_indices('-').count(), 4);
	}

	#[test]
	fn hex_round_trips() {
		let bytes = [0x00u8, 0x0f, 0xa5, 0xff];
		assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
		assert!(from_hex("abc").is_err());
		assert!(from_hex("zz").is_err());
	}

	#[test]
	fn identity_survives_a_round_trip() {
		let identity = Identity::generate();
		let encoded = format!(
			"{}\n{}\n",
			identity.pairing_id,
			to_hex(identity.signing_key.as_bytes())
		);

		let loaded = Identity::parse(&encoded).unwrap();
		assert_eq!(loaded.pairing_id(), identity.pairing_id());
		assert_eq!(loaded.public_key(), identity.public_key());
	}

	#[test]
	fn signatures_verify_against_our_own_key() {
		let identity = Identity::generate();
		let signature = identity.sign(b"iOSDeviceInfo");

		assert!(verify_signature(&identity.public_key(), b"iOSDeviceInfo", &signature).is_ok());
		assert!(verify_signature(&identity.public_key(), b"tampered", &signature).is_err());
	}
}
