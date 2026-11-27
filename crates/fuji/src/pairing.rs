//! Pairing with the iPhone for Photos search, from a signed-in session.
//!
//! Search goes to the iPhone with a key shared with it, which sign-in only
//! leaves behind when it went through device approval with a code (or the
//! iPhone's passkey). After a security-key sign-in there is none, so this
//! does what icloud.com does then ("on-demand 2FA"): get a token for asking
//! the second factor again, have a device approve, and run the key exchange
//! with the code it shows.

use std::sync::{Arc, Mutex};

use fuji_icloud::{Account, AuthClient, AuthError, BridgeApproval, BridgeStart};
use gpui::{
    Context, Entity, EventEmitter, Focusable, SharedString, Subscription, Window, div, prelude::*,
    px,
};

use crate::input::{Submitted, TextInput};
use crate::settings::{Tone, heading, row, section};
use crate::{session_store, theme, ui};

const CODE_LENGTH: usize = 6;

pub enum PairingEvent {
    /// A pairing key was saved: search with the iPhone can start.
    Paired,
    /// The pairing was forgotten: stop searching with the iPhone.
    Unpaired,
}

#[derive(Clone)]
enum Stage {
    /// No pairing key; offer to make one.
    Off,
    /// Waiting for a device to approve.
    Approving,
    /// Approved: waiting for the code the device shows.
    Code,
    /// Checking the code with the device.
    Verifying,
    /// A pairing key is saved.
    On,
}

pub struct PairingCard {
    auth: Arc<AuthClient>,
    account: Account,
    stage: Stage,
    error: Option<SharedString>,
    code: Entity<TextInput>,
    approval: Arc<Mutex<Option<BridgeApproval>>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PairingEvent> for PairingCard {}

impl PairingCard {
    pub fn new(
        auth: Arc<AuthClient>,
        account: Account,
        paired: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let code = cx.new(|cx| TextInput::new("6-digit code", cx));
        let subscriptions = vec![
            cx.subscribe_in(&code, window, |this, _, _: &Submitted, window, cx| {
                this.submit_code(window, cx)
            }),
            cx.observe_in(&code, window, |this, code, window, cx| {
                let digits = code
                    .read(cx)
                    .text()
                    .chars()
                    .filter(char::is_ascii_digit)
                    .count();
                if digits == CODE_LENGTH && matches!(this.stage, Stage::Code) {
                    this.submit_code(window, cx);
                }
            }),
        ];
        Self {
            auth,
            account,
            stage: if paired { Stage::On } else { Stage::Off },
            error: None,
            code,
            approval: Arc::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Ask the devices to approve; on approval, ask for the code.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.stage, Stage::Off) {
            return;
        }
        self.stage = Stage::Approving;
        self.error = None;
        cx.notify();
        let auth = self.auth.clone();
        let account = self.account.clone();
        let slot = self.approval.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<(), String> = cx
                .background_spawn(async move {
                    auth.begin_on_demand(&account).map_err(|e| {
                        format!("Apple wouldn't start pairing: {e}")
                    })?;
                    match auth.start_bridge() {
                        Ok(BridgeStart::Approval(approval)) => {
                            *slot.lock().unwrap() = Some(*approval);
                            Ok(())
                        }
                        other => {
                            // Not this way: say what Apple offered instead,
                            // unless Apple refused outright (asking again
                            // would only be refused again).
                            let refused = matches!(other, Err(AuthError::Unexpected { .. }));
                            let key = !refused
                                && matches!(auth.security_key_challenge(), Ok(Some(_)));
                            auth.end_on_demand();
                            Err(match other {
                                Ok(BridgeStart::PlainCode) => {
                                    "Your iPhone approved, but Apple skipped the key exchange search needs. Try again.".to_string()
                                }
                                _ if key => {
                                    "Apple only offers your security key for this, which can't pair Fuji with your iPhone.".to_string()
                                }
                                Ok(_) => "Apple didn't offer approval on your iPhone.".to_string(),
                                Err(e) => e.to_string(),
                            })
                        }
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.stage = Stage::Code;
                        this.code.update(cx, |c, cx| c.clear(cx));
                        this.code.focus_handle(cx).focus(window);
                    }
                    Err(message) => {
                        this.stage = Stage::Off;
                        this.error = Some(message.into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn submit_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.stage, Stage::Code) {
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
        let Some(approval) = self.approval.lock().unwrap().take() else {
            self.stage = Stage::Off;
            cx.notify();
            return;
        };
        self.stage = Stage::Verifying;
        self.error = None;
        self.code.update(cx, |c, cx| c.set_disabled(true, cx));
        cx.notify();
        let auth = self.auth.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let result = auth.finish_pairing(approval, &code);
                    auth.end_on_demand();
                    if let Ok(pairing) = &result {
                        session_store::save_pairing(pairing);
                    }
                    result.map(|_| ())
                })
                .await;
            this.update_in(cx, |this, _, cx| {
                this.code.update(cx, |c, cx| {
                    c.set_disabled(false, cx);
                    c.clear(cx);
                });
                match result {
                    Ok(()) => {
                        this.stage = Stage::On;
                        cx.emit(PairingEvent::Paired);
                    }
                    Err(err) => {
                        // The exchange is spent: start over.
                        this.stage = Stage::Off;
                        this.error = Some(match err {
                            AuthError::InvalidCode => "That code didn't match. Try again.".into(),
                            err => err.to_string().into(),
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether a pairing key is saved (learned after the card is made).
    pub fn set_paired(&mut self, paired: bool, cx: &mut Context<Self>) {
        match (&self.stage, paired) {
            (Stage::Off, true) => self.stage = Stage::On,
            (Stage::On, false) => self.stage = Stage::Off,
            _ => return,
        }
        cx.notify();
    }

    /// Forget the pairing key, so search stops going to the iPhone (and a
    /// new pairing starts clean).
    fn unpair(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.stage, Stage::On) {
            return;
        }
        self.stage = Stage::Off;
        self.error = None;
        cx.background_spawn(async { session_store::clear_pairing() })
            .detach();
        cx.emit(PairingEvent::Unpaired);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.approval.lock().unwrap().take();
        self.auth.end_on_demand();
        self.stage = Stage::Off;
        self.error = None;
        cx.notify();
    }
}

impl Render for PairingCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (description, control): (&str, gpui::AnyElement) = match self.stage {
            Stage::Off => (
                "Search your photos for things like \u{201c}cat\u{201d} or \u{201c}beach\u{201d}. Your iPhone does the searching, as on iCloud.com.",
                ui::text_button(
                    "pair-start",
                    None,
                    "Turn On",
                    cx.listener(|this, _, window, cx| this.start(window, cx)),
                )
                .into_any_element(),
            ),
            Stage::Approving => (
                "Allow it on your iPhone, iPad or Mac.",
                ui::text_button(
                    "pair-cancel",
                    None,
                    "Cancel",
                    cx.listener(|this, _, _, cx| this.cancel(cx)),
                )
                .into_any_element(),
            ),
            Stage::Code => (
                "Enter the code your device shows.",
                div()
                    .w(px(150.))
                    .child(self.code.clone())
                    .into_any_element(),
            ),
            Stage::Verifying => (
                "Checking the code with your iPhone. This can take up to a minute.",
                ui::loading_status("pair-verifying", "Checking\u{2026}", theme::text_muted())
                    .into_any_element(),
            ),
            Stage::On => (
                "Paired. Type in the search box and your iPhone finds the photos.",
                ui::text_button(
                    "pair-off",
                    None,
                    "Turn Off",
                    cx.listener(|this, _, _, cx| this.unpair(cx)),
                )
                .into_any_element(),
            ),
        };
        section(Tone::Default)
            .child(heading("Search with your iPhone", None))
            .child(row("Photo search", description, false, control))
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .pt(px(8.))
                        .text_size(px(12.))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
    }
}
