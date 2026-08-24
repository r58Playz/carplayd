//! SRP-6a client for HAP pair-setup.
//!
//! HAP R13 §5.6 pins the parameters: the 3072-bit group from RFC 5054 (so g = 5), with
//! SHA-512 replacing SHA-1 throughout, and the username fixed to `Pair-Setup`. We are
//! always the client here, since we are emulating the iOS device.

use anyhow::{Result, bail};
use num_bigint::{BigUint, RandBigInt};
use rand::rngs::OsRng;
use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;

/// Width of N, and therefore of every padded field, in bytes.
const N_BYTES: usize = 384;

/// The SRP username HAP requires (§5.6.2 step 5, §5.6.3 step 2).
pub const USERNAME: &[u8] = b"Pair-Setup";

/// RFC 5054 3072-bit group modulus.
const N_HEX: &str = concat!(
	"FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E08",
	"8A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B",
	"302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9",
	"A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE6",
	"49286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8",
	"FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D",
	"670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C",
	"180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
	"3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D",
	"04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7D",
	"B3970F85A6E1E4C7ABF5AE8CDB0933D71E8C94E04A25619DCEE3D226",
	"1AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200C",
	"BBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFC",
	"E0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF"
);

fn modulus() -> BigUint {
	BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("RFC 5054 constant is valid hex")
}

fn generator() -> BigUint {
	BigUint::from(5u32)
}

/// Left-pad to the width of N, as every SRP hash input is specified to be.
fn pad(value: &BigUint) -> Vec<u8> {
	let bytes = value.to_bytes_be();

	if bytes.len() >= N_BYTES {
		bytes[bytes.len() - N_BYTES..].to_vec()
	} else {
		let mut padded = vec![0u8; N_BYTES - bytes.len()];
		padded.extend_from_slice(&bytes);
		padded
	}
}

fn sha512(parts: &[&[u8]]) -> [u8; 64] {
	let mut hasher = Sha512::new();
	for part in parts {
		hasher.update(part);
	}
	hasher.finalize().into()
}

/// The outcome of processing the accessory's M2.
pub struct Session {
	/// M1, the proof we send in pair-setup M3.
	pub proof: [u8; 64],
	/// K = H(S). This is the "SRP shared secret" every later HKDF is keyed on.
	pub shared_secret: [u8; 64],
	/// The M2 we expect back from the accessory in pair-setup M4.
	expected_server_proof: [u8; 64],
}

impl Session {
	/// Constant-time check of the accessory's proof.
	pub fn verify_server_proof(&self, proof: &[u8]) -> bool {
		proof.ct_eq(&self.expected_server_proof).into()
	}
}

/// Client-side SRP state: everything fixed before the accessory replies.
pub struct SrpClient {
	n: BigUint,
	g: BigUint,
	password: Vec<u8>,
	a: BigUint,
	a_pub: BigUint,
}

impl SrpClient {
	/// Generate a fresh private exponent and the matching public key A = g^a mod N.
	pub fn new(password: &[u8]) -> Self {
		let n = modulus();
		let g = generator();
		let a = OsRng.gen_biguint(256);
		let a_pub = g.modpow(&a, &n);

		Self {
			n,
			g,
			password: password.to_vec(),
			a,
			a_pub,
		}
	}

	/// A, padded to 384 bytes for `kTLVType_PublicKey`.
	pub fn public_key(&self) -> Vec<u8> {
		pad(&self.a_pub)
	}

	/// Consume the accessory's salt and B, producing our proof and the shared secret.
	pub fn process_challenge(&self, salt: &[u8], server_public_key: &[u8]) -> Result<Session> {
		let b_pub = BigUint::from_bytes_be(server_public_key);

		if (&b_pub % &self.n) == BigUint::ZERO {
			bail!("accessory SRP public key is zero mod N");
		}

		// u = H(PAD(A) || PAD(B))
		let u = BigUint::from_bytes_be(&sha512(&[&pad(&self.a_pub), &pad(&b_pub)]));
		if u == BigUint::ZERO {
			bail!("SRP u parameter is zero");
		}

		// x = H(salt || H(username || ":" || password))
		let inner = sha512(&[USERNAME, b":", &self.password]);
		let x = BigUint::from_bytes_be(&sha512(&[salt, &inner]));

		// k = H(N || PAD(g))
		let k = BigUint::from_bytes_be(&sha512(&[&pad(&self.n), &pad(&self.g)]));

		// S = (B - k * g^x) ^ (a + u * x) mod N, taking care not to underflow.
		let g_x = self.g.modpow(&x, &self.n);
		let k_gx = (&k * &g_x) % &self.n;
		let base = if b_pub >= k_gx {
			(&b_pub - &k_gx) % &self.n
		} else {
			(&b_pub + &self.n - &k_gx) % &self.n
		};
		let exponent = (&self.a + &u * &x) % (&self.n - BigUint::from(1u32));
		let s = base.modpow(&exponent, &self.n);

		// K = H(PAD(S)). HAP keys every later derivation on K, not on S itself.
		let shared_secret = sha512(&[&pad(&s)]);

		// M1 = H(H(N) XOR H(g) || H(username) || salt || PAD(A) || PAD(B) || K)
		//
		// H(g) is taken over the bare generator byte 0x05, *not* over PAD(g), even
		// though k above does pad it. Padding here yields a proof the accessory
		// rejects with kTLVError_Authentication in M4, which looks exactly like a
		// wrong PIN and is miserable to chase down.
		let h_n = sha512(&[&pad(&self.n)]);
		let h_g = sha512(&[&self.g.to_bytes_be()]);
		let mut h_ng = [0u8; 64];
		for i in 0..64 {
			h_ng[i] = h_n[i] ^ h_g[i];
		}

		let h_username = sha512(&[USERNAME]);
		let proof = sha512(&[
			&h_ng,
			&h_username,
			salt,
			&pad(&self.a_pub),
			&pad(&b_pub),
			&shared_secret,
		]);

		// M2 = H(PAD(A) || M1 || K)
		let expected_server_proof = sha512(&[&pad(&self.a_pub), &proof, &shared_secret]);

		Ok(Session {
			proof,
			shared_secret,
			expected_server_proof,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn modulus_is_3072_bits() {
		assert_eq!(modulus().to_bytes_be().len(), N_BYTES);
	}

	#[test]
	fn public_key_is_padded_to_n() {
		let client = SrpClient::new(b"3939");
		assert_eq!(client.public_key().len(), N_BYTES);
	}

	#[test]
	fn rejects_degenerate_server_key() {
		let client = SrpClient::new(b"3939");
		let zero = vec![0u8; N_BYTES];
		assert!(client.process_challenge(&[0u8; 16], &zero).is_err());
	}

	/// A full client/server run against a locally computed verifier, which checks the
	/// whole S/K/M1/M2 chain hangs together.
	#[test]
	fn agrees_with_a_reference_server() {
		let n = modulus();
		let g = generator();
		let salt = [0x42u8; 16];
		let password = b"3939";

		let inner = sha512(&[USERNAME, b":", password.as_slice()]);
		let x = BigUint::from_bytes_be(&sha512(&[&salt, &inner]));
		let v = g.modpow(&x, &n);

		let k = BigUint::from_bytes_be(&sha512(&[&pad(&n), &pad(&g)]));
		let b = BigUint::from(0x1234_5678u32);
		let b_pub = (&k * &v + g.modpow(&b, &n)) % &n;

		let client = SrpClient::new(password);
		let session = client.process_challenge(&salt, &pad(&b_pub)).unwrap();

		let a_pub = BigUint::from_bytes_be(&client.public_key());
		let u = BigUint::from_bytes_be(&sha512(&[&pad(&a_pub), &pad(&b_pub)]));
		let s = (&a_pub * v.modpow(&u, &n)).modpow(&b, &n);

		assert_eq!(session.shared_secret, sha512(&[&pad(&s)]));

		let server_proof = sha512(&[&pad(&a_pub), &session.proof, &session.shared_secret]);
		assert!(session.verify_server_proof(&server_proof));
		assert!(!session.verify_server_proof(&[0u8; 64]));
	}
}
