//! ElevenLabs account information: the credit balance
//! (GET /v1/user/subscription, needs the `user_read` permission) and the
//! per-model credit multipliers (GET /v1/models, needs `models_read`).
//!
//! Responses are read field by field from JSON, so an unexpected type or a
//! `null` in one field never loses the rest.

use std::time::Duration;

use serde_json::Value;

use super::{ElevenLabsClient, error_message};

/// The parts of the subscription the app shows. The "character" counters
/// are credits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Subscription {
    pub tier: Option<String>,
    pub status: Option<String>,
    pub character_count: Option<i64>,
    pub character_limit: Option<i64>,
    pub next_character_count_reset_unix: Option<i64>,
    pub voice_slots_used: Option<i64>,
    pub voice_limit: Option<i64>,
    pub professional_voice_slots_used: Option<i64>,
    pub professional_voice_limit: Option<i64>,
    /// Usage-based billing may continue past the limit.
    pub can_extend_character_limit: Option<bool>,
    /// `max_credit_limit_extension`: a number, or "unlimited".
    pub max_credit_limit_extension: Option<String>,
}

fn int(v: &Value, k: &str) -> Option<i64> {
    v[k].as_i64()
        .or_else(|| v[k].as_f64().map(|f| f.round() as i64))
}

fn text(v: &Value, k: &str) -> Option<String> {
    match &v[k] {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Subscription {
    pub fn from_json(v: &Value) -> Self {
        Subscription {
            tier: text(v, "tier"),
            status: text(v, "status"),
            character_count: int(v, "character_count"),
            character_limit: int(v, "character_limit"),
            next_character_count_reset_unix: int(v, "next_character_count_reset_unix"),
            voice_slots_used: int(v, "voice_slots_used"),
            voice_limit: int(v, "voice_limit"),
            professional_voice_slots_used: int(v, "professional_voice_slots_used"),
            professional_voice_limit: int(v, "professional_voice_limit"),
            can_extend_character_limit: v["can_extend_character_limit"].as_bool(),
            max_credit_limit_extension: text(v, "max_credit_limit_extension"),
        }
    }

    /// Credits left this period: `max(0, limit - used)`.
    pub fn remaining(&self) -> Option<i64> {
        Some((self.character_limit? - self.character_count.unwrap_or(0)).max(0))
    }

    /// Whether usage-based billing can continue past the limit.
    pub fn overage_allowed(&self) -> bool {
        self.can_extend_character_limit == Some(true)
            && self
                .max_credit_limit_extension
                .as_deref()
                .is_some_and(|x| x != "0")
    }
}

/// A model's credits per character from /v1/models
/// (`character_cost_multiplier` × `cost_discount_multiplier`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRate {
    pub model_id: String,
    pub multiplier: f64,
}

pub fn parse_models(v: &Value) -> Vec<ModelRate> {
    let list = v
        .as_array()
        .or_else(|| v["models"].as_array())
        .cloned()
        .unwrap_or_default();
    list.iter()
        .filter_map(|m| {
            let model_id = m["model_id"].as_str()?.to_string();
            let rates = &m["model_rates"];
            let base = rates["character_cost_multiplier"].as_f64()?;
            let discount = rates["cost_discount_multiplier"].as_f64().unwrap_or(1.0);
            let multiplier = base * discount;
            (multiplier.is_finite() && multiplier >= 0.0).then_some(ModelRate {
                model_id,
                multiplier,
            })
        })
        .collect()
}

/// What the worker reports after fetching the account.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    /// The balance, or why it is unavailable.
    pub subscription: Result<Subscription, String>,
    /// Models whose list rate was refreshed.
    pub model_rates: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountError {
    /// The key works but lacks this permission (scope).
    MissingPermission(&'static str),
    Rejected {
        status: u16,
        message: String,
    },
    Transport(String),
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountError::MissingPermission(scope) => {
                write!(f, "the API key lacks the {scope} permission")
            }
            AccountError::Rejected { status: 401, .. } => {
                write!(f, "the API key was rejected (HTTP 401)")
            }
            AccountError::Rejected { status, message } => {
                write!(f, "HTTP {status}: {message}")
            }
            AccountError::Transport(m) => write!(f, "unreachable: {m}"),
        }
    }
}

/// 401 `missing_permissions` (legacy, in `detail.status`) or 403
/// `insufficient_permissions` (current, in `detail.code` / `detail.type`):
/// the key lacks a scope, which is not the same as an invalid key.
pub fn is_missing_permission(status: u16, body: &str) -> bool {
    if status != 401 && status != 403 {
        return false;
    }
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let d = &v["detail"];
    ["status", "code", "type"].iter().any(|k| {
        matches!(
            d[*k].as_str(),
            Some("missing_permissions" | "insufficient_permissions")
        )
    }) || body.contains("missing_permissions")
        || body.contains("insufficient_permissions")
}

impl ElevenLabsClient {
    async fn get_json(&self, path: &str, scope: &'static str) -> Result<Value, AccountError> {
        let resp = self
            .http
            .get(format!("{}/{path}", self.base))
            .header("xi-api-key", &self.api_key)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| AccountError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| AccountError::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            if is_missing_permission(status, &body) {
                return Err(AccountError::MissingPermission(scope));
            }
            return Err(AccountError::Rejected {
                status,
                message: error_message(&body),
            });
        }
        serde_json::from_str(&body).map_err(|e| AccountError::Transport(e.to_string()))
    }

    /// GET /v1/user/subscription.
    pub async fn subscription(&self) -> Result<Subscription, AccountError> {
        let v = self.get_json("user/subscription", "user_read").await?;
        Ok(Subscription::from_json(&v))
    }

    /// GET /v1/models: the credit multiplier of each model.
    pub async fn models(&self) -> Result<Vec<ModelRate>, AccountError> {
        let v = self.get_json("models", "models_read").await?;
        Ok(parse_models(&v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subscription_fields_and_balance() {
        let s = Subscription::from_json(&json!({
            "tier": "creator", "status": "active",
            "character_count": 18_700, "character_limit": 431_000,
            "next_character_count_reset_unix": 1_793_664_000,
            "voice_slots_used": 4, "voice_limit": 30,
            "professional_voice_slots_used": 0, "professional_voice_limit": 1,
            "can_extend_character_limit": true, "max_credit_limit_extension": "unlimited",
            "currency": null, "open_invoices": null
        }));
        assert_eq!(s.remaining(), Some(412_300));
        assert_eq!(s.tier.as_deref(), Some("creator"));
        assert!(s.overage_allowed());

        // Over the limit clamps to 0; nulls and odd types do not break parsing.
        let s = Subscription::from_json(&json!({
            "character_count": 12.0, "character_limit": 10, "voice_limit": null,
            "max_credit_limit_extension": 0, "tier": {"weird": true}
        }));
        assert_eq!(s.remaining(), Some(0));
        assert_eq!(s.voice_limit, None);
        assert_eq!(s.tier, None);
        assert!(!s.overage_allowed());
        assert_eq!(Subscription::default().remaining(), None);
    }

    #[test]
    fn model_rates() {
        let rates = parse_models(&json!([
            {"model_id": "eleven_v4", "model_rates": {"character_cost_multiplier": 1, "cost_discount_multiplier": 1}},
            {"model_id": "eleven_flash_v2_5", "model_rates": {"character_cost_multiplier": 0.5}},
            {"model_id": "eleven_v3", "model_rates": {"character_cost_multiplier": 1, "cost_discount_multiplier": 0.8}},
            {"model_id": "no_rates", "model_rates": null},
            {"name": "no id"}
        ]));
        assert_eq!(rates.len(), 3);
        assert_eq!(rates[1].multiplier, 0.5);
        assert!((rates[2].multiplier - 0.8).abs() < 1e-12);
    }

    #[test]
    fn permission_errors_are_not_invalid_keys() {
        assert!(is_missing_permission(
            401,
            r#"{"detail":{"status":"missing_permissions","message":"The API key you used is missing the permission user_read"}}"#
        ));
        assert!(is_missing_permission(
            403,
            r#"{"detail":{"type":"authorization_error","code":"insufficient_permissions"}}"#
        ));
        assert!(!is_missing_permission(
            401,
            r#"{"detail":{"status":"invalid_api_key"}}"#
        ));
        assert!(!is_missing_permission(500, "missing_permissions"));
        assert_eq!(
            AccountError::MissingPermission("user_read").to_string(),
            "the API key lacks the user_read permission"
        );
    }
}
