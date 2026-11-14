//! The SRP-6a client Apple uses for Apple ID sign-in.
//!
//! The password never leaves the machine: we derive a key from it, prove we
//! know that key to Apple, and check Apple's proof back.
//!
//! Parameters match what icloud.com uses (and what `pysrp` produces with
//! `rfc5054_enable()` + `no_username_in_x()`): the 2048-bit group from
//! RFC 5054, SHA-256, `k` and `u` hashed over padded values, and `x` computed
//! without the account name.

use num_bigint::BigUint;
use sha2::{Digest, Sha256};

/// RFC 5054 2048-bit group prime.
const N_HEX: &[u8] = b"\
AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4\
A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF60\
95179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF\
747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B907\
8717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB37861\
60279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DB\
FBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";
const G: u32 = 2;

/// Which password-hashing variant Apple asked for in `signin/init`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// PBKDF2 over the raw SHA-256 digest of the password.
    S2k,
    /// PBKDF2 over the hex-encoded SHA-256 digest of the password.
    S2kFo,
}

impl Protocol {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "s2k" => Some(Self::S2k),
            "s2k_fo" => Some(Self::S2kFo),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SrpError {
    #[error("Apple sent an invalid sign-in challenge")]
    InvalidChallenge,
}

/// Derive the SRP password key from the user's password.
pub fn derive_password_key(
    password: &str,
    salt: &[u8],
    iterations: u32,
    protocol: Protocol,
) -> [u8; 32] {
    let digest = Sha256::digest(password.as_bytes());
    let input: Vec<u8> = match protocol {
        Protocol::S2k => digest.to_vec(),
        Protocol::S2kFo => hex(&digest).into_bytes(),
    };
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(&input, salt, iterations, &mut key);
    key
}

/// The two proofs sent to `signin/complete`.
pub struct Proof {
    /// Client proof (`m1`).
    pub m1: Vec<u8>,
    /// What the server's proof must equal (`m2`; Apple wants it sent too).
    pub m2: Vec<u8>,
}

pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    big_a: BigUint,
}

impl Default for SrpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SrpClient {
    /// A client with a fresh random secret.
    pub fn new() -> Self {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret);
        Self::with_secret(&secret)
    }

    fn with_secret(secret: &[u8]) -> Self {
        let n = group_prime();
        let g = BigUint::from(G);
        let a = BigUint::from_bytes_be(secret);
        let big_a = g.modpow(&a, &n);
        Self { n, g, a, big_a }
    }

    /// Public value `A`, sent to `signin/init`.
    pub fn public_key(&self) -> Vec<u8> {
        self.big_a.to_bytes_be()
    }

    /// Compute the proofs from the server's salt and public value `B`.
    pub fn process_challenge(
        &self,
        account_name: &str,
        password_key: &[u8],
        salt: &[u8],
        server_public: &[u8],
    ) -> Result<Proof, SrpError> {
        let n = &self.n;
        let width = n.to_bytes_be().len();
        let big_b = BigUint::from_bytes_be(server_public);
        if (&big_b % n) == BigUint::ZERO {
            return Err(SrpError::InvalidChallenge);
        }

        let u = BigUint::from_bytes_be(&hash(&[&pad(&self.big_a, width), &pad(&big_b, width)]));
        if u == BigUint::ZERO {
            return Err(SrpError::InvalidChallenge);
        }

        let k = multiplier(n, &self.g, width);
        let x = private_key(salt, password_key);
        let v = self.g.modpow(&x, n);

        // S = (B - k*v) ^ (a + u*x) mod N, kept non-negative.
        let kv = (&k * &v) % n;
        let base = ((&big_b % n) + n - kv) % n;
        let s = base.modpow(&(&self.a + &u * &x), n);
        let session_key = hash(&[&s.to_bytes_be()]);

        let m1 = client_proof(
            n,
            &self.g,
            width,
            account_name,
            salt,
            &self.big_a,
            &big_b,
            &session_key,
        );
        let m2 = hash(&[&self.big_a.to_bytes_be(), &m1, &session_key]);
        Ok(Proof { m1, m2 })
    }
}

fn group_prime() -> BigUint {
    BigUint::parse_bytes(N_HEX, 16).expect("valid group prime")
}

/// k = H(N | PAD(g))
fn multiplier(n: &BigUint, g: &BigUint, width: usize) -> BigUint {
    BigUint::from_bytes_be(&hash(&[&n.to_bytes_be(), &pad(g, width)]))
}

/// x = H(salt | H(":" | password_key)); the account name is left out.
fn private_key(salt: &[u8], password_key: &[u8]) -> BigUint {
    let inner = hash(&[b":", password_key]);
    BigUint::from_bytes_be(&hash(&[salt, &inner]))
}

/// M1 = H(H(N) xor H(PAD(g)) | H(I) | salt | A | B | K)
#[allow(clippy::too_many_arguments)]
fn client_proof(
    n: &BigUint,
    g: &BigUint,
    width: usize,
    account_name: &str,
    salt: &[u8],
    big_a: &BigUint,
    big_b: &BigUint,
    session_key: &[u8],
) -> Vec<u8> {
    let hn = hash(&[&n.to_bytes_be()]);
    let hg = hash(&[&pad(g, width)]);
    let n_xor_g: Vec<u8> = hn.iter().zip(&hg).map(|(a, b)| a ^ b).collect();
    let hi = hash(&[account_name.as_bytes()]);
    hash(&[
        &n_xor_g,
        &hi,
        salt,
        &big_a.to_bytes_be(),
        &big_b.to_bytes_be(),
        session_key,
    ])
}

fn hash(parts: &[&[u8]]) -> Vec<u8> {
    let mut h = Sha256::new();
    for part in parts {
        h.update(part);
    }
    h.finalize().to_vec()
}

/// Left-pad with zeros to `width` bytes.
fn pad(value: &BigUint, width: usize) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    let mut out = vec![0u8; width.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Play the server's side and check both sides agree on the proofs.
    #[test]
    fn client_and_server_agree() {
        let account = "someone@example.com";
        let salt = b"0123456789abcdef";
        let key = derive_password_key("correct horse", salt, 1000, Protocol::S2k);

        let client = SrpClient::with_secret(&[7u8; 32]);
        let n = group_prime();
        let g = BigUint::from(G);
        let width = n.to_bytes_be().len();

        // Server: verifier and public value B = k*v + g^b.
        let x = private_key(salt, &key);
        let v = g.modpow(&x, &n);
        let b = BigUint::from_bytes_be(&[9u8; 32]);
        let k = multiplier(&n, &g, width);
        let big_b = (&k * &v + g.modpow(&b, &n)) % &n;

        let proof = client
            .process_challenge(account, &key, salt, &big_b.to_bytes_be())
            .unwrap();

        // Server: S = (A * v^u)^b.
        let big_a = BigUint::from_bytes_be(&client.public_key());
        let u = BigUint::from_bytes_be(&hash(&[&pad(&big_a, width), &pad(&big_b, width)]));
        let s = ((&big_a * v.modpow(&u, &n)) % &n).modpow(&b, &n);
        let session_key = hash(&[&s.to_bytes_be()]);
        let expected_m1 = client_proof(&n, &g, width, account, salt, &big_a, &big_b, &session_key);
        let expected_m2 = hash(&[&big_a.to_bytes_be(), &expected_m1, &session_key]);

        assert_eq!(proof.m1, expected_m1);
        assert_eq!(proof.m2, expected_m2);
    }

    #[test]
    fn wrong_password_gives_different_proof() {
        let salt = b"salt";
        let client = SrpClient::with_secret(&[3u8; 32]);
        let b = BigUint::from(123_456_789u64).to_bytes_be();
        let right = derive_password_key("a", salt, 10, Protocol::S2k);
        let wrong = derive_password_key("b", salt, 10, Protocol::S2k);
        let p1 = client.process_challenge("x", &right, salt, &b).unwrap();
        let p2 = client.process_challenge("x", &wrong, salt, &b).unwrap();
        assert_ne!(p1.m1, p2.m1);
    }

    #[test]
    fn rejects_zero_server_value() {
        let client = SrpClient::with_secret(&[1u8; 32]);
        let n = group_prime().to_bytes_be();
        assert!(client.process_challenge("x", b"k", b"s", &n).is_err());
    }

    #[test]
    fn protocols_hash_differently() {
        let a = derive_password_key("pw", b"salt", 5, Protocol::S2k);
        let b = derive_password_key("pw", b"salt", 5, Protocol::S2kFo);
        assert_ne!(a, b);
        assert_eq!(Protocol::parse("s2k_fo"), Some(Protocol::S2kFo));
        assert_eq!(Protocol::parse("other"), None);
    }

    /// PBKDF2-HMAC-SHA256 known answer (RFC 7914 section 11), to pin the KDF.
    #[test]
    fn pbkdf2_known_answer() {
        let mut out = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(b"passwd", b"salt", 1, &mut out);
        assert_eq!(
            hex(&out),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }
}
