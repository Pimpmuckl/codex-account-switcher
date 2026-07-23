//! Import accounts from local cookie / token JSON payloads.
//!
//! Codex / OpenAI shapes:
//! - Browser cookie export array: `[{ "name", "value", "domain"? }, ...]`
//! - Cookie header object: `{ "cookie": "name=value; ..." }` or `{ "cookies": "..." }`
//! - Name/value map: `{ "id_token": "...", "access_token": "...", ... }`
//! - Codex `auth.json`-like: `{ "tokens": { "id_token", "access_token", "refresh_token", "account_id"? } }`
//! - Session-style: `{ "accessToken": "...", "user": { "email": "..." } }`
//!
//! Cursor shapes:
//! - Browser export with `WorkosCursorSessionToken` (and optional `url` for cursor.com)
//! - Cookie jar object `{ "url", "cookies": [ ... ] }` as exported by extensions

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Map, Value};
use time::OffsetDateTime;

use crate::identity::{parse_identity_from_auth_json, parse_identity_from_id_token};
use crate::model::{DisplayIdentity, SNAPSHOT_SCHEMA_VERSION, SnapshotBlob, SnapshotFile};

const SESSION_ENDPOINT: &str = "https://chatgpt.com/api/auth/session";

#[derive(Debug, Clone)]
pub struct CookieImportResult {
    pub identity: DisplayIdentity,
    pub snapshot: SnapshotBlob,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct CookieJar {
    by_name: HashMap<String, String>,
}

impl CookieJar {
    fn insert(&mut self, name: &str, value: &str) {
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return;
        }
        self.by_name.insert(name.to_owned(), value.to_owned());
    }

    fn get_ci(&self, name: &str) -> Option<&str> {
        if let Some(value) = self.by_name.get(name) {
            return Some(value.as_str());
        }
        let needle = name.to_ascii_lowercase();
        self.by_name
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&needle))
            .map(|(_, value)| value.as_str())
    }

    fn header_value(&self) -> String {
        self.by_name
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Parse local JSON text into a Codex snapshot. Never logs cookie/token values.
pub fn import_codex_from_cookies_json(json_text: &str) -> Result<CookieImportResult> {
    let trimmed = json_text.trim();
    if trimmed.is_empty() {
        bail!("cookie JSON is empty");
    }
    let value: Value = serde_json::from_str(trimmed).context("cookie payload is not valid JSON")?;

    let mut warnings = Vec::new();
    let mut jar = CookieJar::default();
    collect_cookies_from_value(&value, &mut jar);

    // Direct auth.json / tokens object wins when present.
    if let Some(auth_json) = try_build_auth_json_from_tokens(&value)? {
        return finalize_auth_json(auth_json, warnings);
    }

    // Tokens nested under common wrappers.
    for key in ["auth", "auth_json", "codex", "session", "data", "account"] {
        if let Some(child) = value.get(key)
            && let Some(auth_json) = try_build_auth_json_from_tokens(child)?
        {
            return finalize_auth_json(auth_json, warnings);
        }
    }

    // Token-like cookie names / JWT cookie values.
    if let Some(auth_json) = try_build_auth_json_from_cookie_jar(&jar)? {
        return finalize_auth_json(auth_json, warnings);
    }

    // Optional: exchange ChatGPT session cookie for access token.
    if !jar.is_empty()
        && (jar.get_ci("__Secure-next-auth.session-token").is_some()
            || jar.get_ci("next-auth.session-token").is_some()
            || jar.get_ci("__Secure-next-auth.session-token.0").is_some())
    {
        match exchange_session_cookie_for_auth(&jar) {
            Ok(auth_json) => {
                warnings.push(
                    "Imported via ChatGPT session cookie exchange. Refresh token may be missing; re-login if the session expires."
                        .to_owned(),
                );
                return finalize_auth_json(auth_json, warnings);
            }
            Err(error) => {
                warnings.push(format!("Session cookie exchange failed: {error:#}"));
            }
        }
    }

    bail!(
        "could not build a Codex auth.json from this JSON. Expected OpenAI/Codex tokens \
(id_token + access_token, preferably refresh_token), a Codex auth.json object, or ChatGPT \
session cookies that can be exchanged. Cursor/Claude cookie import is not supported."
    );
}

fn finalize_auth_json(auth_json: Value, warnings: Vec<String>) -> Result<CookieImportResult> {
    let bytes =
        serde_json::to_vec_pretty(&auth_json).context("failed to encode imported auth.json")?;
    let identity = parse_identity_from_auth_json(&bytes)
        .context("imported payload did not contain a usable id_token/email")?;
    let snapshot = SnapshotBlob {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        files: vec![
            SnapshotFile {
                name: "auth.json".to_owned(),
                bytes_base64: STANDARD.encode(bytes),
            },
            SnapshotFile {
                name: "cap_sid".to_owned(),
                bytes_base64: STANDARD.encode(b""),
            },
        ],
    };
    Ok(CookieImportResult {
        identity,
        snapshot,
        warnings,
    })
}

fn collect_cookies_from_value(value: &Value, jar: &mut CookieJar) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_cookies_from_value(item, jar);
            }
        }
        Value::Object(map) => {
            if let (Some(name), Some(cookie_value)) = (
                string_field(map, &["name", "Name", "key", "Key"]),
                string_field(map, &["value", "Value", "val", "Val"]),
            ) {
                jar.insert(&name, &cookie_value);
            }
            if let Some(header) = string_field(
                map,
                &["cookie", "Cookie", "cookies", "Cookies", "cookie_header"],
            ) {
                parse_cookie_header(&header, jar);
            }
            // Flat name→value map of cookie-like keys.
            for (key, val) in map {
                if let Some(text) = val.as_str() {
                    let lower = key.to_ascii_lowercase();
                    if lower.contains("token")
                        || lower.contains("cookie")
                        || lower.starts_with("__secure-")
                        || lower.starts_with("oai-")
                        || lower == "session"
                    {
                        if text.contains('=') && text.contains(';') && !looks_like_jwt(text) {
                            parse_cookie_header(text, jar);
                        } else if !key.eq_ignore_ascii_case("cookie")
                            && !key.eq_ignore_ascii_case("cookies")
                        {
                            jar.insert(key, text);
                        }
                    }
                } else if key.eq_ignore_ascii_case("cookies") || key.eq_ignore_ascii_case("cookie")
                {
                    collect_cookies_from_value(val, jar);
                }
            }
        }
        Value::String(text) if text.contains('=') => {
            parse_cookie_header(text, jar);
        }
        _ => {}
    }
}

fn parse_cookie_header(header: &str, jar: &mut CookieJar) {
    for part in header.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((name, value)) = part.split_once('=') {
            jar.insert(name, value);
        }
    }
}

fn try_build_auth_json_from_tokens(value: &Value) -> Result<Option<Value>> {
    let Some(map) = value.as_object() else {
        return Ok(None);
    };

    // Already an auth.json with tokens object.
    if let Some(tokens) = map.get("tokens").and_then(Value::as_object) {
        let access = token_from_map(tokens, &["access_token", "accessToken"]);
        let id = token_from_map(tokens, &["id_token", "idToken"]);
        let refresh = token_from_map(tokens, &["refresh_token", "refreshToken"]);
        let account_id = token_from_map(tokens, &["account_id", "accountId"]);
        if let (Some(access), Some(id)) = (access, id) {
            return Ok(Some(build_auth_json(
                access,
                id,
                refresh,
                account_id,
                map.get("last_refresh").and_then(Value::as_str),
            )));
        }
    }

    let access = token_from_map(
        map,
        &[
            "access_token",
            "accessToken",
            "openai_access_token",
            "chatgpt_access_token",
        ],
    );
    let id = token_from_map(
        map,
        &["id_token", "idToken", "openai_id_token", "chatgpt_id_token"],
    );
    let refresh = token_from_map(
        map,
        &[
            "refresh_token",
            "refreshToken",
            "openai_refresh_token",
            "chatgpt_refresh_token",
        ],
    );
    let account_id = token_from_map(map, &["account_id", "accountId", "chatgpt_account_id"]);

    if let (Some(access), Some(id)) = (access.clone(), id.clone()) {
        return Ok(Some(build_auth_json(
            access,
            id,
            refresh.clone(),
            account_id.clone(),
            None,
        )));
    }

    // Session API style: accessToken only — synthesize id_token from user email when possible.
    if let Some(access) = access {
        if let Some(id) = id {
            return Ok(Some(build_auth_json(access, id, refresh, account_id, None)));
        }
        if looks_like_jwt(&access) && parse_identity_from_id_token(&access).is_ok() {
            return Ok(Some(build_auth_json(
                access.clone(),
                access,
                refresh,
                account_id,
                None,
            )));
        }
        if let Some(email) = map
            .get("user")
            .and_then(Value::as_object)
            .and_then(|user| string_field(user, &["email", "Email"]))
        {
            let synthetic = synthesize_id_token(&email, account_id.as_deref())?;
            return Ok(Some(build_auth_json(
                access, synthetic, refresh, account_id, None,
            )));
        }
    }

    Ok(None)
}

fn try_build_auth_json_from_cookie_jar(jar: &CookieJar) -> Result<Option<Value>> {
    let access = first_cookie(
        jar,
        &[
            "access_token",
            "accessToken",
            "openai_access_token",
            "chatgpt_access_token",
        ],
    );
    let id = first_cookie(
        jar,
        &["id_token", "idToken", "openai_id_token", "chatgpt_id_token"],
    );
    let refresh = first_cookie(
        jar,
        &[
            "refresh_token",
            "refreshToken",
            "openai_refresh_token",
            "chatgpt_refresh_token",
        ],
    );
    let account_id = first_cookie(jar, &["account_id", "accountId", "chatgpt_account_id"]);

    if let (Some(access), Some(id)) = (access.clone(), id) {
        return Ok(Some(build_auth_json(access, id, refresh, account_id, None)));
    }

    // Any JWT cookie with an email claim can seed id_token; access may be the same JWT.
    for value in jar.by_name.values() {
        if !looks_like_jwt(value) {
            continue;
        }
        if parse_identity_from_id_token(value).is_ok() {
            let access = access.unwrap_or_else(|| value.clone());
            return Ok(Some(build_auth_json(
                access,
                value.clone(),
                refresh,
                account_id,
                None,
            )));
        }
    }

    Ok(None)
}

fn exchange_session_cookie_for_auth(jar: &CookieJar) -> Result<Value> {
    let cookie_header = jar.header_value();
    let mut response = ureq::get(SESSION_ENDPOINT)
        .header("Cookie", &cookie_header)
        .header("User-Agent", "codex-account-switcher")
        .header("Accept", "application/json")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(12)))
        .build()
        .call()
        .context("failed to contact ChatGPT session endpoint")?;
    let status = response.status();
    let body = response
        .body_mut()
        .read_to_string()
        .context("failed to read ChatGPT session response")?;
    if status.as_u16() < 200 || status.as_u16() >= 300 {
        bail!("ChatGPT session endpoint returned HTTP {}", status.as_u16());
    }
    let session: Value =
        serde_json::from_str(&body).context("ChatGPT session response was not JSON")?;
    try_build_auth_json_from_tokens(&session)?
        .context("ChatGPT session response did not include usable tokens")
}

fn build_auth_json(
    access_token: String,
    id_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
    last_refresh: Option<&str>,
) -> Value {
    let mut tokens = Map::new();
    tokens.insert("access_token".to_owned(), Value::String(access_token));
    tokens.insert("id_token".to_owned(), Value::String(id_token));
    tokens.insert(
        "refresh_token".to_owned(),
        Value::String(refresh_token.unwrap_or_default()),
    );
    if let Some(account_id) = account_id {
        tokens.insert("account_id".to_owned(), Value::String(account_id));
    }
    let mut root = Map::new();
    root.insert("tokens".to_owned(), Value::Object(tokens));
    root.insert(
        "last_refresh".to_owned(),
        Value::String(
            last_refresh
                .map(str::to_owned)
                .unwrap_or_else(|| OffsetDateTime::now_utc().to_string()),
        ),
    );
    Value::Object(root)
}

fn synthesize_id_token(email: &str, subject: Option<&str>) -> Result<String> {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
    let mut claims = Map::new();
    claims.insert("email".to_owned(), Value::String(email.to_owned()));
    if let Some(sub) = subject {
        claims.insert("sub".to_owned(), Value::String(sub.to_owned()));
    } else {
        claims.insert("sub".to_owned(), Value::String(format!("cookie:{email}")));
    }
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&Value::Object(claims)).context("failed to encode synthetic JWT")?,
    );
    Ok(format!("{header}.{payload}."))
}

fn first_cookie(jar: &CookieJar, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| jar.get_ci(name).map(str::to_owned))
}

fn token_from_map(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    string_field(map, keys).filter(|value| !value.is_empty())
}

fn string_field(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = map.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    // Case-insensitive fallback.
    for key in keys {
        let needle = key.to_ascii_lowercase();
        if let Some((_, value)) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case(&needle))
            && let Some(text) = value.as_str()
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn looks_like_jwt(value: &str) -> bool {
    let mut parts = value.split('.');
    parts.next().is_some() && parts.next().is_some() && value.matches('.').count() >= 2
}

pub fn unsupported_provider_message(provider: &str) -> String {
    match provider {
        "claude" => {
            "Claude accounts use Claude Code credentials/Keychain, not browser cookies. Use Backup/Save on a live Claude session instead.".to_owned()
        }
        other => format!("Cookie JSON import is not supported for provider '{other}'"),
    }
}

/// Detect provider from payload content when the UI does not specify one.
pub fn detect_provider_from_json(json_text: &str) -> &'static str {
    let lower = json_text.to_ascii_lowercase();
    if lower.contains("workoscursorsessiontoken")
        || lower.contains("\"cursor.com\"")
        || lower.contains("cursorauth/")
    {
        return "cursor";
    }
    "codex"
}

/// Import a Cursor account from browser cookie export JSON.
///
/// Uses `WorkosCursorSessionToken` (+ optional profile APIs) to build a
/// `cursor_auth.json` snapshot compatible with the switcher store.
pub fn import_cursor_from_cookies_json(json_text: &str) -> Result<CookieImportResult> {
    let trimmed = json_text.trim();
    if trimmed.is_empty() {
        bail!("cookie JSON is empty");
    }
    let value: Value = serde_json::from_str(trimmed).context("cookie payload is not valid JSON")?;

    let mut warnings = Vec::new();
    let mut jar = CookieJar::default();
    collect_cookies_from_value(&value, &mut jar);

    // Direct token map (IDE export style).
    if let Some(result) = try_build_cursor_from_token_map(&value)? {
        return Ok(result);
    }

    let session = jar
        .get_ci("WorkosCursorSessionToken")
        .or_else(|| jar.get_ci("workos_cursor_session_token"))
        .map(str::to_owned)
        .context(
            "no WorkosCursorSessionToken cookie found. Export cookies from cursor.com while signed in.",
        )?;

    let session = urlencoding_decode(&session);
    let (user_id, access_token) = split_cursor_session_token(&session)?;

    let profile = fetch_cursor_web_profile(&access_token, &session)
        .map_err(|e| {
            warnings.push(format!("Could not refresh Cursor profile online: {e:#}"));
            e
        })
        .ok();

    let email = profile
        .as_ref()
        .and_then(|p| p.email.clone())
        .or_else(|| guess_email_from_payload(&value))
        .unwrap_or_else(|| format!("{user_id}@cursor.local"));

    let name = profile.as_ref().and_then(|p| p.name.clone());
    let plan = profile
        .as_ref()
        .and_then(|p| p.membership_type.clone())
        .unwrap_or_else(|| "pro".to_owned());

    if profile.is_none() {
        warnings.push(
            "Imported from WorkosCursorSessionToken without live profile; email may be incomplete until Cursor validates the session."
                .to_owned(),
        );
    }
    warnings.push(
        "Cursor web session token imported. Activate this account while Cursor is closed for best results."
            .to_owned(),
    );

    build_cursor_import_result(&email, name.as_deref(), &plan, &access_token, &session, warnings)
}

#[derive(Debug, Default)]
struct CursorWebProfile {
    email: Option<String>,
    name: Option<String>,
    membership_type: Option<String>,
}

fn urlencoding_decode(value: &str) -> String {
    // Minimal decode for %3A%3A → :: and similar in cookie exports.
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn split_cursor_session_token(session: &str) -> Result<(String, String)> {
    if let Some((user, jwt)) = session.split_once("::") {
        if user.is_empty() || jwt.is_empty() {
            bail!("WorkosCursorSessionToken is malformed");
        }
        return Ok((user.to_owned(), jwt.to_owned()));
    }
    if looks_like_jwt(session) {
        return Ok(("cursor-user".to_owned(), session.to_owned()));
    }
    bail!("WorkosCursorSessionToken is not a user::jwt session token");
}

fn fetch_cursor_web_profile(access_token: &str, full_session: &str) -> Result<CursorWebProfile> {
    let mut profile = CursorWebProfile::default();

    let mut me = ureq::get("https://cursor.com/api/auth/me")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header(
            "Cookie",
            &format!("WorkosCursorSessionToken={full_session}"),
        )
        .header("User-Agent", "codex-account-switcher")
        .header("Accept", "application/json")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(12)))
        .build()
        .call()
        .context("Cursor /api/auth/me request failed")?;
    let me_status = me.status().as_u16();
    let me_body = me
        .body_mut()
        .read_to_string()
        .context("failed to read Cursor /api/auth/me body")?;
    if !(200..300).contains(&me_status) {
        bail!("Cursor /api/auth/me returned HTTP {me_status}");
    }
    let me_json: Value =
        serde_json::from_str(&me_body).context("failed to decode Cursor /api/auth/me JSON")?;
    profile.email = me_json
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned);
    profile.name = me_json
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);

    if let Ok(mut stripe) = ureq::get("https://api2.cursor.sh/auth/full_stripe_profile")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("User-Agent", "codex-account-switcher")
        .header("Accept", "application/json")
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(12)))
        .build()
        .call()
    {
        let status = stripe.status().as_u16();
        if (200..300).contains(&status)
            && let Ok(body) = stripe.body_mut().read_to_string()
            && let Ok(stripe_json) = serde_json::from_str::<Value>(&body)
        {
            profile.membership_type = stripe_json
                .get("membershipType")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }

    if profile.email.is_none() {
        bail!("Cursor profile response missing email");
    }
    Ok(profile)
}

fn guess_email_from_payload(value: &Value) -> Option<String> {
    // Some exporters nest email at the top level.
    value
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn try_build_cursor_from_token_map(value: &Value) -> Result<Option<CookieImportResult>> {
    let map = match value {
        Value::Object(map) => map,
        _ => return Ok(None),
    };
    let access = map
        .get("cursorAuth/accessToken")
        .or_else(|| map.get("accessToken"))
        .or_else(|| map.get("access_token"))
        .and_then(Value::as_str);
    let email = map
        .get("cursorAuth/cachedEmail")
        .or_else(|| map.get("email"))
        .and_then(Value::as_str);
    let (Some(access), Some(email)) = (access, email) else {
        return Ok(None);
    };
    let refresh = map
        .get("cursorAuth/refreshToken")
        .or_else(|| map.get("refreshToken"))
        .or_else(|| map.get("refresh_token"))
        .and_then(Value::as_str)
        .unwrap_or(access);
    let plan = map
        .get("cursorAuth/stripeMembershipType")
        .or_else(|| map.get("plan"))
        .and_then(Value::as_str)
        .unwrap_or("pro");
    let name = map.get("name").and_then(Value::as_str);
    Ok(Some(build_cursor_import_result(
        email,
        name,
        plan,
        access,
        refresh,
        vec!["Imported Cursor tokens from JSON map.".to_owned()],
    )?))
}

fn build_cursor_import_result(
    email: &str,
    name: Option<&str>,
    plan: &str,
    access_token: &str,
    refresh_or_session: &str,
    warnings: Vec<String>,
) -> Result<CookieImportResult> {
    let mut auth_map: HashMap<String, String> = HashMap::new();
    auth_map.insert(
        "cursorAuth/cachedEmail".to_owned(),
        STANDARD.encode(email.as_bytes()),
    );
    auth_map.insert(
        "cursorAuth/accessToken".to_owned(),
        STANDARD.encode(access_token.as_bytes()),
    );
    auth_map.insert(
        "cursorAuth/refreshToken".to_owned(),
        STANDARD.encode(refresh_or_session.as_bytes()),
    );
    auth_map.insert(
        "cursorAuth/stripeMembershipType".to_owned(),
        STANDARD.encode(plan.as_bytes()),
    );
    if let Some(name) = name {
        let profile = serde_json::json!({ "displayName": name }).to_string();
        auth_map.insert(
            "cursorAuth/cachedScopedProfile".to_owned(),
            STANDARD.encode(profile.as_bytes()),
        );
    }

    let json_bytes =
        serde_json::to_vec_pretty(&auth_map).context("failed to encode cursor_auth.json")?;
    let plan_label = if plan.eq_ignore_ascii_case("pro") {
        Some("Pro".to_owned())
    } else if plan.eq_ignore_ascii_case("free") {
        Some("Free".to_owned())
    } else {
        Some(plan.to_owned())
    };

    // Subject from JWT when possible.
    let subject = access_token
        .split('.')
        .nth(1)
        .and_then(|payload| {
            let pad = "=".repeat((4 - payload.len() % 4) % 4);
            URL_SAFE_NO_PAD
                .decode(format!("{payload}{pad}"))
                .ok()
                .or_else(|| URL_SAFE_NO_PAD.decode(payload).ok())
        })
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|claims| {
            claims
                .get("sub")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });

    let identity = DisplayIdentity {
        email: email.to_owned(),
        subject,
        name: name.map(str::to_owned),
        plan_label,
        workspace_id: None,
        workspace_name: None,
    };

    let snapshot = SnapshotBlob {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        files: vec![SnapshotFile {
            name: "cursor_auth.json".to_owned(),
            bytes_base64: STANDARD.encode(json_bytes),
        }],
    };

    Ok(CookieImportResult {
        identity,
        snapshot,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn jwt(payload: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(payload);
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn imports_tokens_object() {
        let id = jwt(r#"{"email":"a@example.com","sub":"s1"}"#);
        let json = format!(
            r#"{{"tokens":{{"access_token":"access","id_token":"{id}","refresh_token":"refresh","account_id":"acct"}}}}"#
        );
        let result = import_codex_from_cookies_json(&json).expect("import");
        assert_eq!(result.identity.email, "a@example.com");
        assert_eq!(result.snapshot.files.len(), 2);
    }

    #[test]
    fn imports_cookie_array_with_token_names() {
        let id = jwt(r#"{"email":"b@example.com","sub":"s2"}"#);
        let json = format!(
            r#"[{{"name":"access_token","value":"access"}},{{"name":"id_token","value":"{id}"}},{{"name":"refresh_token","value":"rt"}}]"#
        );
        let result = import_codex_from_cookies_json(&json).expect("import");
        assert_eq!(result.identity.email, "b@example.com");
    }

    #[test]
    fn imports_cookie_header_object() {
        let id = jwt(r#"{"email":"c@example.com","sub":"s3"}"#);
        let json =
            format!(r#"{{"cookie":"access_token=access; id_token={id}; refresh_token=rt"}}"#);
        let result = import_codex_from_cookies_json(&json).expect("import");
        assert_eq!(result.identity.email, "c@example.com");
    }

    #[test]
    fn rejects_empty_and_non_token_cookies() {
        assert!(import_codex_from_cookies_json("").is_err());
        assert!(import_codex_from_cookies_json(r#"[{"name":"foo","value":"bar"}]"#).is_err());
    }
}
