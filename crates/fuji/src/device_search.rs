//! Photos search provided by the user's iPhone (Skybridge), on a thread of
//! its own: it connects when first asked (push, IDS, finding the paired
//! device, key verification), then answers searches over that connection.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fuji_icloud::AuthClient;
use fuji_icloud::skybridge::Skybridge;
use futures::channel::oneshot;
use serde_json::{Value, json};

/// How many photos and collections to ask for.
const ASSETS_LIMIT: u64 = 1000;
const COLLECTIONS_LIMIT: u64 = 20;
/// How long to wait for the device's answer.
const ANSWER_TIME: Duration = Duration::from_secs(20);

/// What the device found.
#[derive(Clone, Debug)]
pub struct DeviceResults {
    /// Asset record names, best first (recommended ones lead).
    pub ids: Vec<String>,
    /// The device's name ("Provided by ...").
    pub provider: String,
    /// As many came back as asked for: there may be more.
    pub capped: bool,
}

enum Job {
    /// Connect now, so the first search needn't wait for it.
    Connect,
    Search {
        text: String,
        reply: oneshot::Sender<Result<DeviceResults, String>>,
    },
}

pub struct DeviceSearch {
    jobs: Sender<Job>,
    /// The paired device's name, once it has said it.
    name: Arc<Mutex<Option<String>>>,
}

struct Connected {
    sky: Skybridge,
    device: usize,
    name: String,
    /// The session ID token and when it was fetched (reused for 30 s, as
    /// icloud.com does).
    token: (String, std::time::Instant),
}

/// How long a session ID token is reused.
const TOKEN_REUSE: Duration = Duration::from_secs(30);

impl Connected {
    fn token(&mut self, auth: &AuthClient) -> Result<String, String> {
        if self.token.1.elapsed() >= TOKEN_REUSE {
            let fresh = auth.session_id_token().map_err(|e| e.to_string())?;
            self.token = (fresh, std::time::Instant::now());
        }
        Ok(self.token.0.clone())
    }
}

fn debug(message: &str) {
    if std::env::var_os("FUJI_DEBUG").is_some() {
        eprintln!("device search: {message}");
    }
}

impl DeviceSearch {
    /// Start, if fuji is paired with a device (`pairing_key`, base64).
    pub fn start(
        auth: Arc<AuthClient>,
        dsid: String,
        apple_id: String,
        pairing_key: String,
    ) -> Option<Self> {
        let key = BASE64.decode(pairing_key).ok()?;
        let (jobs, rx) = channel();
        let name = Arc::new(Mutex::new(None));
        let worker_name = name.clone();
        std::thread::Builder::new()
            .name("fuji-device-search".into())
            .spawn(move || worker(rx, &auth, &dsid, &apple_id, &key, &worker_name))
            .ok()?;
        let search = Self { jobs, name };
        let _ = search.jobs.send(Job::Connect);
        Some(search)
    }

    /// The paired device's name, if it has answered already.
    pub fn device_name(&self) -> Option<String> {
        self.name.lock().ok()?.clone()
    }

    /// Ask the device to search for `text`.
    pub async fn search(&self, text: String) -> Result<DeviceResults, String> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Job::Search { text, reply })
            .map_err(|_| "device search stopped".to_string())?;
        answer
            .await
            .unwrap_or_else(|_| Err("device search stopped".into()))
    }
}

fn connect(
    auth: &Arc<AuthClient>,
    dsid: &str,
    apple_id: &str,
    key: &[u8],
) -> Result<Connected, String> {
    // The session token comes from another service: fetch it meanwhile.
    let token_auth = auth.clone();
    let token = std::thread::spawn(move || token_auth.session_id_token());
    let handle = if apple_id.contains('@') {
        format!("mailto:{apple_id}")
    } else {
        format!("tel:{apple_id}")
    };
    // Go on as the connection of an earlier run, so Apple doesn't get a
    // new registration every launch.
    let saved = crate::session_store::load_bridge();
    let mut sky =
        Skybridge::connect(auth, saved.as_ref(), dsid, &handle).map_err(|e| e.to_string())?;
    debug(&format!(
        "{} devices ({})",
        sky.device_count(),
        if sky.reused() {
            "kept registration"
        } else {
            "registered"
        }
    ));
    let token = token
        .join()
        .map_err(|_| "session token fetch failed".to_string())?
        .map_err(|e| e.to_string())?;
    let (device, name) = match sky.verify_key(key, &token) {
        Ok(found) => found,
        // The kept connection didn't work out: start fresh, once.
        Err(err) if saved.is_some() => {
            debug(&format!("kept connection failed ({err}); starting fresh"));
            sky.close();
            crate::session_store::clear_bridge();
            sky = Skybridge::connect(auth, None, dsid, &handle).map_err(|e| e.to_string())?;
            sky.verify_key(key, &token).map_err(|e| e.to_string())?
        }
        Err(err) => return Err(err.to_string()),
    };
    debug(&format!("paired device answered: {name}"));
    crate::session_store::save_bridge(&sky.saved());
    Ok(Connected {
        sky,
        device,
        name,
        token: (token, std::time::Instant::now()),
    })
}

fn ids_of(found: &Value, list: &str, into: &mut Vec<String>) {
    if let Some(items) = found[list].as_array() {
        for id in items.iter().filter_map(Value::as_str) {
            if !into.iter().any(|known| known == id) {
                into.push(id.to_string());
            }
        }
    }
}

fn search(conn: &mut Connected, auth: &AuthClient, text: &str) -> Result<DeviceResults, String> {
    let token = conn.token(auth)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let body = json!({ "photos": { "search": {
        "string": text,
        "assetsLimit": ASSETS_LIMIT,
        "collectionsLimit": COLLECTIONS_LIMIT,
        "clientTimestamp": now,
    } } });
    let id = conn
        .sky
        .send(conn.device, body, Some(&token))
        .map_err(|e| e.to_string())?;

    // The answer comes in numbered parts, in any order.
    let mut parts: Vec<(u64, Value)> = Vec::new();
    let deadline = std::time::Instant::now() + ANSWER_TIME;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let Some(reply) = conn.sky.receive(left).map_err(|e| e.to_string())? else {
            if parts.is_empty() {
                return Err(format!("{} didn't answer", conn.name));
            }
            break;
        };
        if reply.msg_id != id {
            continue;
        }
        if let Some(error) = reply.body.get("error") {
            return Err(format!("{} couldn't search: {error}", conn.name));
        }
        // An unnumbered answer is the whole answer.
        let (i, total) = reply.part.unwrap_or((0, 1));
        parts.push((i, reply.body));
        if parts.len() as u64 >= total {
            break;
        }
    }
    parts.sort_by_key(|(i, _)| *i);
    let mut ids = Vec::new();
    for (_, body) in &parts {
        ids_of(
            &body["photos"]["search"]["resultsFound"],
            "recommendedResults",
            &mut ids,
        );
    }
    for (_, body) in &parts {
        ids_of(
            &body["photos"]["search"]["resultsFound"],
            "results",
            &mut ids,
        );
    }
    let capped = ids.len() as u64 >= ASSETS_LIMIT;
    Ok(DeviceResults {
        ids,
        provider: conn.name.clone(),
        capped,
    })
}

fn worker(
    jobs: Receiver<Job>,
    auth: &Arc<AuthClient>,
    dsid: &str,
    apple_id: &str,
    key: &[u8],
    name: &Mutex<Option<String>>,
) {
    let mut conn: Option<Connected> = None;
    let ensure = |conn: &mut Option<Connected>| -> Result<(), String> {
        if conn.is_none() {
            let started = std::time::Instant::now();
            let c = connect(auth, dsid, apple_id, key)?;
            debug(&format!("connected in {:?}", started.elapsed()));
            if let Ok(mut n) = name.lock() {
                *n = Some(c.name.clone());
            }
            *conn = Some(c);
        }
        Ok(())
    };
    while let Ok(mut job) = jobs.recv() {
        // Only the latest search matters.
        while let Ok(newer) = jobs.try_recv() {
            if let Job::Search { reply, .. } = job {
                let _ = reply.send(Err("superseded".into()));
            }
            job = newer;
        }
        let (text, reply) = match job {
            Job::Connect => {
                if let Err(err) = ensure(&mut conn) {
                    debug(&format!("couldn't connect: {err}"));
                }
                continue;
            }
            Job::Search { text, reply } => (text, reply),
        };
        if let Err(err) = ensure(&mut conn) {
            debug(&format!("couldn't connect: {err}"));
            let _ = reply.send(Err(err));
            continue;
        }
        let c = conn.as_mut().expect("connected above");
        let started = std::time::Instant::now();
        let result = search(c, auth, &text);
        match &result {
            Ok(found) => {
                debug(&format!(
                    "{} results in {:?}",
                    found.ids.len(),
                    started.elapsed()
                ));
                // Its message counter moved on.
                crate::session_store::save_bridge(&c.sky.saved());
            }
            Err(err) => {
                debug(&format!("search failed: {err}"));
                // Start over next time: the connection may be gone.
                if let Some(c) = conn.take() {
                    c.sky.close();
                }
            }
        }
        let _ = reply.send(result);
    }
}
