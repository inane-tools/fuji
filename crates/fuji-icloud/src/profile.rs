//! The account holder's picture, from their iCloud Contacts "me" card (the
//! same picture icloud.com shows in its account menu).

use serde_json::Value;

use crate::auth::{Account, AuthClient};

const CLIENT_BUILD_NUMBER: &str = "2522Project44";
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Download the account picture. `None` if there isn't one, or it couldn't
/// be fetched: it's decoration, so failures aren't reported.
///
/// Blocking I/O: call from a background thread.
pub fn account_picture(auth: &AuthClient, account: &Account) -> Option<Vec<u8>> {
    let root = account.webservices.get("contacts")?;
    let agent = auth.agent();
    let cookies = auth.cookie_log();
    let mut resp = agent
        .get(format!("{}/co/mecard/", root.trim_end_matches('/')))
        .query("clientBuildNumber", CLIENT_BUILD_NUMBER)
        .query("clientMasteringNumber", CLIENT_BUILD_NUMBER)
        .query("clientId", auth.client_id())
        .query("dsid", &account.dsid)
        .query("clientVersion", "2.1")
        .query("locale", "en_US")
        .query("order", "last,first")
        .header("Origin", "https://www.icloud.com")
        .header("Referer", "https://www.icloud.com/")
        .call()
        .ok()?;
    cookies.record(&resp);
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.body_mut().read_json().ok()?;
    let url = picture_url(&data)?;

    let mut resp = agent.get(&url).call().ok()?;
    cookies.record(&resp);
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
        .ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// The picture of the "me" card in a `/co/mecard/` response.
fn picture_url(data: &Value) -> Option<String> {
    let contacts = data.get("contacts")?.as_array()?;
    let me = data.get("meCardId").and_then(Value::as_str);
    let card = contacts
        .iter()
        .find(|c| me.is_some() && c.get("contactId").and_then(Value::as_str) == me)
        .or_else(|| contacts.first())?;
    let url = card.get("photo")?.get("url")?.as_str()?;
    url.starts_with("https://").then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_the_me_card_picture() {
        let data = json!({
            "meCardId": "B",
            "contacts": [
                { "contactId": "A", "photo": { "url": "https://x/a.jpg" } },
                { "contactId": "B", "photo": { "url": "https://x/b.jpg" } },
            ],
        });
        assert_eq!(picture_url(&data).as_deref(), Some("https://x/b.jpg"));
    }

    #[test]
    fn no_picture_is_none() {
        assert_eq!(picture_url(&json!({ "contacts": [] })), None);
        assert_eq!(
            picture_url(&json!({ "contacts": [{ "contactId": "A" }] })),
            None
        );
        // Only fetch over HTTPS.
        let plain = json!({ "contacts": [{ "photo": { "url": "http://x/a.jpg" } }] });
        assert_eq!(picture_url(&plain), None);
    }
}
