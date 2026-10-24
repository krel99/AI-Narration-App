//! `POST /v1/text-to-dialogue/with-timestamps`: one JSON answer with the
//! audio (base64) and the voice segments that say where each input is.
//!
//! The response types follow the official generated SDK types: every field
//! has a default and the indices are `i64`, so an unexpected value becomes a
//! validation failure in [`super::cut`] instead of an unreadable response.

use std::time::Duration;

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Input, Settings};
use crate::tts::TtsError;

pub const PATH: &str = "/text-to-dialogue/with-timestamps";

/// The SDKs' default timeout for dialogue requests.
const TIMEOUT: Duration = Duration::from_secs(240);

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    #[serde(skip)]
    pub output_format: String,
    pub inputs: Vec<Input>,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_code: Option<String>,
    pub settings: Settings,
    pub seed: u32,
    /// The neighbouring paragraphs' text (at most 100 characters each).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub future_text: Option<String>,
    /// Earlier dialogue requests of this chapter, closest last (at most 3).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub previous_request_ids: Vec<String>,
}

impl Request {
    /// Whether the request carries any continuity field ("not supported by
    /// every model").
    pub fn has_stitching(&self) -> bool {
        self.previous_text.is_some()
            || self.future_text.is_some()
            || !self.previous_request_ids.is_empty()
    }

    pub fn drop_stitching(&mut self) {
        self.previous_text = None;
        self.future_text = None;
        self.previous_request_ids.clear();
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Response {
    #[serde(default)]
    audio_base64: String,
    #[serde(default)]
    alignment: Option<Alignment>,
    #[serde(default)]
    voice_segments: Vec<VoiceSegment>,
}

/// Character timings of the original text: `characters` is every input's
/// text concatenated with no separator.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Alignment {
    #[serde(default)]
    pub characters: Vec<String>,
    #[serde(default)]
    pub character_start_times_seconds: Vec<f64>,
    #[serde(default)]
    pub character_end_times_seconds: Vec<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VoiceSegment {
    #[serde(default)]
    pub voice_id: String,
    #[serde(default)]
    pub start_time_seconds: f64,
    #[serde(default)]
    pub end_time_seconds: f64,
    /// Into `alignment.characters`.
    #[serde(default)]
    pub character_start_index: i64,
    /// Exclusive.
    #[serde(default)]
    pub character_end_index: i64,
    /// The input this segment belongs to. Cuts are keyed on this, never on
    /// the voice id (one voice can have several inputs).
    #[serde(default)]
    pub dialogue_input_index: i64,
}

#[derive(Debug)]
pub struct Reply {
    /// Decoded audio in the requested output format.
    pub audio: Vec<u8>,
    pub voice_segments: Vec<VoiceSegment>,
    pub alignment: Option<Alignment>,
    pub request_id: Option<String>,
    /// Credits billed, from the `character-cost` (or `x-character-count`) header.
    pub character_cost: Option<i64>,
}

#[derive(Debug)]
pub enum CallError {
    /// The request failed (HTTP status or network), as for text-to-speech.
    Api(TtsError),
    /// ElevenLabs answered 200 but the answer cannot be used (billed: the
    /// `character-cost` header, when it was sent).
    Unusable {
        message: String,
        character_cost: Option<i64>,
    },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Api(e) => write!(f, "{e}"),
            CallError::Unusable { message, .. } => {
                write!(f, "unusable dialogue response: {message}")
            }
        }
    }
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

/// A billed amount from a header: a non-negative number, possibly fractional.
fn parse_cost(s: &str) -> Option<i64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|c| c.is_finite() && *c >= 0.0)
        .map(|c| c.round() as i64)
}

/// Base64 as the API sends it; padding and whitespace are not insisted on.
pub fn decode_audio(b64: &str) -> Result<Vec<u8>, String> {
    let engine = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    engine
        .decode(clean.as_bytes())
        .map_err(|e| format!("audio_base64: {e}"))
}

/// Parse a 200 answer's body.
pub fn parse_reply(
    body: &str,
    request_id: Option<String>,
    character_cost: Option<i64>,
) -> Result<Reply, String> {
    let r: Response =
        serde_json::from_str(body).map_err(|e| format!("not the expected JSON: {e}"))?;
    let audio = decode_audio(&r.audio_base64)?;
    if audio.is_empty() {
        return Err("no audio".into());
    }
    Ok(Reply {
        audio,
        voice_segments: r.voice_segments,
        alignment: r.alignment,
        request_id,
        character_cost,
    })
}

/// Send one dialogue request. `base` is the API base (`…/v1`).
pub async fn convert_with_timestamps(
    http: &reqwest::Client,
    api_key: &str,
    base: &str,
    req: &Request,
) -> Result<Reply, CallError> {
    // The format is a plain identifier (letters, digits, underscores).
    let url = format!(
        "{}{PATH}?output_format={}",
        base.trim_end_matches('/'),
        req.output_format
    );
    let resp = http
        .post(url)
        .header("xi-api-key", api_key)
        .timeout(TIMEOUT)
        .json(req)
        .send()
        .await
        .map_err(|e| CallError::Api(TtsError::Transient(e.to_string())))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let body = resp
        .text()
        .await
        .map_err(|e| CallError::Api(TtsError::Transient(e.to_string())))?;
    if status.as_u16() == 429 {
        let retry_after = header("retry-after").and_then(|s| crate::tts::parse_retry_after(&s));
        return Err(CallError::Api(TtsError::RateLimited {
            retry_after,
            message: error_message(&body),
        }));
    }
    if status.is_server_error() {
        return Err(CallError::Api(TtsError::Transient(format!(
            "HTTP {}: {}",
            status.as_u16(),
            error_message(&body)
        ))));
    }
    if !status.is_success() {
        return Err(CallError::Api(TtsError::Rejected {
            status: status.as_u16(),
            message: error_message(&body),
        }));
    }
    let request_id = header("request-id");
    let character_cost = header("character-cost")
        .and_then(|s| parse_cost(&s))
        .or_else(|| header("x-character-count").and_then(|s| parse_cost(&s)));
    parse_reply(&body, request_id, character_cost).map_err(|message| CallError::Unusable {
        message,
        character_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_shape() {
        let mut req = Request {
            output_format: "pcm_24000".into(),
            inputs: vec![
                Input {
                    text: "[giggling] Knock knock".into(),
                    voice_id: "A".into(),
                },
                Input {
                    text: "Who is there?".into(),
                    voice_id: "B".into(),
                },
            ],
            model_id: "eleven_v4".into(),
            language_code: Some("en".into()),
            settings: Settings {
                stability: 0.5,
                similarity: Some(0.75),
            },
            seed: 1,
            previous_text: Some("Before.".into()),
            future_text: None,
            previous_request_ids: vec![],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "inputs": [
                    {"text": "[giggling] Knock knock", "voice_id": "A"},
                    {"text": "Who is there?", "voice_id": "B"}
                ],
                "model_id": "eleven_v4",
                "language_code": "en",
                "settings": {"stability": 0.5, "similarity": 0.75},
                "seed": 1,
                "previous_text": "Before."
            })
        );
        assert!(req.has_stitching());
        req.previous_request_ids.push("r1".into());
        req.drop_stitching();
        assert!(!req.has_stitching());
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("previous_text").is_none() && v.get("previous_request_ids").is_none());
    }

    #[test]
    fn replies_parse_leniently() {
        let body = serde_json::json!({
            "audio_base64": "AAEC\nAwQ=",
            "alignment": {"characters": ["H", "i"], "character_start_times_seconds": [0.0, 0.1],
                          "character_end_times_seconds": [0.1, 0.2]},
            "normalized_alignment": null,
            "voice_segments": [{"voice_id": "A", "start_time_seconds": 0, "end_time_seconds": 0.2,
                                "character_start_index": 0, "character_end_index": 2,
                                "dialogue_input_index": 0, "extra": true}]
        })
        .to_string();
        let r = parse_reply(&body, Some("req".into()), Some(2)).unwrap();
        assert_eq!(r.audio, vec![0, 1, 2, 3, 4]);
        assert_eq!(r.voice_segments[0].character_end_index, 2);
        assert_eq!(r.alignment.unwrap().characters, vec!["H", "i"]);
        // Unpadded base64 and missing optional fields.
        let r = parse_reply(r#"{"audio_base64":"AAEC"}"#, None, None).unwrap();
        assert_eq!(r.audio, vec![0, 1, 2]);
        assert!(r.voice_segments.is_empty() && r.alignment.is_none());
        // Unusable answers.
        assert!(parse_reply("<html>", None, None).is_err());
        assert!(parse_reply(r#"{"audio_base64":""}"#, None, None).is_err());
        assert!(parse_reply(r#"{"audio_base64":"!!!"}"#, None, None).is_err());
        // A negative index is still readable (and fails validation later).
        let r = parse_reply(
            r#"{"audio_base64":"AA==","voice_segments":[{"dialogue_input_index":-1}]}"#,
            None,
            None,
        )
        .unwrap();
        assert_eq!(r.voice_segments[0].dialogue_input_index, -1);
    }

    #[test]
    fn costs_and_errors() {
        assert_eq!(parse_cost("47"), Some(47));
        assert_eq!(parse_cost(" 46.6 "), Some(47));
        assert_eq!(parse_cost("-1"), None);
        assert_eq!(parse_cost("NaN"), None);
        assert_eq!(parse_cost("x"), None);
        assert_eq!(
            error_message(r#"{"detail":{"message":"Too many voices"}}"#),
            "Too many voices"
        );
    }
}
