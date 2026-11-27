//! Asking a security key (FIDO) to sign Apple's two-factor challenge.
//! On Windows, through the system's WebAuthn (`webauthn.dll`), which asks
//! for the touch and PIN itself. Linux has no system WebAuthn, so there
//! this talks to USB keys itself through Mozilla's `authenticator` crate
//! (the code Firefox uses).

use fuji_icloud::{PasskeyAssertion, SecurityKeyChallenge};

/// Whether this build can use security keys.
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", windows));

/// Why signing didn't happen.
#[derive(Debug, Clone)]
pub enum KeyError {
    /// The key has a PIN: ask for it, then sign again with it.
    NeedsPin,
    /// The PIN was wrong; tries left, if the key says.
    WrongPin(Option<u8>),
    /// Anything else, ready to show.
    Failed(String),
}

impl KeyError {
    pub fn message(&self) -> String {
        match self {
            KeyError::NeedsPin => "Enter your security key's PIN.".into(),
            KeyError::WrongPin(Some(left)) => {
                format!("That PIN isn't right. {left} tries left before the key locks.")
            }
            KeyError::WrongPin(None) => "That PIN isn't right.".into(),
            KeyError::Failed(message) => message.clone(),
        }
    }
}

/// Sign with the first key the user touches. Blocks until then, or until
/// it times out: call from a background thread. `window` is the native
/// handle the system dialog belongs to (Windows); `pin` is for Linux, where
/// Fuji asks for it.
pub fn sign(
    window: isize,
    challenge: &SecurityKeyChallenge,
    pin: Option<String>,
) -> Result<PasskeyAssertion, KeyError> {
    imp::sign(window, challenge, pin)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};

    use authenticator::authenticatorservice::{AuthenticatorService, SignArgs};
    use authenticator::ctap2::server::{
        AuthenticationExtensionsClientInputs, PublicKeyCredentialDescriptor, Transport,
        UserVerificationRequirement,
    };
    use authenticator::errors::AuthenticatorError;
    use authenticator::statecallback::StateCallback;
    use authenticator::{Pin, StatusPinUv, StatusUpdate};
    use fuji_icloud::passkey::ORIGIN;

    use super::*;

    /// How long to wait for a key to be plugged in and touched.
    const TIMEOUT_MS: u64 = 60_000;

    pub fn sign(
        _window: isize,
        challenge: &SecurityKeyChallenge,
        pin: Option<String>,
    ) -> Result<PasskeyAssertion, KeyError> {
        let debug = std::env::var_os("FUJI_DEBUG").is_some();
        let mut service = AuthenticatorService::new()
            .map_err(|e| KeyError::Failed(format!("Couldn't use security keys: {e}")))?;
        service.add_u2f_usb_hid_platform_transports();

        let client_data_json = challenge.client_data_json();
        let args = SignArgs {
            client_data_hash: challenge.client_data_hash(),
            origin: ORIGIN.to_string(),
            relying_party_id: challenge.rp_id.clone(),
            allow_list: challenge
                .key_handles
                .iter()
                .map(|id| PublicKeyCredentialDescriptor {
                    id: id.clone(),
                    transports: vec![Transport::USB],
                })
                .collect(),
            // As icloud.com asks.
            user_verification_req: UserVerificationRequirement::Preferred,
            user_presence_req: true,
            extensions: AuthenticationExtensionsClientInputs::default(),
            pin: None,
            use_ctap1_fallback: false,
        };

        // Status: hand over the PIN once if the key asks; otherwise stop
        // and say why, so the screen can ask for it.
        let (status_tx, status_rx) = channel::<StatusUpdate>();
        let pin_problem: Arc<Mutex<Option<KeyError>>> = Arc::default();
        let problem = pin_problem.clone();
        std::thread::spawn(move || {
            let mut pin = pin;
            for update in status_rx {
                match update {
                    StatusUpdate::PinUvError(StatusPinUv::PinRequired(reply)) => {
                        match pin.take() {
                            Some(p) => {
                                let _ = reply.send(Pin::new(&p));
                            }
                            // Dropping `reply` cancels the request.
                            None => *problem.lock().unwrap() = Some(KeyError::NeedsPin),
                        }
                    }
                    StatusUpdate::PinUvError(StatusPinUv::InvalidPin(_, left)) => {
                        *problem.lock().unwrap() = Some(KeyError::WrongPin(left));
                    }
                    StatusUpdate::PinUvError(
                        StatusPinUv::PinBlocked | StatusPinUv::PinAuthBlocked,
                    ) => {
                        *problem.lock().unwrap() = Some(KeyError::Failed(
                            "This security key's PIN is locked. Unplug it and try again, or reset it."
                                .into(),
                        ));
                    }
                    StatusUpdate::PinUvError(other) => {
                        *problem.lock().unwrap() = Some(KeyError::Failed(format!(
                            "Security key PIN problem: {other:?}"
                        )));
                    }
                    // A key with several accounts for this site: any will do.
                    StatusUpdate::SelectResultNotice(reply, _) => {
                        let _ = reply.send(Some(0));
                    }
                    other => {
                        if debug {
                            eprintln!("security key: {other:?}");
                        }
                    }
                }
            }
        });

        let (result_tx, result_rx) = channel();
        let callback = StateCallback::new(Box::new(move |rv| {
            let _ = result_tx.send(rv);
        }));
        service
            .sign(TIMEOUT_MS, args, status_tx, callback)
            .map_err(|e| KeyError::Failed(format!("Couldn't use security keys: {e}")))?;
        let result = result_rx
            .recv()
            .map_err(|_| KeyError::Failed("The security key stopped answering.".into()))?;

        match result {
            Ok(signed) => {
                let assertion = signed.assertion;
                let credential_id = assertion
                    .credentials
                    .map(|c| c.id)
                    .or_else(|| match challenge.key_handles.as_slice() {
                        [only] => Some(only.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        KeyError::Failed("The security key didn't say which key it used.".into())
                    })?;
                Ok(PasskeyAssertion {
                    client_data_json,
                    authenticator_data: assertion.auth_data.to_vec(),
                    signature: assertion.signature,
                    user_handle: assertion.user.map(|u| u.id).unwrap_or_default(),
                    credential_id,
                    large_blob: None,
                })
            }
            Err(err) => {
                if debug {
                    eprintln!("security key: sign failed: {err:?}");
                }
                if let Some(problem) = pin_problem.lock().unwrap().take() {
                    return Err(problem);
                }
                Err(KeyError::Failed(match err {
                    AuthenticatorError::CancelledByUser => "Cancelled.".into(),
                    AuthenticatorError::CredentialExcluded => {
                        "This security key isn't one of your Apple Account's keys.".into()
                    }
                    _ => "No security key answered. Plug in your key, then try again and touch it when it blinks."
                        .into(),
                }))
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Networking::WindowsWebServices::*;
    use windows::core::{HSTRING, PCWSTR};

    use super::*;

    unsafe fn bytes(ptr: *const u8, len: u32) -> Vec<u8> {
        if ptr.is_null() || len == 0 {
            Vec::new()
        } else {
            // SAFETY: webauthn.dll gives a buffer of `len` bytes, valid until
            // the assertion is freed.
            unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
        }
    }

    pub fn sign(
        window: isize,
        challenge: &SecurityKeyChallenge,
        _pin: Option<String>,
    ) -> Result<PasskeyAssertion, KeyError> {
        let mut client_data = challenge.client_data_json();
        let client = WEBAUTHN_CLIENT_DATA {
            dwVersion: WEBAUTHN_CLIENT_DATA_CURRENT_VERSION,
            cbClientDataJSON: client_data.len() as u32,
            pbClientDataJSON: client_data.as_mut_ptr(),
            pwszHashAlgId: WEBAUTHN_HASH_ALGORITHM_SHA_256,
        };

        // Only the account's keys, as icloud.com asks; Windows then offers
        // security keys rather than this PC or a phone.
        let mut ids = challenge.key_handles.clone();
        let mut list: Vec<WEBAUTHN_CREDENTIAL> = ids
            .iter_mut()
            .map(|id| WEBAUTHN_CREDENTIAL {
                dwVersion: WEBAUTHN_CREDENTIAL_CURRENT_VERSION,
                cbId: id.len() as u32,
                pbId: id.as_mut_ptr(),
                pwszCredentialType: WEBAUTHN_CREDENTIAL_TYPE_PUBLIC_KEY,
            })
            .collect();
        let options = WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS {
            dwVersion: WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_4,
            dwTimeoutMilliseconds: 120_000,
            CredentialList: WEBAUTHN_CREDENTIALS {
                cCredentials: list.len() as u32,
                pCredentials: list.as_mut_ptr(),
            },
            dwAuthenticatorAttachment: WEBAUTHN_AUTHENTICATOR_ATTACHMENT_CROSS_PLATFORM,
            dwUserVerificationRequirement: WEBAUTHN_USER_VERIFICATION_REQUIREMENT_PREFERRED,
            ..Default::default()
        };

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
            Ok(_) => return Err(KeyError::Failed("The security key didn't answer.".into())),
            Err(err) => return Err(KeyError::Failed(describe(err.code()))),
        };
        // SAFETY: a non-null assertion from webauthn.dll, freed below.
        let a = unsafe { &*assertion };
        let out = unsafe {
            PasskeyAssertion {
                client_data_json: client_data.clone(),
                authenticator_data: bytes(a.pbAuthenticatorData, a.cbAuthenticatorData),
                signature: bytes(a.pbSignature, a.cbSignature),
                user_handle: bytes(a.pbUserId, a.cbUserId),
                credential_id: bytes(a.Credential.pbId, a.Credential.cbId),
                large_blob: None,
            }
        };
        unsafe { WebAuthNFreeAssertion(assertion) };
        drop(list);
        Ok(out)
    }

    /// A sentence for a WebAuthn failure.
    fn describe(code: windows::core::HRESULT) -> String {
        let name = unsafe { WebAuthNGetErrorName(code) };
        let name = unsafe { name.to_string() }.unwrap_or_default();
        match name.as_str() {
            "NotAllowedError" => "The security key sign-in was cancelled or timed out.".into(),
            "InvalidStateError" | "ConstraintError" => {
                "This security key isn't one of your Apple Account's keys.".into()
            }
            _ => format!("The security key sign-in failed ({name}, {code})."),
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;

    pub fn sign(
        _: isize,
        _: &SecurityKeyChallenge,
        _: Option<String>,
    ) -> Result<PasskeyAssertion, KeyError> {
        Err(KeyError::Failed(
            "Fuji can't use security keys on this system yet.".into(),
        ))
    }
}
