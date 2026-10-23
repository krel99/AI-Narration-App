//! Speaker attribution through the Claude Messages API (raw HTTP; there is
//! no official Rust SDK). One request per chunk of paragraphs returns strict
//! JSON constrained by `output_config.format`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::lang::Language;
use crate::model::{Character, NARRATOR, SegmentKind, SegmentationSettings};
use crate::segment::ProposedSegment;

pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2026-10-21";
/// Server-side fallback: a request declined by a safety classifier is re-run on
/// the recommended fallback model instead of failing the chunk.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

#[derive(Debug)]
pub enum LlmError {
    /// Worth retrying after a pause (429, 5xx, overloaded, network).
    Transient(String),
    /// The response hit max_tokens; retry with a smaller chunk.
    TooLong,
    Fatal(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Transient(m) => write!(f, "temporary Claude API error: {m}"),
            LlmError::TooLong => write!(f, "response too long for one request"),
            LlmError::Fatal(m) => write!(f, "Claude API error: {m}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// A paragraph sent to the model.
#[derive(Debug, Clone)]
pub struct PromptParagraph {
    /// 1-based paragraph number within the chapter.
    pub number: usize,
    pub text: String,
    /// Attribute this paragraph (it contains dialogue). Others are context only.
    pub target: bool,
    /// For context paragraphs that were already attributed: their speakers.
    pub known_speakers: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NewCharacter {
    pub key: String,
    pub display_name: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Attribution {
    /// paragraph number → proposed segments.
    pub paragraphs: BTreeMap<usize, Vec<ProposedSegment>>,
    pub characters: Vec<NewCharacter>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

const SYSTEM_PROMPT: &str = "You attribute speech in prose for a multi-voice audiobook. \
For each target paragraph you split the text into segments and name who speaks each one.

Segmentation rules:
- Every quoted speech span is its own segment of kind \"dialogue\", including its quote marks.
- Every stretch of non-speech text between quotes is a segment of kind \"narration\" with speaker \"narrator\". \
Dialogue tags such as \"he said\" are narration.
- Copy the text exactly, character for character: same words, punctuation and quote characters. \
Never drop, add, reorder, translate or normalise anything. The segments of a paragraph, joined with single spaces, \
must reproduce that paragraph.
- Return segments only for paragraphs marked TARGET. Other paragraphs are context for working out who is speaking.

Speaker rules:
- Use the keys from the character registry when a speaker is already known, including when the text refers to \
them by an alias (for example \"the old man\").
- For a speaker not in the registry, invent a short lowercase key (letters, digits, underscores) and list them in \
\"characters\" with a display name and any aliases the text uses. Also list a known character there when the text \
gives them a new alias.
- If the speaker of a quote genuinely cannot be determined, use \"unknown\".";

fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["segments", "characters"],
        "properties": {
            "segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["paragraph", "kind", "speaker", "text"],
                    "properties": {
                        "paragraph": {"type": "integer"},
                        "kind": {"type": "string", "enum": ["narration", "dialogue"]},
                        "speaker": {"type": "string"},
                        "text": {"type": "string"}
                    }
                }
            },
            "characters": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["key", "display_name", "aliases"],
                    "properties": {
                        "key": {"type": "string"},
                        "display_name": {"type": "string"},
                        "aliases": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }
        }
    })
}

pub fn build_user_prompt(
    lang: &Language,
    registry: &[Character],
    paragraphs: &[PromptParagraph],
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Language: {} ({}). Speech is quoted with {}.\n\n",
        lang.name,
        lang.code,
        lang.quote_example()
    ));
    s.push_str("Character registry (key: display name; aliases):\n");
    s.push_str(&format!("- {NARRATOR}: the narrator\n"));
    for c in registry {
        if c.aliases.is_empty() {
            s.push_str(&format!("- {}: {}\n", c.key, c.display_name));
        } else {
            s.push_str(&format!(
                "- {}: {}; {}\n",
                c.key,
                c.display_name,
                c.aliases.join(", ")
            ));
        }
    }
    s.push_str("\nParagraphs:\n\n");
    for p in paragraphs {
        if p.target {
            s.push_str(&format!("[P{} TARGET]\n{}\n\n", p.number, p.text));
        } else if p.known_speakers.is_empty() {
            s.push_str(&format!("[P{} context]\n{}\n\n", p.number, p.text));
        } else {
            s.push_str(&format!(
                "[P{} context; speakers: {}]\n{}\n\n",
                p.number,
                p.known_speakers.join(", "),
                p.text
            ));
        }
    }
    let targets: Vec<String> = paragraphs
        .iter()
        .filter(|p| p.target)
        .map(|p| p.number.to_string())
        .collect();
    s.push_str(&format!(
        "Return segments for paragraphs {} only, using the paragraph number in \"paragraph\".",
        targets.join(", ")
    ));
    s
}

#[derive(Deserialize)]
struct RawSegment {
    paragraph: i64,
    kind: String,
    speaker: String,
    text: String,
}

#[derive(Deserialize)]
struct RawCharacter {
    key: String,
    display_name: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Deserialize)]
struct RawOutput {
    segments: Vec<RawSegment>,
    #[serde(default)]
    characters: Vec<RawCharacter>,
}

/// Lowercase key with underscores, as the registry stores it.
pub fn normalize_key(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

/// Proposed segments per paragraph number, plus registry additions.
pub type ParsedOutput = (BTreeMap<usize, Vec<ProposedSegment>>, Vec<NewCharacter>);

pub fn parse_output(text: &str) -> Result<ParsedOutput, String> {
    let raw: RawOutput =
        serde_json::from_str(text.trim()).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut paragraphs: BTreeMap<usize, Vec<ProposedSegment>> = BTreeMap::new();
    for s in raw.segments {
        let kind =
            SegmentKind::parse(&s.kind).ok_or_else(|| format!("unknown kind {:?}", s.kind))?;
        if s.paragraph < 1 {
            return Err(format!("bad paragraph number {}", s.paragraph));
        }
        paragraphs
            .entry(s.paragraph as usize)
            .or_default()
            .push(ProposedSegment {
                kind,
                speaker: normalize_key(&s.speaker),
                text: s.text,
            });
    }
    let characters = raw
        .characters
        .into_iter()
        .filter(|c| !normalize_key(&c.key).is_empty())
        .map(|c| NewCharacter {
            key: normalize_key(&c.key),
            display_name: c.display_name,
            aliases: c.aliases,
        })
        .collect();
    Ok((paragraphs, characters))
}

pub struct ClaudeClient {
    http: reqwest::Client,
    api_key: String,
    url: String,
}

impl ClaudeClient {
    pub fn new(http: reqwest::Client, api_key: String, url: &str) -> Self {
        Self {
            http,
            api_key,
            url: url.to_string(),
        }
    }

    pub async fn attribute(
        &self,
        settings: &SegmentationSettings,
        lang: &Language,
        registry: &[Character],
        paragraphs: &[PromptParagraph],
    ) -> Result<Attribution, LlmError> {
        let body = json!({
            "model": settings.model,
            "max_tokens": 32000,
            "fallbacks": "default",
            "output_config": {
                "effort": settings.effort,
                "format": {"type": "json_schema", "schema": output_schema()}
            },
            "system": SYSTEM_PROMPT,
            "messages": [{"role": "user", "content": build_user_prompt(lang, registry, paragraphs)}]
        });
        let resp = self
            .http
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(600))
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transient(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Transient(e.to_string()))?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or(text);
            let msg = format!("HTTP {}: {msg}", status.as_u16());
            return Err(
                if status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error() {
                    LlmError::Transient(msg)
                } else {
                    LlmError::Fatal(msg)
                },
            );
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Fatal(format!("bad response: {e}")))?;
        match v["stop_reason"].as_str() {
            Some("refusal") => {
                return Err(LlmError::Fatal(format!(
                    "the model declined this chunk ({})",
                    v["stop_details"]["category"]
                        .as_str()
                        .unwrap_or("no category")
                )));
            }
            Some("max_tokens") => return Err(LlmError::TooLong),
            _ => {}
        }
        let out: String = v["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        let (paragraphs, characters) = parse_output(&out).map_err(LlmError::Transient)?;
        Ok(Attribution {
            paragraphs,
            characters,
            input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::by_code;

    #[test]
    fn keys_are_normalized() {
        assert_eq!(normalize_key("Old Man"), "old_man");
        assert_eq!(normalize_key(" Vortram "), "vortram");
        assert_eq!(normalize_key("Dr. Who?"), "dr_who");
        assert_eq!(normalize_key("Šárka"), "šárka");
    }

    #[test]
    fn parses_strict_output() {
        let out = r#"{"segments":[
            {"paragraph":2,"kind":"dialogue","speaker":"Vortram","text":"\"It is,\""},
            {"paragraph":2,"kind":"narration","speaker":"narrator","text":"started Vortram,"}
        ],"characters":[{"key":"vortram","display_name":"Vortram","aliases":["the old man"]}]}"#;
        let (paras, chars) = parse_output(out).unwrap();
        assert_eq!(paras[&2].len(), 2);
        assert_eq!(paras[&2][0].speaker, "vortram");
        assert_eq!(chars[0].aliases, vec!["the old man"]);
        assert!(parse_output("{\"segments\":[{\"paragraph\":0,\"kind\":\"dialogue\",\"speaker\":\"a\",\"text\":\"b\"}]}").is_err());
        assert!(parse_output("not json").is_err());
    }

    #[test]
    fn prompt_marks_targets_and_registry() {
        let registry = vec![Character {
            id: 1,
            project_id: 1,
            key: "vortram".into(),
            display_name: "Vortram".into(),
            aliases: vec!["the old man".into()],
        }];
        let paras = vec![
            PromptParagraph {
                number: 1,
                text: "Dawn.".into(),
                target: false,
                known_speakers: vec![],
            },
            PromptParagraph {
                number: 2,
                text: "\"Hi,\" he said.".into(),
                target: true,
                known_speakers: vec![],
            },
        ];
        let p = build_user_prompt(by_code("en"), &registry, &paras);
        assert!(p.contains("- vortram: Vortram; the old man"));
        assert!(p.contains("[P1 context]"));
        assert!(p.contains("[P2 TARGET]"));
        assert!(p.contains("paragraphs 2 only"));
    }
}
