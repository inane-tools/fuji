//! SPAKE2+ as Apple's sign-in widget runs it for approval on a device
//! ("bridge"): the browser is the prover, the trusted device the verifier,
//! and the password is the code the device shows.
//!
//! Transcribed from the widget's `proverWorker-*.js` (suite
//! "P256-SHA256-HKDF-HMAC-SCRYPT", context "SPAKE2Web"); see
//! `docs/skybridge.md`. Everything on the wire is base64 of raw bytes.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::elliptic_curve::Group;
use p256::elliptic_curve::ops::Reduce;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{AffinePoint, FieldBytes, ProjectivePoint, Scalar};
use sha2::{Digest, Sha256};

const CONTEXT: &[u8] = b"SPAKE2Web";
const CLIENT_IDENTITY: &[u8] = b"com.apple.security.webprover";
const SERVER_IDENTITY: &[u8] = b"com.apple.security.webverifier";

/// The suite's fixed points M and N (compressed SEC1).
const M_HEX: &str = "02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f";
const N_HEX: &str = "03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpakeError {
    #[error("the device's message isn't a valid curve point")]
    BadPoint,
    #[error("the device's confirmation didn't match: wrong code?")]
    BadConfirmation,
    #[error("couldn't decrypt the device's message")]
    Decrypt,
    #[error("bad salt")]
    BadSalt,
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("constant hex"))
        .collect()
}

fn point(bytes: &[u8]) -> Option<ProjectivePoint> {
    AffinePoint::from_sec1_bytes(bytes)
        .ok()
        .map(ProjectivePoint::from)
}

/// Uncompressed SEC1, as the widget encodes points.
fn encode(p: &ProjectivePoint) -> Vec<u8> {
    p.to_affine().to_sec1_point(false).as_bytes().to_vec()
}

fn scalar(bytes: &[u8; 32]) -> Scalar {
    Scalar::reduce(&FieldBytes::from(*bytes))
}

fn m_point() -> ProjectivePoint {
    point(&hex_bytes(M_HEX)).expect("M is on the curve")
}

fn n_point() -> ProjectivePoint {
    point(&hex_bytes(N_HEX)).expect("N is on the curve")
}

/// HKDF-SHA256 with an all-zero salt, as the widget's `kdf("", ikm, info)`.
fn kdf(ikm: &[u8], info: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    Hkdf::<Sha256>::new(Some(&[0u8; 32]), ikm)
        .expand(info.as_bytes(), &mut out)
        .expect("valid length");
    out
}

fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// Fields, each preceded by its length as 8 little-endian bytes.
fn transcript(fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        out.extend_from_slice(&(f.len() as u64).to_le_bytes());
        out.extend_from_slice(f);
    }
    out
}

/// A big-endian number without its leading zero bytes (`BN.toArrayLike`).
fn minimal(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[start..]
}

/// `w0` and `w1` from the code: scrypt (N 16384, r 8, p 1) into 64 bytes.
fn derive_w(code: &str, salt: &[u8]) -> ([u8; 32], [u8; 32]) {
    let params = scrypt::Params::new(14, 8, 1).expect("valid scrypt parameters");
    let mut out = [0u8; 64];
    scrypt::scrypt(code.as_bytes(), salt, &params, &mut out).expect("valid length");
    let mut w0 = [0u8; 32];
    let mut w1 = [0u8; 32];
    w0.copy_from_slice(&out[..32]);
    w1.copy_from_slice(&out[32..]);
    (w0, w1)
}

fn random_scalar() -> Scalar {
    // 40 random bytes reduced mod n, like the widget's range sampler; the
    // extra bytes make the bias negligible.
    let mut wide = [0u8; 40];
    rand::fill(&mut wide);
    let hi = scalar(&wide[..32].try_into().expect("32 bytes"));
    let lo_bytes = {
        let mut b = [0u8; 32];
        b[24..].copy_from_slice(&wide[32..]);
        b
    };
    // hi * 2^64 + lo
    let shift = Scalar::from(1u64 << 32) * Scalar::from(1u64 << 32);
    hi * shift + scalar(&lo_bytes)
}

/// The browser's side, from the code onwards.
pub struct Prover {
    x: Scalar,
    w0: Scalar,
    w0_raw: [u8; 32],
    w1: Scalar,
    /// Our share `T`, encoded.
    share: Vec<u8>,
}

/// Keys agreed after both confirmations.
pub struct Agreed {
    /// What icloud.com keeps as the pairing key (`bridge.key`).
    pub shared_key: Vec<u8>,
    verifier_key: Vec<u8>,
}

/// Halfway: our confirmation is ready, the device's still to check.
pub struct Confirming {
    confirm_v: Vec<u8>,
    share_p: Vec<u8>,
    shared_key: Vec<u8>,
    /// Our confirmation, for the device.
    pub message2: Vec<u8>,
}

impl Prover {
    /// Start from the salt (as sent, base64 decoded) and the code.
    pub fn new(salt: &[u8], code: &str) -> Self {
        Self::with_x(salt, code, random_scalar())
    }

    fn with_x(salt: &[u8], code: &str, x: Scalar) -> Self {
        let (w0_raw, w1_raw) = derive_w(code, salt);
        let (w0, w1) = (scalar(&w0_raw), scalar(&w1_raw));
        let t = ProjectivePoint::GENERATOR * x + m_point() * w0;
        Self {
            x,
            w0,
            w0_raw,
            w1,
            share: encode(&t),
        }
    }

    /// Message 1 for the device: our share.
    pub fn message1(&self) -> &[u8] {
        &self.share
    }

    /// Take the device's share and make our confirmation.
    pub fn finish(&self, device_share: &[u8]) -> Result<Confirming, SpakeError> {
        let s = point(device_share).ok_or(SpakeError::BadPoint)?;
        if bool::from(s.is_identity()) {
            return Err(SpakeError::BadPoint);
        }
        let base = s - n_point() * self.w0;
        let z = base * self.x;
        let v = base * self.w1;
        let tt = transcript(&[
            CONTEXT,
            CLIENT_IDENTITY,
            SERVER_IDENTITY,
            &encode(&m_point()),
            &encode(&n_point()),
            &self.share,
            &encode(&s),
            &encode(&z),
            &encode(&v),
            minimal(&self.w0_raw),
        ]);
        let hash = Sha256::digest(&tt);
        let confirm = kdf(&hash, "ConfirmationKeys", 64);
        let (confirm_p, confirm_v) = confirm.split_at(32);
        let s_bytes = encode(&s);
        Ok(Confirming {
            message2: mac(confirm_p, &s_bytes),
            confirm_v: confirm_v.to_vec(),
            share_p: self.share.clone(),
            shared_key: kdf(&hash, "SharedKey", 32),
        })
    }
}

impl Confirming {
    /// Check the device's confirmation; then the keys are agreed.
    pub fn verify(self, device_confirmation: &[u8]) -> Result<Agreed, SpakeError> {
        if mac(&self.confirm_v, &self.share_p) != device_confirmation {
            return Err(SpakeError::BadConfirmation);
        }
        Ok(Agreed::from_shared(self.shared_key))
    }
}

impl Agreed {
    fn from_shared(shared_key: Vec<u8>) -> Self {
        Self {
            verifier_key: kdf(&shared_key, "webVerifier", 32),
            shared_key,
        }
    }

    /// Decrypt a message from the device: version byte, 12-byte IV, 16-byte
    /// tag, ciphertext; AES-256-GCM with the version byte as associated data.
    pub fn decrypt(&self, message: &[u8]) -> Result<String, SpakeError> {
        if message.len() < 1 + 12 + 16 || message[0] != 0 {
            return Err(SpakeError::Decrypt);
        }
        let (version, rest) = message.split_at(1);
        let (iv, rest) = rest.split_at(12);
        let (tag, body) = rest.split_at(16);
        let mut sealed = body.to_vec();
        sealed.extend_from_slice(tag);
        let cipher =
            Aes256Gcm::new_from_slice(&self.verifier_key).map_err(|_| SpakeError::Decrypt)?;
        let plain = cipher
            .decrypt(
                &Nonce::try_from(iv).map_err(|_| SpakeError::Decrypt)?,
                Payload {
                    msg: &sealed,
                    aad: version,
                },
            )
            .map_err(|_| SpakeError::Decrypt)?;
        String::from_utf8(plain).map_err(|_| SpakeError::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device's side, as the worker's verifier class does it.
    struct Verifier {
        y: Scalar,
        w0: Scalar,
        w0_raw: [u8; 32],
        l: ProjectivePoint,
        share: Vec<u8>,
    }

    impl Verifier {
        fn new(salt: &[u8], code: &str, y: Scalar) -> Self {
            let (w0_raw, w1_raw) = derive_w(code, salt);
            let w0 = scalar(&w0_raw);
            let l = ProjectivePoint::GENERATOR * scalar(&w1_raw);
            let s = ProjectivePoint::GENERATOR * y + n_point() * w0;
            Self {
                y,
                w0,
                w0_raw,
                l,
                share: encode(&s),
            }
        }

        /// (shared key, confirmation for the prover, expected prover confirmation)
        fn finish(&self, prover_share: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let t = point(prover_share).unwrap();
            let z = (t - m_point() * self.w0) * self.y;
            let v = self.l * self.y;
            let tt = transcript(&[
                CONTEXT,
                CLIENT_IDENTITY,
                SERVER_IDENTITY,
                &encode(&m_point()),
                &encode(&n_point()),
                prover_share,
                &self.share,
                &encode(&z),
                &encode(&v),
                minimal(&self.w0_raw),
            ]);
            let hash = Sha256::digest(&tt);
            let confirm = kdf(&hash, "ConfirmationKeys", 64);
            let (confirm_p, confirm_v) = confirm.split_at(32);
            (
                kdf(&hash, "SharedKey", 32),
                mac(confirm_v, prover_share),
                mac(confirm_p, &self.share),
            )
        }
    }

    fn encrypt(agreed_shared: &[u8], text: &str) -> Vec<u8> {
        let key = kdf(agreed_shared, "webVerifier", 32);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let iv = [7u8; 12];
        let sealed = cipher
            .encrypt(
                &Nonce::try_from(&iv[..]).unwrap(),
                Payload {
                    msg: text.as_bytes(),
                    aad: &[0],
                },
            )
            .unwrap();
        let (body, tag) = sealed.split_at(sealed.len() - 16);
        [&[0u8][..], &iv, tag, body].concat()
    }

    #[test]
    fn prover_and_verifier_agree() {
        let salt = b"some salt bytes";
        let prover = Prover::new(salt, "123456");
        let device = Verifier::new(salt, "123456", Scalar::from(987_654_321u64));
        let (device_key, device_confirm, expect_prover_confirm) = device.finish(prover.message1());
        let confirming = prover.finish(&device.share).unwrap();
        assert_eq!(confirming.message2, expect_prover_confirm);
        let agreed = confirming.verify(&device_confirm).unwrap();
        assert_eq!(agreed.shared_key, device_key);
        assert_eq!(agreed.shared_key.len(), 32);
        // The device sends the real code encrypted under the agreed key.
        let sealed = encrypt(&device_key, "778899");
        assert_eq!(agreed.decrypt(&sealed).unwrap(), "778899");
    }

    #[test]
    fn wrong_code_fails_confirmation() {
        let salt = b"salt";
        let prover = Prover::new(salt, "111111");
        let device = Verifier::new(salt, "222222", Scalar::from(5u64));
        let (_, device_confirm, _) = device.finish(prover.message1());
        let confirming = prover.finish(&device.share).unwrap();
        assert_eq!(
            confirming.verify(&device_confirm).err(),
            Some(SpakeError::BadConfirmation)
        );
    }

    #[test]
    fn rejects_bad_points_and_ciphertext() {
        let prover = Prover::new(b"salt", "123456");
        assert!(prover.finish(&[4u8; 65]).is_err());
        let agreed = Agreed::from_shared(vec![1; 32]);
        assert!(agreed.decrypt(&[0u8; 10]).is_err());
    }

    #[test]
    fn points_encode_uncompressed() {
        assert_eq!(encode(&m_point()).len(), 65);
        assert_eq!(encode(&m_point())[0], 4);
        assert_eq!(minimal(&[0, 0, 5, 0]), &[5, 0]);
    }
}
