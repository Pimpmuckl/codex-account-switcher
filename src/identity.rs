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
    let mut identity = parse_identity_from_id_token(id_token)?;
    let workspace = extract_workspace(&auth_json);
    identity.workspace_id = identity.workspace_id.or(workspace.0);
    identity.workspace_name = identity.workspace_name.or(workspace.1);
    Ok(identity)
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
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("JWT payload did not contain email"))?
        .to_owned();
    let subject = claims.get("sub").and_then(Value::as_str).map(str::to_owned);
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
    let (workspace_id, workspace_name) = extract_workspace(&claims);
    Ok(DisplayIdentity {
        email,
        subject,
        name,
        plan_label,
        workspace_id,
        workspace_name,
    })
}

fn extract_workspace(value: &Value) -> (Option<String>, Option<String>) {
    let mut workspace_id = workspace_id_from_object(value, false);
    let mut workspace_name = workspace_name_from_object(value, false);

    for key in [
        "https://api.openai.com/auth",
        "auth",
        "workspace",
        "organization",
        "org",
        "account",
        "tokens",
    ] {
        let Some(child) = value.get(key) else {
            continue;
        };
        workspace_id = workspace_id.or_else(|| workspace_id_from_object(child, true));
        workspace_name = workspace_name.or_else(|| workspace_name_from_object(child, true));
    }

    (workspace_id, workspace_name)
}

fn workspace_id_from_object(value: &Value, allow_generic_id: bool) -> Option<String> {
    let keys = [
        "workspace_id",
        "workspaceId",
        "active_workspace_id",
        "activeWorkspaceId",
        "organization_id",
        "organizationId",
        "org_id",
        "orgId",
        "account_id",
        "accountId",
        "chatgpt_account_id",
        "chatgptAccountId",
    ];
    keys.iter()
        .find_map(|key| string_field(value, key))
        .or_else(|| {
            allow_generic_id
                .then(|| string_field(value, "id"))
                .flatten()
        })
}

fn workspace_name_from_object(value: &Value, allow_generic_name: bool) -> Option<String> {
    let keys = [
        "workspace_name",
        "workspaceName",
        "active_workspace_name",
        "activeWorkspaceName",
        "organization_name",
        "organizationName",
        "org_name",
        "orgName",
        "account_name",
        "accountName",
        "chatgpt_account_name",
        "chatgptAccountName",
    ];
    keys.iter()
        .find_map(|key| string_field(value, key))
        .or_else(|| {
            allow_generic_name
                .then(|| string_field(value, "name"))
                .flatten()
        })
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
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
            r#"{"email":"person@example.com","sub":"abc","name":"Jane","https://api.openai.com/auth":{"chatgpt_plan_type":"pro_lite"}}"#,
        );
        let identity = parse_identity_from_id_token(&token).expect("identity");
        assert_eq!(identity.email, "person@example.com");
        assert_eq!(identity.subject.as_deref(), Some("abc"));
        assert_eq!(identity.plan_label.as_deref(), Some("Pro Lite"));
    }

    #[test]
    fn parses_workspace_from_openai_auth_claims() {
        let token = token(
            r#"{"email":"person@example.com","sub":"abc","https://api.openai.com/auth":{"workspace_id":"ws_123","workspace_name":"Team Space"}}"#,
        );
        let identity = parse_identity_from_id_token(&token).expect("identity");
        assert_eq!(identity.workspace_id.as_deref(), Some("ws_123"));
        assert_eq!(identity.workspace_name.as_deref(), Some("Team Space"));
    }

    #[test]
    fn auth_json_workspace_fills_missing_token_workspace() {
        let token = token(r#"{"email":"person@example.com","sub":"abc"}"#);
        let auth_json = format!(
            r#"{{"workspace_id":"ws_456","workspace_name":"Other Space","tokens":{{"id_token":"{token}"}}}}"#
        );
        let identity = parse_identity_from_auth_json(auth_json.as_bytes()).expect("identity");
        assert_eq!(identity.workspace_id.as_deref(), Some("ws_456"));
        assert_eq!(identity.workspace_name.as_deref(), Some("Other Space"));
    }
}
