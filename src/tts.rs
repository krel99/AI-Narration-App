//! ElevenLabs text-to-speech client.

pub mod account;
pub mod voices;

pub use voices::VoiceInfo;

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::model::VoiceSettings;

pub const API_BASE: &str = "https://api.elevenlabs.io/v1";

#[derive(Debug, Clone, Serialize)]
pub struct TtsRequest {
    #[serde(skip)]
    pub voice_id: String,
    #[serde(skip)]
    pub output_format: String,
    pub text: String,
    pub model_id: String,
    pub voice_settings: VoiceSettings,
    pub seed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_code: Option<String>,
    /// Neighbouring segments' text, for prosody continuity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_text: Option<String>,
    /// Request ids of the takes just before this segment (at most 3).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub previous_request_ids: Vec<String>,
}

#[derive(Debug)]
pub struct TtsResponse {
    pub audio: Vec<u8>,
    pub request_id: Option<String>,
    /// Characters billed, from the response headers when present.
    pub character_cost: Option<i64>,
}

#[derive(Debug)]
pub enum TtsError {
    /// 429: too many concurrent requests or a rate limit. Back off and retry.
    RateLimited {
        retry_after: Option<Duration>,
        message: String,
    },
    /// 5xx or network failure; worth retrying.
    Transient(String),
    /// The request itself was rejected (bad voice id, no credits, …).
    Rejected { status: u16, message: String },
}

impl std::fmt::Display for TtsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TtsError::RateLimited { message, .. } => write!(f, "rate limited: {message}"),
            TtsError::Transient(m) => write!(f, "temporary ElevenLabs error: {m}"),
            TtsError::Rejected { status, message } => {
                write!(f, "ElevenLabs HTTP {status}: {message}")
            }
        }
    }
}

impl std::error::Error for TtsError {}

/// Longest pause a Retry-After header can ask for; longer waits back off
/// normally instead.
const MAX_RETRY_AFTER_S: f64 = 300.0;

/// A Retry-After value in seconds. Negative, infinite, NaN or huge values
/// (and HTTP dates) give `None`, i.e. the normal backoff: never a panic.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && (0.0..=MAX_RETRY_AFTER_S).contains(s))
        .map(Duration::from_secs_f64)
}

fn error_message(body: &str) -> String {
    let v: Option<Value> = serde_json::from_str(body).ok();
    v.as_ref()
        .and_then(|v| {
            v["detail"]["message"]
                .as_str()
                .or_else(|| v["detail"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

pub struct ElevenLabsClient {
    http: reqwest::Client,
    api_key: String,
    base: String,
}

impl ElevenLabsClient {
    pub fn new(http: reqwest::Client, api_key: String, base: &str) -> Self {
        Self {
            http,
            api_key,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub async fn synthesize(&self, req: &TtsRequest) -> Result<TtsResponse, TtsError> {
        // Both values are plain identifiers (letters, digits, underscores).
        let url = format!(
            "{}/text-to-speech/{}?output_format={}",
            self.base,
            req.voice_id.trim(),
            req.output_format
        );
        let resp = self
            .http
            .post(url)
            .header("xi-api-key", &self.api_key)
            .timeout(Duration::from_secs(180))
            .json(req)
            .send()
            .await
            .map_err(|e| TtsError::Transient(e.to_string()))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        if status.as_u16() == 429 {
            let retry_after = header("retry-after").and_then(|s| parse_retry_after(&s));
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::RateLimited {
                retry_after,
                message: error_message(&body),
            });
        }
        if status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::Transient(format!(
                "HTTP {}: {}",
                status.as_u16(),
                error_message(&body)
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::Rejected {
                status: status.as_u16(),
                message: error_message(&body),
            });
        }
        let request_id = header("request-id");
        let character_cost = header("character-cost")
            .or_else(|| header("x-character-count"))
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|c| c.is_finite() && *c >= 0.0)
            .map(|c| c.round() as i64);
        let audio = resp
            .bytes()
            .await
            .map_err(|e| TtsError::Transient(e.to_string()))?
            .to_vec();
        if audio.is_empty() {
            return Err(TtsError::Transient("empty audio response".into()));
        }
        Ok(TtsResponse {
            audio,
            request_id,
            character_cost,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_shape() {
        let req = TtsRequest {
            voice_id: "voice".into(),
            output_format: "pcm_44100".into(),
            text: "\"It is,\"".into(),
            model_id: "eleven_multilingual_v2".into(),
            voice_settings: VoiceSettings::default(),
            seed: 7,
            language_code: None,
            previous_text: Some("SOME STORY...".into()),
            next_text: None,
            previous_request_ids: vec![],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["seed"], 7);
        assert_eq!(v["previous_text"], "SOME STORY...");
        assert!(v.get("voice_id").is_none());
        assert!(v.get("next_text").is_none());
        assert!(v.get("previous_request_ids").is_none());
        assert_eq!(v["voice_settings"]["similarity_boost"], 0.75);
    }

    #[test]
    fn retry_after_values_never_panic() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after(" 0.5 "), Some(Duration::from_millis(500)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        for odd in [
            "-1",
            "inf",
            "-inf",
            "NaN",
            "1e300",
            "301",
            "Wed, 21 Oct 2026 07:28:00 GMT",
            "",
        ] {
            assert_eq!(parse_retry_after(odd), None, "{odd}");
        }
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            error_message(r#"{"detail":{"status":"x","message":"Voice not found"}}"#),
            "Voice not found"
        );
        assert_eq!(error_message(r#"{"detail":"bad"}"#), "bad");
        assert_eq!(error_message("plain"), "plain");
    }
}
