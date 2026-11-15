//! "Next generation messaging" (NGM): how IDS endpoints encrypt messages
//! to each other, as icloud.com does it (`pair-ec`). See `docs/skybridge.md`.
//!
//! To send: ECDH between a fresh ephemeral key and the recipient's pre-key,
//! HKDF-SHA256 (salt "LastPawn-MessageKeys") into an AES-256 key and a CTR
//! counter, the padded `InnerMessage { message, counter }` encrypted with
//! AES-CTR, and the whole signed with the sender's NGM key:
//! `OuterMessage { 1 encryptedPayload, 2 ephemeralPubKey, 3 signature,
//! 99 keyValidator }`.

use aes::Aes256;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hkdf::Hkdf;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{AffinePoint, ProjectivePoint};
use sha2::Sha256;

use crate::push::pb;

type Aes256Ctr = ctr::Ctr64BE<Aes256>;

const SALT: &[u8] = b"LastPawn-MessageKeys";
/// Messages are padded to at least this, then to a multiple of 16.
const FIRST_THRESHOLD: usize = 196;
const PAD_STEP: usize = 16;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NgmError {
    #[error("a key in the device's identity isn't valid")]
    BadKey,
    #[error("the device's pre-key isn't signed by its identity")]
    BadPrekey,
    #[error("the message isn't signed by the device")]
    BadSignature,
    #[error("the message couldn't be read")]
    Malformed,
}

/// `(p - 1) / 2` for P-256 (see `ids.rs`).
const HALF_P: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0x80, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

/// The point for a compact key (x alone): the one with the smaller y.
pub fn from_compact(x: &[u8]) -> Option<AffinePoint> {
    if x.len() != 32 {
        return None;
    }
    for tag in [2u8, 3] {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(x);
        let Ok(point) = AffinePoint::from_sec1_bytes(&bytes) else {
            continue;
        };
        if point.to_sec1_point(false).as_bytes()[33..65] <= HALF_P[..] {
            return Some(point);
        }
    }
    None
}

pub fn compact_of(point: &AffinePoint) -> Vec<u8> {
    point.to_sec1_point(false).as_bytes()[1..33].to_vec()
}

/// A device's public keys, from its IDS record.
#[derive(Clone, Debug)]
pub struct PeerKeys {
    pub signing: AffinePoint,
    pub prekey: AffinePoint,
    /// The raw `ngm-public-identity`, as key verification uses it.
    pub kt_loggable_data: Vec<u8>,
}

impl PeerKeys {
    /// From a device's `ngm-public-identity` and signed pre-key; checks the
    /// pre-key signature.
    pub fn from_ids(kt_loggable_data: &[u8], prekey_data: &[u8]) -> Result<Self, NgmError> {
        let outer = pb::fields(kt_loggable_data).ok_or(NgmError::BadKey)?;
        let identity = pb::fields(pb::get_bytes(&outer, 1).ok_or(NgmError::BadKey)?)
            .ok_or(NgmError::BadKey)?;
        let signing = from_compact(pb::get_bytes(&identity, 1).ok_or(NgmError::BadKey)?)
            .ok_or(NgmError::BadKey)?;

        let pre = pb::fields(prekey_data).ok_or(NgmError::BadKey)?;
        let prekey_x = pb::get_bytes(&pre, 1).ok_or(NgmError::BadKey)?;
        let prekey = from_compact(prekey_x).ok_or(NgmError::BadKey)?;
        let signature = pb::get_bytes(&pre, 2).ok_or(NgmError::BadPrekey)?;
        let timestamp = pb::get_fixed64(&pre, 3).ok_or(NgmError::BadPrekey)?;
        let mut signed = b"NGMPrekeySignature".to_vec();
        signed.extend_from_slice(prekey_x);
        signed.extend_from_slice(&timestamp.to_le_bytes());
        let sig = Signature::from_slice(signature).map_err(|_| NgmError::BadPrekey)?;
        VerifyingKey::from_affine(signing)
            .map_err(|_| NgmError::BadKey)?
            .verify(&signed, &sig)
            .map_err(|_| NgmError::BadPrekey)?;
        Ok(Self {
            signing,
            prekey,
            kt_loggable_data: kt_loggable_data.to_vec(),
        })
    }
}

/// Our side: the NGM signing key and pre-key.
pub struct OwnKeys<'a> {
    pub signing: &'a SigningKey,
    pub prekey: &'a SigningKey,
}

fn dh(secret: &SigningKey, public: &AffinePoint) -> Vec<u8> {
    let scalar: p256::Scalar = **secret.as_nonzero_scalar();
    let shared = (ProjectivePoint::from(*public) * scalar).to_affine();
    shared.to_sec1_point(false).as_bytes()[1..33].to_vec()
}

fn keys(dh_output: &[u8]) -> ([u8; 32], [u8; 16]) {
    let mut okm = [0u8; 48];
    Hkdf::<Sha256>::new(Some(SALT), dh_output)
        .expand(&[], &mut okm)
        .expect("valid length");
    let mut key = [0u8; 32];
    let mut iv = [0u8; 16];
    key.copy_from_slice(&okm[..32]);
    iv.copy_from_slice(&okm[32..]);
    (key, iv)
}

fn ctr(key: &[u8; 32], iv: &[u8; 16], data: &mut [u8]) {
    let mut cipher = Aes256Ctr::new(key.into(), iv.into());
    cipher.apply_keystream(data);
}

/// What gets signed: the DH output, the recipient's pre-key, the sender's
/// ephemeral key, the recipient's signing key (all compact), the ciphertext.
fn signed_data(
    dh_output: &[u8],
    recipient_prekey: &[u8],
    ephemeral: &[u8],
    recipient_signing: &[u8],
    ciphertext: &[u8],
) -> Vec<u8> {
    [
        dh_output,
        recipient_prekey,
        ephemeral,
        recipient_signing,
        ciphertext,
    ]
    .concat()
}

fn pad(mut data: Vec<u8>) -> Vec<u8> {
    let len = data.len();
    let target = if len <= FIRST_THRESHOLD {
        FIRST_THRESHOLD
    } else {
        len.div_ceil(PAD_STEP) * PAD_STEP
    };
    let pad = target - len;
    let mut random = vec![0u8; pad];
    rand::fill(&mut random[..]);
    data.extend_from_slice(&random);
    data.extend_from_slice(&(pad as u32).to_le_bytes());
    data
}

fn unpad(data: &[u8]) -> Option<&[u8]> {
    let n = data.len().checked_sub(4)?;
    let pad = u32::from_le_bytes(data[n..].try_into().ok()?) as usize;
    data.get(..n.checked_sub(pad)?)
}

/// Encrypt `message` (with its `counter`) for `peer`.
pub fn encrypt(own: &OwnKeys<'_>, peer: &PeerKeys, message: &[u8], counter: u32) -> Vec<u8> {
    let ephemeral = loop {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        if let Ok(key) = SigningKey::from_slice(&bytes) {
            // Compact keys only: the recipient rebuilds it from x.
            let point = key.verifying_key().as_affine().to_sec1_point(false);
            if point.as_bytes()[33..65] <= HALF_P[..] {
                break key;
            }
        }
    };
    let dh_output = dh(&ephemeral, &peer.prekey);
    let (key, iv) = keys(&dh_output);

    let mut inner = Vec::new();
    pb::bytes_field(&mut inner, 1, message);
    pb::varint_field(&mut inner, 2, u64::from(counter));
    let mut ciphertext = pad(inner);
    ctr(&key, &iv, &mut ciphertext);

    let ephemeral_x = compact_of(ephemeral.verifying_key().as_affine());
    let prekey_x = compact_of(&peer.prekey);
    let signing_x = compact_of(&peer.signing);
    let to_sign = signed_data(&dh_output, &prekey_x, &ephemeral_x, &signing_x, &ciphertext);
    let signature: Signature = own.signing.sign(&to_sign);

    let own_signing_x = compact_of(own.signing.verifying_key().as_affine());
    let own_prekey_check = [
        &own_signing_x[..2],
        &signing_x[..2],
        &prekey_x[..2],
        &[1u8][..],
    ]
    .concat();

    let mut outer = Vec::new();
    pb::bytes_field(&mut outer, 1, &ciphertext);
    pb::bytes_field(&mut outer, 2, &ephemeral_x);
    pb::bytes_field(&mut outer, 3, &signature.to_bytes());
    pb::bytes_field(&mut outer, 99, &own_prekey_check);
    outer
}

/// Decrypt an `OuterMessage` from `peer`: (counter, message).
pub fn decrypt(
    own: &OwnKeys<'_>,
    peer: &PeerKeys,
    outer: &[u8],
) -> Result<(u32, Vec<u8>), NgmError> {
    let fields = pb::fields(outer).ok_or(NgmError::Malformed)?;
    let ciphertext = pb::get_bytes(&fields, 1).ok_or(NgmError::Malformed)?;
    let ephemeral_x = pb::get_bytes(&fields, 2).ok_or(NgmError::Malformed)?;
    let signature = pb::get_bytes(&fields, 3).ok_or(NgmError::Malformed)?;
    let ephemeral = from_compact(ephemeral_x).ok_or(NgmError::BadKey)?;

    let dh_output = dh(own.prekey, &ephemeral);
    let own_prekey_x = compact_of(own.prekey.verifying_key().as_affine());
    let own_signing_x = compact_of(own.signing.verifying_key().as_affine());
    let to_verify = signed_data(
        &dh_output,
        &own_prekey_x,
        ephemeral_x,
        &own_signing_x,
        ciphertext,
    );
    let sig = Signature::from_slice(signature).map_err(|_| NgmError::BadSignature)?;
    VerifyingKey::from_affine(peer.signing)
        .map_err(|_| NgmError::BadKey)?
        .verify(&to_verify, &sig)
        .map_err(|_| NgmError::BadSignature)?;

    let (key, iv) = keys(&dh_output);
    let mut plain = ciphertext.to_vec();
    ctr(&key, &iv, &mut plain);
    let inner = pb::fields(unpad(&plain).ok_or(NgmError::Malformed)?).ok_or(NgmError::Malformed)?;
    let message = pb::get_bytes(&inner, 1)
        .ok_or(NgmError::Malformed)?
        .to_vec();
    let counter = pb::get_int(&inner, 2).unwrap_or(0) as u32;
    Ok((counter, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact_key() -> SigningKey {
        loop {
            let mut bytes = [0u8; 32];
            rand::fill(&mut bytes);
            if let Ok(key) = SigningKey::from_slice(&bytes) {
                let p = key.verifying_key().as_affine().to_sec1_point(false);
                if p.as_bytes()[33..65] <= HALF_P[..] {
                    return key;
                }
            }
        }
    }

    fn peer_of(signing: &SigningKey, prekey: &SigningKey) -> PeerKeys {
        PeerKeys {
            signing: *signing.verifying_key().as_affine(),
            prekey: *prekey.verifying_key().as_affine(),
            kt_loggable_data: Vec::new(),
        }
    }

    #[test]
    fn round_trip_between_two_endpoints() {
        let (a_sign, a_pre) = (compact_key(), compact_key());
        let (b_sign, b_pre) = (compact_key(), compact_key());
        let a = OwnKeys {
            signing: &a_sign,
            prekey: &a_pre,
        };
        let b = OwnKeys {
            signing: &b_sign,
            prekey: &b_pre,
        };
        let msg = br#"{"msgID":"x","body":{"kv":{"s":{"v":1}}}}"#;
        let outer = encrypt(&a, &peer_of(&b_sign, &b_pre), msg, 7);
        let (counter, plain) = decrypt(&b, &peer_of(&a_sign, &a_pre), &outer).unwrap();
        assert_eq!(counter, 7);
        assert_eq!(plain, msg);
        // Someone else can't pass it off as theirs.
        let c_sign = compact_key();
        assert_eq!(
            decrypt(&b, &peer_of(&c_sign, &a_pre), &outer).err(),
            Some(NgmError::BadSignature)
        );
    }

    #[test]
    fn padding() {
        assert_eq!(pad(vec![1; 10]).len(), 196 + 4);
        assert_eq!(pad(vec![1; 200]).len(), 208 + 4);
        let padded = pad(vec![5; 30]);
        assert_eq!(unpad(&padded), Some(&[5u8; 30][..]));
    }

    #[test]
    fn compact_keys_rebuild() {
        let key = compact_key();
        let point = *key.verifying_key().as_affine();
        assert_eq!(from_compact(&compact_of(&point)), Some(point));
    }
}
