//! The Apple ID sign-in screen: credentials, then a verification code; or
//! a passkey (the iPhone, by QR code), which also pairs fuji with that
//! device for Photos search.

use std::sync::Arc;
use std::time::Duration;

use std::sync::Mutex;

use fuji_icloud::{
    Account, AuthClient, AuthError, BridgeApproval, BridgeStart, SecurityKeyChallenge, SignInStep,
    TrustedPhone,
};
use gpui::{
    Animation, AnimationExt as _, App, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, SharedString, Subscription, Window, div, ease_out_quint, prelude::*, px,
};

use crate::assets::icon;
use crate::input::{FocusNext, FocusPrev, Submitted, TextInput};
use crate::security_key::{self, KeyError};
use crate::{passkey, platform, session_store, theme};

const CODE_LENGTH: usize = 6;
/// How long a typed code waits for a device approval that's on its way.
const APPROVAL_GRACE: Duration = Duration::from_secs(5);

/// Approval on a device ("bridge"), which runs in the background while the
/// code screen is up.
#[derive(Default)]
enum BridgeSlot {
    /// None running: codes go to plain verification.
    #[default]
    Idle,
    /// Waiting for a device to approve.
    Waiting,
    /// A device approved; its code finishes the key exchange.
    Ready(Box<BridgeApproval>),
    /// A device approved; its code goes to plain verification.
    Plain,
}

#[derive(Default)]
struct BridgeState {
    /// Bumped to cancel: a background task whose generation is stale
    /// leaves the slot alone and asks Apple for nothing more.
    generation: u64,
    slot: BridgeSlot,
}

impl BridgeState {
    fn cancel(&mut self) {
        self.generation += 1;
        self.slot = BridgeSlot::Idle;
    }
}

pub enum LoginEvent {
    SignedIn(Account),
}

#[derive(Clone)]
enum Stage {
    Credentials,
    Code {
        /// Set when the code was texted to this phone instead of a device.
        sms: Option<TrustedPhone>,
        phones: Option<Vec<TrustedPhone>>,
    },
    /// The account signs in with a security key; Apple sends no codes.
    SecurityKey {
        /// The challenge to sign; `None` to fetch a new one (after Apple
        /// turned down an answer).
        challenge: Option<SecurityKeyChallenge>,
        /// The key has a PIN: show the field.
        needs_pin: bool,
    },
}

impl Stage {
    fn key(&self) -> &'static str {
        match self {
            Stage::Credentials => "stage-credentials",
            Stage::Code { .. } => "stage-code",
            Stage::SecurityKey { .. } => "stage-security-key",
        }
    }
}

pub struct LoginView {
    auth: Arc<AuthClient>,
    stage: Stage,
    apple_id: Entity<TextInput>,
    password: Entity<TextInput>,
    code: Entity<TextInput>,
    /// The security key's PIN, when it has one.
    key_pin: Entity<TextInput>,
    busy: bool,
    /// The busy wait is the passkey dialog.
    passkey_busy: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    focus_handle: FocusHandle,
    /// Approval on a device ("bridge"); when it went through, the typed
    /// code finishes it and pairs fuji with that device.
    bridge: Arc<Mutex<BridgeState>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LoginEvent> for LoginView {}

impl Focusable for LoginView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl LoginView {
    /// `apple_id` pre-fills the field (from the last session); `notice` explains
    /// why the user is seeing this screen, if it isn't obvious.
    pub fn new(
        apple_id: Option<String>,
        auth: Arc<AuthClient>,
        notice: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let apple_id_input = cx.new(|cx| {
            let mut input = TextInput::new("Apple ID (email or phone)", cx);
            if let Some(id) = &apple_id {
                input.set_text(id.clone(), cx);
            }
            input
        });
        let password = cx.new(|cx| TextInput::new("Password", cx).masked());
        let code = cx.new(|cx| TextInput::new("6-digit code", cx));
        let key_pin = cx.new(|cx| TextInput::new("Security key PIN", cx).masked());

        let subscriptions = vec![
            cx.subscribe_in(
                &apple_id_input,
                window,
                |this, _, _: &Submitted, window, cx| {
                    this.password.focus_handle(cx).focus(window);
                },
            ),
            cx.subscribe_in(&password, window, |this, _, _: &Submitted, window, cx| {
                this.sign_in(window, cx);
            }),
            cx.subscribe_in(&code, window, |this, _, _: &Submitted, window, cx| {
                this.submit_code(window, cx);
            }),
            cx.subscribe_in(&key_pin, window, |this, _, _: &Submitted, window, cx| {
                this.use_security_key(window, cx);
            }),
            // Submit as soon as all six digits are in.
            cx.observe_in(&code, window, |this, code, window, cx| {
                let digits = code
                    .read(cx)
                    .text()
                    .chars()
                    .filter(char::is_ascii_digit)
                    .count();
                if digits == CODE_LENGTH && !this.busy {
                    this.submit_code(window, cx);
                }
            }),
        ];

        // Focus the first empty field.
        if apple_id.is_some() {
            password.focus_handle(cx).focus(window);
        } else {
            apple_id_input.focus_handle(cx).focus(window);
        }

        Self {
            auth,
            stage: Stage::Credentials,
            apple_id: apple_id_input,
            password,
            code,
            key_pin,
            busy: false,
            passkey_busy: false,
            error: None,
            notice,
            focus_handle: cx.focus_handle(),
            bridge: Arc::default(),
            _subscriptions: subscriptions,
        }
    }

    fn sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let apple_id = self.apple_id.read(cx).text().trim().to_string();
        let password = self.password.read(cx).text().to_string();
        if apple_id.is_empty() || password.is_empty() {
            self.error = Some("Enter your Apple ID and password.".into());
            cx.notify();
            return;
        }

        self.set_busy(true, cx);
        let auth = self.auth.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let step = auth.sign_in(&apple_id, &password);
                    // Persist on the background thread too: keychain calls can block.
                    if let Ok(SignInStep::Done(_)) = &step {
                        session_store::persist(&auth);
                    }
                    // Accounts with security keys get no codes: ask for the key.
                    let key = match &step {
                        Ok(SignInStep::NeedsCode) => match auth.security_key_challenge() {
                            Ok(key) => key,
                            Err(err) => {
                                if std::env::var_os("FUJI_DEBUG").is_some() {
                                    eprintln!("security key: couldn't read the page: {err}");
                                }
                                None
                            }
                        },
                        _ => None,
                    };
                    (step, key)
                })
                .await;
            let (result, key) = result;
            this.update_in(cx, |this, window, cx| {
                this.set_busy(false, cx);
                this.password.update(cx, |p, cx| p.clear(cx));
                match result {
                    Ok(SignInStep::Done(account)) => cx.emit(LoginEvent::SignedIn(account)),
                    Ok(SignInStep::NeedsCode) if key.is_some() => {
                        this.stage = Stage::SecurityKey {
                            challenge: key,
                            needs_pin: false,
                        };
                        this.notice = None;
                        this.use_security_key(window, cx);
                    }
                    Ok(SignInStep::NeedsCode) => {
                        this.stage = Stage::Code {
                            sms: None,
                            phones: None,
                        };
                        this.notice = Some(
                            "Allow the sign-in on your iPhone, iPad or Mac, then enter the code it shows."
                                .into(),
                        );
                        this.code.focus_handle(cx).focus(window);
                        this.start_bridge(window, cx);
                        cx.notify();
                    }
                    Err(err) => this.fail(err, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask the security key to sign Apple's challenge (the user touches it),
    /// then send the answer to Apple.
    fn use_security_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Stage::SecurityKey {
            challenge,
            needs_pin,
        } = &self.stage
        else {
            return;
        };
        if self.busy {
            return;
        }
        let pin = needs_pin.then(|| self.key_pin.read(cx).text().to_string());
        if pin.as_deref() == Some("") {
            self.error = Some("Enter your security key's PIN.".into());
            cx.notify();
            return;
        }
        let challenge = challenge.clone();
        let handle = platform::native_handle(window).unwrap_or(0);
        self.set_busy(true, cx);
        let auth = self.auth.clone();
        cx.spawn_in(window, async move |this, cx| {
            // On failure: the key's error or Apple's, and whether Apple saw the challenge.
            let result: Result<Account, (Option<KeyError>, Option<AuthError>, bool)> = cx
                .background_spawn(async move {
                    let challenge = match challenge {
                        Some(c) => c,
                        None => match auth.security_key_challenge() {
                            Ok(Some(c)) => c,
                            Ok(None) => {
                                return Err((
                                    Some(KeyError::Failed(
                                        "Apple stopped asking for a security key. Sign in again."
                                            .into(),
                                    )),
                                    None,
                                    false,
                                ));
                            }
                            Err(err) => return Err((None, Some(err), false)),
                        },
                    };
                    let assertion = security_key::sign(handle, &challenge, pin)
                        .map_err(|e| (Some(e), None, false))?;
                    // Apple has seen this challenge now: a retry needs a new one.
                    let account = auth
                        .submit_security_key(&challenge, &assertion)
                        .map_err(|e| (None, Some(e), true))?;
                    session_store::persist(&auth);
                    Ok(account)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.set_busy(false, cx);
                match result {
                    Ok(account) => cx.emit(LoginEvent::SignedIn(account)),
                    Err((key_err, auth_err, spent)) => {
                        if let Stage::SecurityKey {
                            challenge,
                            needs_pin,
                        } = &mut this.stage
                        {
                            if spent {
                                *challenge = None;
                            }
                            if matches!(key_err, Some(KeyError::NeedsPin | KeyError::WrongPin(_))) {
                                *needs_pin = true;
                            }
                        }
                        match (key_err, auth_err) {
                            (Some(KeyError::NeedsPin), _) => {
                                this.notice = Some(KeyError::NeedsPin.message().into());
                                this.key_pin.focus_handle(cx).focus(window);
                            }
                            (Some(err), _) => {
                                if matches!(err, KeyError::WrongPin(_)) {
                                    this.key_pin.update(cx, |p, cx| p.clear(cx));
                                    this.key_pin.focus_handle(cx).focus(window);
                                }
                                this.error = Some(err.message().into());
                            }
                            (None, Some(err)) => this.fail(err, cx),
                            (None, None) => {}
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask the devices to approve, in the background; the code screen is
    /// already up, so a code from anywhere else still works meanwhile. A
    /// device that approves also gets paired with fuji.
    fn start_bridge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = {
            let mut state = self.bridge.lock().unwrap();
            state.cancel();
            state.slot = BridgeSlot::Waiting;
            state.generation
        };
        let auth = self.auth.clone();
        let bridge = self.bridge.clone();
        let debug = std::env::var_os("FUJI_DEBUG").is_some();
        cx.spawn_in(window, async move |this, cx| {
            let notice = cx
                .background_spawn(async move {
                    let current = || bridge.lock().unwrap().generation == generation;
                    let started = auth.start_bridge();
                    if !current() {
                        return None;
                    }
                    let (slot, notice) = match started {
                        Ok(BridgeStart::Approval(approval)) => (
                            BridgeSlot::Ready(approval),
                            "Enter the code your device shows.",
                        ),
                        Ok(BridgeStart::PlainCode) => {
                            (BridgeSlot::Plain, "Enter the code your device shows.")
                        }
                        Ok(BridgeStart::NotOffered) | Err(_) => {
                            if let Err(err) = &started
                                && debug
                            {
                                eprintln!("bridge: no approval: {err}");
                            }
                            // Ask for a code the older way.
                            let _ = auth.request_device_code();
                            (
                                BridgeSlot::Idle,
                                "Enter the code shown on your device, or have one texted to you.",
                            )
                        }
                    };
                    let mut state = bridge.lock().unwrap();
                    if state.generation != generation {
                        return None;
                    }
                    state.slot = slot;
                    Some(notice)
                })
                .await;
            if let Some(notice) = notice {
                this.update(cx, |this, cx| {
                    if matches!(this.stage, Stage::Code { sms: None, .. }) {
                        this.notice = Some(notice.into());
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Sign in with a passkey through the system's dialog. Uses the Apple
    /// ID if one is typed in; without one, any of the user's passkeys can
    /// answer.
    fn sign_in_with_passkey(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(handle) = platform::native_handle(window) else {
            return;
        };
        let apple_id = self.apple_id.read(cx).text().trim().to_string();
        self.set_busy(true, cx);
        self.passkey_busy = true;
        let auth = self.auth.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<SignInStep, SharedString> = cx
                .background_spawn(async move {
                    let apple_id = (!apple_id.is_empty()).then_some(apple_id.as_str());
                    let challenge = auth
                        .passkey_challenge(apple_id)
                        .map_err(|e| SharedString::from(e.to_string()))?;
                    let assertion = passkey::get(handle, &challenge).map_err(SharedString::from)?;
                    let (step, pairing) = auth
                        .passkey_sign_in(&challenge, &assertion)
                        .map_err(|e| SharedString::from(e.to_string()))?;
                    if let SignInStep::NeedsCode = &step {
                        let _ = auth.request_device_code();
                    }
                    if let SignInStep::Done(_) = &step {
                        session_store::persist(&auth);
                        match &pairing {
                            Some(pairing) => {
                                session_store::save_pairing(pairing);
                                if std::env::var_os("FUJI_DEBUG").is_some() {
                                    eprintln!("passkey: signed in, pairing key saved");
                                }
                            }
                            None => {
                                if std::env::var_os("FUJI_DEBUG").is_some() {
                                    eprintln!("passkey: signed in, no pairing key in the blob");
                                }
                            }
                        }
                    }
                    Ok(step)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.passkey_busy = false;
                this.set_busy(false, cx);
                match result {
                    Ok(SignInStep::Done(account)) => cx.emit(LoginEvent::SignedIn(account)),
                    Ok(SignInStep::NeedsCode) => {
                        this.stage = Stage::Code {
                            sms: None,
                            phones: None,
                        };
                        this.notice = None;
                        this.code.focus_handle(cx).focus(window);
                        cx.notify();
                    }
                    Err(message) => {
                        this.error = Some(message);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn submit_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Stage::Code { sms, .. } = &self.stage else {
            return;
        };
        let phone_id = sms.as_ref().map(|p| p.id);
        if self.busy {
            return;
        }
        let code: String = self
            .code
            .read(cx)
            .text()
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        if code.len() != CODE_LENGTH {
            self.error = Some("Enter the 6-digit code.".into());
            cx.notify();
            return;
        }

        self.set_busy(true, cx);
        let auth = self.auth.clone();
        let bridge = self.bridge.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    // A device approval may be a moment from arriving (the
                    // widget waits too). If one came, take it. If none has
                    // yet, leave it running: this code may be from a text,
                    // and the device may still approve after a wrong one.
                    let deadline = std::time::Instant::now() + APPROVAL_GRACE;
                    let slot = loop {
                        let mut state = bridge.lock().unwrap();
                        if matches!(state.slot, BridgeSlot::Waiting) {
                            if phone_id.is_none() && std::time::Instant::now() < deadline {
                                drop(state);
                                std::thread::sleep(Duration::from_millis(200));
                                continue;
                            }
                            break BridgeSlot::Waiting;
                        }
                        let slot = std::mem::take(&mut state.slot);
                        state.cancel();
                        break slot;
                    };
                    let approval = match slot {
                        BridgeSlot::Ready(approval) => Some(*approval),
                        _ => None,
                    };
                    let account = match (phone_id, approval) {
                        (Some(id), _) => auth.submit_sms_code(id, &code),
                        (None, Some(approval)) => match auth.finish_bridge(approval, &code) {
                            Ok((account, pairing)) => {
                                session_store::save_pairing(&pairing);
                                if std::env::var_os("FUJI_DEBUG").is_some() {
                                    eprintln!("bridge: signed in, pairing key saved");
                                }
                                Ok(account)
                            }
                            Err(err) => {
                                // The exchange is spent: fall back to a plain
                                // code so the user can still sign in.
                                if std::env::var_os("FUJI_DEBUG").is_some() {
                                    eprintln!("bridge: didn't finish: {err}");
                                }
                                let _ = auth.request_device_code();
                                Err(AuthError::Bridge(format!(
                                    "{err} Enter the new code from your device."
                                )))
                            }
                        },
                        (None, None) => auth.submit_device_code(&code),
                    };
                    if account.is_ok() {
                        // Signed in: stop an approval still waiting.
                        bridge.lock().unwrap().cancel();
                        session_store::persist(&auth);
                    }
                    account
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.set_busy(false, cx);
                match result {
                    Ok(account) => cx.emit(LoginEvent::SignedIn(account)),
                    Err(err) => {
                        this.code.update(cx, |c, cx| c.clear(cx));
                        this.code.focus_handle(cx).focus(window);
                        this.fail(err, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Load the trusted phone numbers so the user can pick one.
    fn show_phones(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.set_busy(true, cx);
        let auth = self.auth.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { auth.trusted_phones() })
                .await;
            this.update(cx, |this, cx| {
                this.set_busy(false, cx);
                match result {
                    Ok(list) if list.is_empty() => {
                        this.error = Some("No trusted phone numbers on this Apple ID.".into());
                    }
                    Ok(list) => {
                        if let Stage::Code { phones, .. } = &mut this.stage {
                            *phones = Some(list);
                        }
                    }
                    Err(err) => this.fail(err, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn send_sms(&mut self, phone: TrustedPhone, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.bridge.lock().unwrap().cancel();
        self.set_busy(true, cx);
        let auth = self.auth.clone();
        let id = phone.id;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { auth.request_sms_code(id) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.set_busy(false, cx);
                match result {
                    Ok(()) => {
                        this.notice = Some(format!("Code sent to {}.", phone.number).into());
                        this.stage = Stage::Code {
                            sms: Some(phone),
                            phones: None,
                        };
                        this.code.update(cx, |c, cx| c.clear(cx));
                        this.code.focus_handle(cx).focus(window);
                    }
                    Err(err) => this.fail(err, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn back_to_credentials(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bridge.lock().unwrap().cancel();
        self.stage = Stage::Credentials;
        self.key_pin.update(cx, |p, cx| p.clear(cx));
        self.error = None;
        self.notice = None;
        self.code.update(cx, |c, cx| c.clear(cx));
        self.password.focus_handle(cx).focus(window);
        cx.notify();
    }

    fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        if busy {
            self.error = None;
        }
        for input in [&self.apple_id, &self.password, &self.code, &self.key_pin] {
            input.update(cx, |i, cx| i.set_disabled(busy, cx));
        }
        cx.notify();
    }

    fn fail(&mut self, err: AuthError, cx: &mut Context<Self>) {
        self.error = Some(err.to_string().into());
        cx.notify();
    }

    fn render_credentials(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(heading(
                "Sign in to iCloud",
                "Use your Apple Account to open your photo library.",
            ))
            .child(self.apple_id.clone())
            .child(self.password.clone())
            .child(primary_button(
                "sign-in",
                if self.busy {
                    "Signing in\u{2026}"
                } else {
                    "Sign in"
                },
                self.busy,
                cx.listener(|this, _, window, cx| this.sign_in(window, cx)),
            ))
            .when(passkey::SUPPORTED, |el| {
                el.child(secondary_button(
                    "sign-in-passkey",
                    if self.passkey_busy {
                        "Waiting for your passkey\u{2026}"
                    } else {
                        "Sign in with a passkey"
                    },
                    self.busy,
                    cx.listener(|this, _, window, cx| this.sign_in_with_passkey(window, cx)),
                ))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::text_meta())
                        .child(
                            "With your iPhone's passkey, Fuji can also ask the iPhone to search your photos.",
                        ),
                )
            })
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::text_meta())
                    .child("Fuji talks only to Apple. Your password is never saved."),
            )
    }

    fn render_code(
        &self,
        sms: &Option<TrustedPhone>,
        phones: &Option<Vec<TrustedPhone>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let subtitle = match sms {
            Some(_) => "Enter the code from the text message.",
            None => "Enter the code shown on your iPhone, iPad or Mac.",
        };
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(heading("Two-factor authentication", subtitle))
            .child(self.code.clone())
            .child(primary_button(
                "verify",
                if self.busy {
                    "Verifying\u{2026}"
                } else {
                    "Continue"
                },
                self.busy,
                cx.listener(|this, _, window, cx| this.submit_code(window, cx)),
            ));

        col = match phones {
            Some(list) => col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text_meta())
                            .child("Text a code to:"),
                    )
                    .children(list.iter().cloned().map(|phone| {
                        let label = phone.number.clone();
                        link_button(
                            ElementId::NamedInteger("phone".into(), phone.id),
                            label,
                            cx.listener(move |this, _, window, cx| {
                                this.send_sms(phone.clone(), window, cx)
                            }),
                        )
                    })),
            ),
            None => col.child(link_button(
                "get-sms",
                "Didn't get a code? Text me one",
                cx.listener(|this, _, window, cx| this.show_phones(window, cx)),
            )),
        };

        col.child(link_button(
            "back",
            "Use a different Apple Account",
            cx.listener(|this, _, window, cx| this.back_to_credentials(window, cx)),
        ))
    }
}

impl LoginView {
    fn render_security_key(
        &self,
        needs_pin: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let subtitle = if security_key::SUPPORTED {
            "Your Apple Account uses security keys. Plug yours in and touch it when it blinks."
        } else {
            "Your Apple Account uses security keys, which Fuji can't use on this system yet."
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(heading("Use your security key", subtitle))
            .when(needs_pin, |el| el.child(self.key_pin.clone()))
            .when(security_key::SUPPORTED, |el| {
                el.child(primary_button(
                    "use-key",
                    if self.busy {
                        "Waiting for your security key\u{2026}"
                    } else if needs_pin {
                        "Continue"
                    } else {
                        "Try again"
                    },
                    self.busy,
                    cx.listener(|this, _, window, cx| this.use_security_key(window, cx)),
                ))
            })
            .child(link_button(
                "back",
                "Use a different Apple Account",
                cx.listener(|this, _, window, cx| this.back_to_credentials(window, cx)),
            ))
    }
}

impl Render for LoginView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stage = self.stage.clone();
        let body = match &stage {
            Stage::Credentials => self.render_credentials(cx).into_any_element(),
            Stage::Code { sms, phones } => self.render_code(sms, phones, cx).into_any_element(),
            Stage::SecurityKey { needs_pin, .. } => {
                self.render_security_key(*needs_pin, cx).into_any_element()
            }
        };

        // Dialog recipe: popover surface, 16px rounding, one deep shadow.
        let card = div()
            .w(px(380.))
            .p(px(24.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .rounded(theme::RADIUS_DIALOG)
            .bg(theme::popover())
            .shadow(vec![gpui::BoxShadow {
                color: theme::scrim(0.72),
                offset: gpui::point(px(0.), px(30.)),
                blur_radius: px(80.),
                spread_radius: px(0.),
            }])
            .child(body)
            .when_some(self.notice.clone(), |el, notice| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(notice),
                )
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .text_color(theme::danger())
                        .child(icon("warning", px(14.), theme::danger()))
                        .child(error),
                )
            })
            // Each stage fades in while rising 10px.
            .with_animation(
                ElementId::Name(stage.key().into()),
                Animation::new(Duration::from_millis(220)).with_easing(ease_out_quint()),
                |el, t| el.opacity(t).mt(px((1. - t) * 10.)),
            );

        div()
            .track_focus(&self.focus_handle)
            .on_action(|_: &FocusNext, window, _| window.focus_next())
            .on_action(|_: &FocusPrev, window, _| window.focus_prev())
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::surface())
            .text_color(theme::text())
            .child(card)
    }
}

fn heading(title: &'static str, subtitle: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .pb(px(8.))
        .child(
            div()
                .text_size(px(26.))
                .line_height(px(30.))
                .font_weight(FontWeight::EXTRA_BOLD)
                .text_color(theme::text())
                .child(title),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child(subtitle),
        )
}

fn primary_button(
    id: &'static str,
    label: &'static str,
    busy: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let button = div()
        .id(id)
        .h(px(36.))
        .mt(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_CONTROL)
        .bg(theme::accent())
        .text_color(gpui::white())
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(13.));
    if busy {
        // A spinner while waiting on Apple.
        button
            .opacity(0.85)
            .child(crate::ui::loading_status(
                "busy-spinner",
                label,
                gpui::white(),
            ))
            .into_any_element()
    } else {
        let button = button.child(label);
        button
            .cursor_pointer()
            .hover(|el| el.bg(theme::accent_hover()))
            .on_click(on_click)
            .into_any_element()
    }
}

fn secondary_button(
    id: &'static str,
    label: &'static str,
    busy: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h(px(36.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_CONTROL)
        .bg(theme::fill())
        .hover(|el| el.bg(theme::fill_hover()))
        .text_color(theme::text())
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(13.))
        .child(label)
        .map(|el| {
            if busy {
                el.opacity(0.6)
            } else {
                el.cursor_pointer()
                    .hover(|el| el.bg(theme::hover()))
                    .on_click(on_click)
            }
        })
}

fn link_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::accent_text())
        .cursor_pointer()
        .hover(|el| el.text_color(theme::text()))
        .on_click(on_click)
        .child(label.into())
}
