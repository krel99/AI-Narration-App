//! The account's own voices: GET /v2/voices (needs the `voices_read`
//! permission), a single voice by id (GET /v1/voices/{id}) and the audio
//! behind a voice's preview.
//!
//! Real responses contain explicit nulls (`samples`, `settings`, `sharing`,
//! `description`, `preview_url`), so voices are read field by field: a null
//! or an odd type in one field never loses the voice, and a voice without an
//! id never loses the page.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use reqwest::Url;
use serde_json::Value;

use super::account::{AccountError, is_missing_permission};
use super::{ElevenLabsClient, error_message};

/// Page size of /v2/voices; 100 is the API's maximum.
pub const PAGE_SIZE: u32 = 100;
/// "non-default" is personal + workspace + community voices (the Voice
/// Library voices the user added to My Voices on the web). It leaves out
/// ElevenLabs' default (premade) voices.
pub const VOICE_TYPE: &str = "non-default";
/// Stop paging after this many pages, whatever `has_more` says.
const MAX_PAGES: usize = 100;
/// The largest preview file the app accepts (previews are 20–170 KB).
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceInfo {
    pub voice_id: String,
    pub name: String,
    /// `cloned`, `generated`, `professional`, `premade`, `famous`,
    /// `high_quality`, … Kept as text: the API may add values.
    pub category: Option<String>,
    /// Free-form labels: gender, age, accent, description, use_case, language…
    pub labels: BTreeMap<String, String>,
    pub description: Option<String>,
    /// A public MP3 (may be null, also on cloned voices).
    pub preview_url: Option<String>,
    pub verified_languages: Vec<VerifiedLanguage>,
    /// The first uploaded sample: the preview of last resort.
    pub sample: Option<VoiceSample>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerifiedLanguage {
    pub language: String,
    pub model_id: Option<String>,
    pub accent: Option<String>,
    pub locale: Option<String>,
    pub preview_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceSample {
    pub sample_id: String,
    /// The uploaded file's type; may be WAV rather than MP3.
    pub mime_type: Option<String>,
}

/// A non-empty, trimmed string field.
fn string(v: &Value, k: &str) -> Option<String> {
    v[k].as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl VoiceInfo {
    /// One voice of a list or lookup response; `None` without a `voice_id`.
    pub fn from_json(v: &Value) -> Option<Self> {
        let voice_id = string(v, "voice_id")?;
        let labels = v["labels"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, val)| {
                        let text = match val {
                            Value::String(s) => s.trim().to_string(),
                            Value::Number(n) => n.to_string(),
                            Value::Bool(b) => b.to_string(),
                            _ => return None,
                        };
                        (!text.is_empty()).then(|| (k.trim().to_lowercase(), text))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let verified_languages = v["verified_languages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                Some(VerifiedLanguage {
                    language: string(l, "language")?,
                    model_id: string(l, "model_id"),
                    accent: string(l, "accent"),
                    locale: string(l, "locale"),
                    preview_url: string(l, "preview_url"),
                })
            })
            .collect();
        let sample = v["samples"].as_array().into_iter().flatten().find_map(|s| {
            Some(VoiceSample {
                sample_id: string(s, "sample_id")?,
                mime_type: string(s, "mime_type"),
            })
        });
        Some(VoiceInfo {
            name: string(v, "name").unwrap_or_else(|| voice_id.clone()),
            voice_id,
            category: string(v, "category"),
            labels,
            description: string(v, "description"),
            preview_url: string(v, "preview_url"),
            verified_languages,
            sample,
        })
    }
}

/// One page of GET /v2/voices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoicesPage {
    pub voices: Vec<VoiceInfo>,
    pub has_more: bool,
    pub next_page_token: Option<String>,
    /// Only sent when asked for (the app does not ask).
    pub total_count: Option<u64>,
}

pub fn parse_page(v: &Value) -> VoicesPage {
    VoicesPage {
        voices: v["voices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(VoiceInfo::from_json)
            .collect(),
        has_more: v["has_more"].as_bool().unwrap_or(false),
        next_page_token: string(v, "next_page_token"),
        total_count: v["total_count"].as_u64(),
    }
}

/// The v2 API root for the client's v1 base (`…/v1` → `…/v2`).
fn v2_base(base: &str) -> String {
    match base.strip_suffix("/v1") {
        Some(root) => format!("{root}/v2"),
        None => format!("{base}/v2"),
    }
}

/// Voice and sample ids are plain identifiers; anything else is not sent
/// as a path segment.
pub fn plausible_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn transport(e: impl std::fmt::Display) -> AccountError {
    AccountError::Transport(e.to_string())
}

/// A network error without the URL (long, and shown inline), ending in
/// its root cause ("Connection refused").
fn network(e: reqwest::Error) -> AccountError {
    if e.is_timeout() {
        return AccountError::Transport("timed out".into());
    }
    let e = e.without_url();
    let mut msg = e.to_string();
    let mut cause = std::error::Error::source(&e);
    let mut root = None;
    while let Some(c) = cause {
        root = Some(c.to_string());
        cause = c.source();
    }
    if let Some(r) = root.filter(|r| !msg.contains(r.as_str())) {
        msg = format!("{msg}: {r}");
    }
    AccountError::Transport(msg)
}

/// Turn a non-success status into an error; `scope` names the permission
/// a 401/403 "missing permission" refers to.
fn check(status: u16, body: &[u8], scope: &'static str) -> Result<(), AccountError> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let text = String::from_utf8_lossy(body);
    if is_missing_permission(status, &text) {
        return Err(AccountError::MissingPermission(scope));
    }
    Err(AccountError::Rejected {
        status,
        message: error_message(&text),
    })
}

/// "No such voice": a 404, or a 400 whose `detail` says `voice_not_found`
/// (both have been recorded for unknown voice ids).
fn is_voice_not_found(status: u16, body: &[u8]) -> bool {
    if status == 404 {
        return true;
    }
    if status != 400 {
        return false;
    }
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    ["status", "code"]
        .iter()
        .any(|k| v["detail"][*k].as_str() == Some("voice_not_found"))
}

impl ElevenLabsClient {
    /// GET (with the API key if `keyed`): the status and the body, up to
    /// `MAX_AUDIO_BYTES`. The caller checks the status.
    async fn get_bytes(&self, url: Url, keyed: bool) -> Result<(u16, Vec<u8>), AccountError> {
        let mut req = self.http.get(url).timeout(Duration::from_secs(30));
        if keyed {
            req = req.header("xi-api-key", &self.api_key);
        }
        let resp = req.send().await.map_err(network)?;
        let status = resp.status().as_u16();
        if resp
            .content_length()
            .is_some_and(|n| n as usize > MAX_AUDIO_BYTES)
        {
            return Err(AccountError::Transport("response too large".into()));
        }
        let body = resp.bytes().await.map_err(network)?.to_vec();
        if body.len() > MAX_AUDIO_BYTES {
            return Err(AccountError::Transport("response too large".into()));
        }
        Ok((status, body))
    }

    /// Every voice in the user's account ("My Voices"), sorted by name.
    /// Follows `next_page_token` while `has_more` says so.
    pub async fn voices(&self) -> Result<Vec<VoiceInfo>, AccountError> {
        let base = v2_base(&self.base);
        let mut out: Vec<VoiceInfo> = Vec::new();
        let mut ids = HashSet::new();
        let mut tokens = HashSet::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = Url::parse(&format!("{base}/voices")).map_err(transport)?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("voice_type", VOICE_TYPE)
                    .append_pair("page_size", &PAGE_SIZE.to_string())
                    .append_pair("include_total_count", "false");
                if let Some(t) = &token {
                    q.append_pair("next_page_token", t);
                }
            }
            let (status, body) = self.get_bytes(url, true).await?;
            check(status, &body, "voices_read")?;
            let v: Value = serde_json::from_slice(&body).map_err(transport)?;
            let page = parse_page(&v);
            for voice in page.voices {
                // Voices added while paging can shift a page: keep the first.
                if ids.insert(voice.voice_id.clone()) {
                    out.push(voice);
                }
            }
            match page.next_page_token {
                Some(t) if page.has_more && tokens.insert(t.clone()) => token = Some(t),
                Some(_) if page.has_more => {
                    log::warn!("ElevenLabs repeated a voice page token; stopping");
                    break;
                }
                None if page.has_more => {
                    log::warn!("ElevenLabs said has_more without a page token; stopping");
                    break;
                }
                _ => break,
            }
        }
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.voice_id.cmp(&b.voice_id))
        });
        Ok(out)
    }

    /// GET /v1/voices/{id}: any voice the key can see, also default voices
    /// that are not in My Voices. `None` when there is no such voice.
    pub async fn voice(&self, voice_id: &str) -> Result<Option<VoiceInfo>, AccountError> {
        if !plausible_id(voice_id) {
            return Ok(None);
        }
        let url = Url::parse(&format!("{}/voices/{voice_id}", self.base)).map_err(transport)?;
        let (status, body) = self.get_bytes(url, true).await?;
        if is_voice_not_found(status, &body) {
            return Ok(None);
        }
        check(status, &body, "voices_read")?;
        let v: Value = serde_json::from_slice(&body).map_err(transport)?;
        Ok(VoiceInfo::from_json(&v))
    }

    /// A voice's public preview file. The URL points at public storage on
    /// another host, so the API key is never sent with it.
    pub async fn download_preview(&self, url: &str) -> Result<Vec<u8>, AccountError> {
        let url = Url::parse(url).map_err(transport)?;
        if !matches!(url.scheme(), "https" | "http") {
            return Err(AccountError::Transport("not an http(s) URL".into()));
        }
        let (status, body) = self.get_bytes(url, false).await?;
        check(status, &body, "voices_read")?;
        Ok(body)
    }

    /// GET /v1/voices/{voice_id}/samples/{sample_id}/audio: the file the
    /// user uploaded for the voice (its format comes from its mime type).
    pub async fn sample_audio(
        &self,
        voice_id: &str,
        sample_id: &str,
    ) -> Result<Vec<u8>, AccountError> {
        if !plausible_id(voice_id) || !plausible_id(sample_id) {
            return Err(AccountError::Transport("invalid voice or sample id".into()));
        }
        let url = Url::parse(&format!(
            "{}/voices/{voice_id}/samples/{sample_id}/audio",
            self.base
        ))
        .map_err(transport)?;
        let (status, body) = self.get_bytes(url, true).await?;
        check(status, &body, "voices_read")?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn voices_survive_nulls_and_odd_types() {
        let page = parse_page(&json!({
            "voices": [
                {
                    "voice_id": "abc123XYZ", "name": "Vale", "category": "cloned",
                    "labels": {"Gender": "female", "age": "young", "accent": "british", "weight": 3, "x": null, "empty": " "},
                    "description": "Warm and low.", "preview_url": "https://example.test/p.mp3",
                    "samples": [{"sample_id": null}, {"sample_id": "s1", "mime_type": "audio/wav"}],
                    "verified_languages": [
                        {"language": "cs", "model_id": "eleven_v4", "accent": "standard", "preview_url": "https://example.test/cs.mp3"},
                        {"model_id": "no language"}
                    ],
                    "settings": null, "sharing": null
                },
                {
                    "voice_id": "nulls0001", "name": null, "category": null, "labels": null,
                    "description": null, "preview_url": null, "samples": null,
                    "verified_languages": null, "high_quality_base_model_ids": null
                },
                {"name": "no id"},
                "garbage"
            ],
            "has_more": true, "next_page_token": "tok", "total_count": null
        }));
        assert_eq!(page.voices.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.next_page_token.as_deref(), Some("tok"));
        assert_eq!(page.total_count, None);

        let vale = &page.voices[0];
        assert_eq!(vale.name, "Vale");
        assert_eq!(vale.category.as_deref(), Some("cloned"));
        assert_eq!(
            vale.labels.get("gender").map(String::as_str),
            Some("female")
        );
        assert_eq!(vale.labels.get("weight").map(String::as_str), Some("3"));
        assert!(!vale.labels.contains_key("x") && !vale.labels.contains_key("empty"));
        assert_eq!(vale.verified_languages.len(), 1);
        assert_eq!(vale.verified_languages[0].language, "cs");
        assert_eq!(vale.sample.as_ref().unwrap().sample_id, "s1");

        let bare = &page.voices[1];
        assert_eq!(bare.name, "nulls0001", "a nameless voice shows its id");
        assert!(bare.labels.is_empty() && bare.sample.is_none() && bare.preview_url.is_none());

        assert_eq!(parse_page(&json!({"voices": null})), VoicesPage::default());
    }

    #[test]
    fn v2_root_and_ids() {
        assert_eq!(
            v2_base("https://api.elevenlabs.io/v1"),
            "https://api.elevenlabs.io/v2"
        );
        assert_eq!(v2_base("http://127.0.0.1:9"), "http://127.0.0.1:9/v2");
        assert!(plausible_id("9BWtsMINqrJLrRacOk9x"));
        assert!(!plausible_id("../user"));
        assert!(!plausible_id("a b"));
        assert!(!plausible_id(""));
    }
}
