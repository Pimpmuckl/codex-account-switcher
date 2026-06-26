use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::model::DisplayIdentity;

pub fn parse_identity_from_auth_json(auth_json_bytes: &[u8]) -> Result<DisplayIdentity> {
    let auth_json: Value =
        serde_json::from_slice(auth_json_bytes).context("failed to parse auth.json")?;
    let id_token = auth_json
        .get("tokens")
        .and_then(|tokens| tokens.get("id_token"))
        .and_then(Value::as_str)
        .context("auth.json did not contain tokens.id_token")?;
    parse_identity_from_id_token(id_token)
}

pub fn parse_identity_from_id_token(id_token: &str) -> Result<DisplayIdentity> {
    let mut parts = id_token.split('.');
    let _header = parts.next().context("JWT header missing")?;
    let payload = parts.next().context("JWT payload missing")?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            let padding = "=".repeat((4 - payload.len() % 4) % 4);
            URL_SAFE_NO_PAD.decode(format!("{payload}{padding}"))
        })
        .context("failed to decode JWT payload")?;
    let claims: Value =
        serde_json::from_slice(&payload_bytes).context("failed to parse JWT payload")?;
    let email = string_claim(&claims, "email")
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(Value::as_object)
                .and_then(|profile| object_string_claim(profile, "email"))
        })
        .ok_or_else(|| anyhow!("JWT payload did not contain email"))?;
    let auth_claims = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object);
    let account_key = auth_claims
        .and_then(|auth| object_string_claim(auth, "chatgpt_account_id"))
        .or_else(|| auth_claims.and_then(|auth| object_string_claim(auth, "chatgpt_user_id")))
        .or_else(|| auth_claims.and_then(|auth| object_string_claim(auth, "user_id")))
        .or_else(|| string_claim(&claims, "sub"))
        .ok_or_else(|| anyhow!("JWT payload did not contain a supported ChatGPT account id"))?;
    let name = claims
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let plan_label = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .and_then(normalize_plan_label);
    Ok(DisplayIdentity {
        account_key,
        email,
        name,
        plan_label,
    })
}

fn string_claim(claims: &Value, key: &str) -> Option<String> {
    claims
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn object_string_claim(claims: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    claims
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn normalize_plan_label(raw: &str) -> Option<String> {
    let normalized = raw.replace(['-', '_'], " ").trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let label = match normalized.as_str() {
        "free" => "Free".to_owned(),
        "plus" => "Plus".to_owned(),
        "pro" => "Pro".to_owned(),
        "pro lite" => "Pro Lite".to_owned(),
        "team" => "Team".to_owned(),
        "enterprise" => "Enterprise".to_owned(),
        other => other
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => {
                        let mut result = first.to_uppercase().collect::<String>();
                        result.push_str(chars.as_str());
                        result
                    }
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    };
    Some(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn token(payload: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(payload);
        format!("{header}.{payload}.")
    }

    #[test]
    fn parses_email_and_plan() {
        let token = token(
            r#"{"email":"person@example.com","sub":"abc","name":"Jane","https://api.openai.com/auth":{"chatgpt_account_id":"account-123","chatgpt_plan_type":"pro_lite"}}"#,
        );
        let identity = parse_identity_from_id_token(&token).expect("identity");
        assert_eq!(identity.email, "person@example.com");
        assert_eq!(identity.account_key, "account-123");
        assert_eq!(identity.plan_label.as_deref(), Some("Pro Lite"));
    }

    #[test]
    fn falls_back_to_subject_when_chatgpt_account_claim_is_missing() {
        let token = token(r#"{"email":"person@example.com","sub":"sub-123"}"#);
        let identity = parse_identity_from_id_token(&token).expect("identity");

        assert_eq!(identity.account_key, "sub-123");
    }

    #[test]
    fn rejects_tokens_without_account_key() {
        let token = token(r#"{"email":"person@example.com"}"#);
        let error = parse_identity_from_id_token(&token).expect_err("identity should fail");

        assert!(format!("{error:#}").contains("supported ChatGPT account id"));
    }
}
