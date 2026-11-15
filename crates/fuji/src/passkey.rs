//! Asking a passkey to sign, through the system's WebAuthn: on Windows,
//! `webauthn.dll`, whose dialog offers the iPhone by QR code (and security
//! keys, and Windows Hello). Apple's side is in `fuji_icloud::passkey`.
//!
//! The large blob is read too: an Apple passkey keeps the Skybridge pairing
//! key there (see `docs/skybridge.md`).

use fuji_icloud::{PasskeyAssertion, PasskeyChallenge};

/// Whether this system can sign in with a passkey.
pub const SUPPORTED: bool = cfg!(windows);

/// Ask for a signature of `challenge`. Blocks while the system's dialog is
/// open (call it off the UI thread); `window` is the native handle of the
/// window the dialog belongs to.
pub fn get(window: isize, challenge: &PasskeyChallenge) -> Result<PasskeyAssertion, String> {
    imp::get(window, challenge)
}

#[cfg(windows)]
mod imp {
    use fuji_icloud::passkey::client_data_json;
    use fuji_icloud::{PasskeyAssertion, PasskeyChallenge};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Networking::WindowsWebServices::*;
    use windows::core::{HSTRING, PCWSTR};

    /// The options version this system understands: large blobs came with
    /// options v5 (API v3).
    fn options_version(api: u32) -> u32 {
        match api {
            5.. => WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_7,
            4 => WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_6,
            3 => WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_5,
            _ => WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_4,
        }
    }

    unsafe fn bytes(ptr: *const u8, len: u32) -> Vec<u8> {
        if ptr.is_null() || len == 0 {
            Vec::new()
        } else {
            // SAFETY: webauthn.dll gives a buffer of `len` bytes, valid until
            // the assertion is freed.
            unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
        }
    }

    pub fn get(window: isize, challenge: &PasskeyChallenge) -> Result<PasskeyAssertion, String> {
        let api = unsafe { WebAuthNGetApiVersionNumber() };
        let mut client_data = client_data_json(&challenge.challenge);
        let client = WEBAUTHN_CLIENT_DATA {
            dwVersion: WEBAUTHN_CLIENT_DATA_CURRENT_VERSION,
            cbClientDataJSON: client_data.len() as u32,
            pbClientDataJSON: client_data.as_mut_ptr(),
            pwszHashAlgId: WEBAUTHN_HASH_ALGORITHM_SHA_256,
        };

        // No allow list: any of the account's passkeys for the site may
        // answer, so Windows offers the phone. With Apple's list Windows
        // either offers only this PC and security keys (no transports) or
        // rejects the call (transports "internal"/"hybrid": 0x80090027,
        // checked on API v9). The sign-in widget sends none either, in its
        // autofill mode.
        let mut options = WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS {
            dwVersion: options_version(api),
            dwTimeoutMilliseconds: 120_000,
            dwAuthenticatorAttachment: WEBAUTHN_AUTHENTICATOR_ATTACHMENT_ANY,
            dwUserVerificationRequirement: WEBAUTHN_USER_VERIFICATION_REQUIREMENT_PREFERRED,
            ..Default::default()
        };
        if api >= 3 {
            options.dwCredLargeBlobOperation = WEBAUTHN_CRED_LARGE_BLOB_OPERATION_GET;
        }

        let rp_id = HSTRING::from(challenge.rp_id.as_str());
        let result = unsafe {
            WebAuthNAuthenticatorGetAssertion(
                HWND(window as *mut core::ffi::c_void),
                PCWSTR(rp_id.as_ptr()),
                &client,
                Some(&options),
            )
        };
        let assertion = match result {
            Ok(ptr) if !ptr.is_null() => ptr,
            Ok(_) => return Err("The passkey didn't answer.".into()),
            Err(err) => return Err(describe(err.code())),
        };

        // SAFETY: a non-null assertion from webauthn.dll, freed below.
        let a = unsafe { &*assertion };
        let large_blob = (a.dwVersion >= WEBAUTHN_ASSERTION_VERSION_2
            && a.dwCredLargeBlobStatus == WEBAUTHN_CRED_LARGE_BLOB_STATUS_SUCCESS)
            .then(|| unsafe { bytes(a.pbCredLargeBlob, a.cbCredLargeBlob) })
            .filter(|b| !b.is_empty());
        let out = unsafe {
            PasskeyAssertion {
                client_data_json: client_data.clone(),
                authenticator_data: bytes(a.pbAuthenticatorData, a.cbAuthenticatorData),
                signature: bytes(a.pbSignature, a.cbSignature),
                user_handle: bytes(a.pbUserId, a.cbUserId),
                credential_id: bytes(a.Credential.pbId, a.Credential.cbId),
                large_blob,
            }
        };
        if std::env::var_os("FUJI_DEBUG").is_some() {
            eprintln!(
                "passkey: api v{api}, assertion v{}, large blob status {}, {} bytes",
                a.dwVersion,
                a.dwCredLargeBlobStatus,
                out.large_blob.as_ref().map_or(0, Vec::len)
            );
        }
        unsafe { WebAuthNFreeAssertion(assertion) };
        Ok(out)
    }

    /// A sentence for a WebAuthn failure.
    fn describe(code: windows::core::HRESULT) -> String {
        let name = unsafe { WebAuthNGetErrorName(code) };
        let name = unsafe { name.to_string() }.unwrap_or_default();
        match name.as_str() {
            "NotAllowedError" => "The passkey sign-in was cancelled or timed out.".into(),
            "InvalidStateError" | "ConstraintError" => {
                "That passkey can't be used to sign in here.".into()
            }
            "NotSupportedError" => format!("This computer can't use passkeys for this ({code})."),
            _ => format!("The passkey sign-in failed ({name}, {code})."),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use fuji_icloud::{PasskeyAssertion, PasskeyChallenge};

    pub fn get(_: isize, _: &PasskeyChallenge) -> Result<PasskeyAssertion, String> {
        Err("Passkey sign-in isn't available on this system yet.".into())
    }
}
