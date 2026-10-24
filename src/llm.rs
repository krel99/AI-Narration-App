//! Speaker attribution through the Claude Messages API (raw HTTP; there is
//! no official Rust SDK). One request per chunk of paragraphs returns strict
//! JSON constrained by `output_config.format`. The same requests can instead
//! go through the user's own Claude Code CLI (`claude_code.rs`), which uses
//! their Pro/Max subscription.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::lang::Language;
use crate::model::{AudioTagSettings, Character, NARRATOR, SegmentKind, SegmentationSettings};
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
    /// The subscription's usage limit is reached (message includes the reset
    /// time). Retrying before the reset is pointless.
    UsageLimit(String),
    /// Claude Code is not signed in, or its login expired.
    Auth(String),
    /// Claude Code cannot run at all (not installed, too old, no result).
    Unavailable(String),
}

impl LlmError {
    /// Every further request would fail the same way: stop the whole pass
    /// instead of trying (and falling back) chunk by chunk.
    pub fn stops_pass(&self) -> bool {
        matches!(
            self,
            LlmError::UsageLimit(_) | LlmError::Auth(_) | LlmError::Unavailable(_)
        )
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Transient(m) => write!(f, "temporary Claude API error: {m}"),
            LlmError::TooLong => write!(f, "response too long for one request"),
            LlmError::Fatal(m) => write!(f, "Claude API error: {m}"),
            LlmError::UsageLimit(m) | LlmError::Auth(m) | LlmError::Unavailable(m) => {
                write!(f, "{m}")
            }
        }
    }
}

/// Tokens and cost of one model's share of one call, as recorded in the
/// `llm_usage` table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// API-equivalent list price. Claude Code reports it; for the API it is
    /// computed from `costs::claude_cost_usd`. None for an unpriced model.
    pub cost_usd: Option<f64>,
}

impl CallUsage {
    /// Every prompt token, cached or not.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Totals of one call, for the progress messages.
#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    input: u64,
    output: u64,
    cost: Option<f64>,
}

fn totals(usage: &[CallUsage]) -> Totals {
    let mut t = Totals::default();
    for u in usage {
        t.input += u.prompt_tokens();
        t.output += u.output_tokens;
        if let Some(c) = u.cost_usd {
            t.cost = Some(t.cost.unwrap_or(0.0) + c);
        }
    }
    t
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
    /// API-equivalent cost of the call, when known.
    pub cost_usd: Option<f64>,
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

/// Where requests go.
pub enum Transport {
    /// The Messages API with an API key (billed per token).
    Api { url: String, key: String },
    /// The user's Claude Code CLI, signed in with their subscription.
    ClaudeCode(crate::claude_code::Invocation),
}

pub struct ClaudeClient {
    http: reqwest::Client,
    transport: Transport,
    /// Usage of every call made so far, failed ones included (a failed call
    /// can still cost tokens). Drained by `take_usage`.
    ledger: Mutex<Vec<CallUsage>>,
}

impl ClaudeClient {
    pub fn new(http: reqwest::Client, api_key: String, url: &str) -> Self {
        Self::with_transport(
            http,
            Transport::Api {
                url: url.to_string(),
                key: api_key,
            },
        )
    }

    pub fn with_transport(http: reqwest::Client, transport: Transport) -> Self {
        Self {
            http,
            transport,
            ledger: Mutex::new(Vec::new()),
        }
    }

    /// `api` or `claude_code`, as stored in `llm_usage.backend`.
    pub fn backend(&self) -> &'static str {
        match self.transport {
            Transport::Api { .. } => "api",
            Transport::ClaudeCode(_) => "claude_code",
        }
    }

    /// Usage recorded since the last call to this.
    pub fn take_usage(&self) -> Vec<CallUsage> {
        self.ledger
            .lock()
            .map(|mut l| std::mem::take(&mut *l))
            .unwrap_or_default()
    }

    fn record(&self, usage: &[CallUsage]) {
        if let Ok(mut l) = self.ledger.lock() {
            l.extend_from_slice(usage);
        }
    }

    /// One JSON-schema-constrained request through the configured transport.
    async fn call(
        &self,
        settings: &SegmentationSettings,
        system: &str,
        user: String,
        schema: Value,
    ) -> Result<(String, Totals), LlmError> {
        let (result, usage) = match &self.transport {
            Transport::Api { url, key } => {
                self.call_api(url, key, settings, system, user, schema)
                    .await
            }
            Transport::ClaudeCode(inv) => {
                let req = crate::claude_code::StructuredRequest {
                    model: &settings.model,
                    effort: &settings.effort,
                    system,
                    schema: &schema,
                    prompt: user,
                };
                let (result, usage) = inv.run(&req).await;
                (result.map(|v| v.to_string()), usage)
            }
        };
        self.record(&usage);
        result.map(|text| (text, totals(&usage)))
    }

    /// One Messages API call. Returns the answer text, plus the usage the
    /// response reported (also on refusals and max_tokens stops).
    async fn call_api(
        &self,
        url: &str,
        key: &str,
        settings: &SegmentationSettings,
        system: &str,
        user: String,
        schema: Value,
    ) -> (Result<String, LlmError>, Vec<CallUsage>) {
        let body = json!({
            "model": settings.model,
            "max_tokens": 32000,
            "fallbacks": "default",
            "output_config": {
                "effort": settings.effort,
                "format": {"type": "json_schema", "schema": schema}
            },
            "system": system,
            "messages": [{"role": "user", "content": user}]
        });
        let resp = match self
            .http
            .post(url)
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(600))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return (Err(LlmError::Transient(e.to_string())), vec![]),
        };
        let status = resp.status();
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => return (Err(LlmError::Transient(e.to_string())), vec![]),
        };
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or(text);
            let msg = format!("HTTP {}: {msg}", status.as_u16());
            let err =
                if status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error() {
                    LlmError::Transient(msg)
                } else {
                    LlmError::Fatal(msg)
                };
            return (Err(err), vec![]);
        }
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => return (Err(LlmError::Fatal(format!("bad response: {e}"))), vec![]),
        };
        let usage = api_usage(&v, &settings.model);
        match v["stop_reason"].as_str() {
            Some("refusal") => {
                let err = LlmError::Fatal(format!(
                    "the model declined this chunk ({})",
                    v["stop_details"]["category"]
                        .as_str()
                        .unwrap_or("no category")
                ));
                return (Err(err), usage);
            }
            Some("max_tokens") => return (Err(LlmError::TooLong), usage),
            _ => {}
        }
        let out: String = v["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        (Ok(out), usage)
    }

    pub async fn attribute(
        &self,
        settings: &SegmentationSettings,
        lang: &Language,
        registry: &[Character],
        paragraphs: &[PromptParagraph],
    ) -> Result<Attribution, LlmError> {
        let user = build_user_prompt(lang, registry, paragraphs);
        let (out, t) = self
            .call(settings, SYSTEM_PROMPT, user, output_schema())
            .await?;
        let (paragraphs, characters) = parse_output(&out).map_err(LlmError::Transient)?;
        Ok(Attribution {
            paragraphs,
            characters,
            input_tokens: t.input,
            output_tokens: t.output,
            cost_usd: t.cost,
        })
    }

    /// Ask for audio tags on `segments`. Returns segment id → tagged text, for
    /// the segments the model chose to tag.
    pub async fn direct(
        &self,
        settings: &SegmentationSettings,
        tags: &AudioTagSettings,
        lang: &Language,
        context: &[String],
        segments: &[DirectionSegment],
    ) -> Result<Direction, LlmError> {
        let user = build_direction_prompt(lang, tags, context, segments);
        let (out, t) = self
            .call(settings, DIRECTION_PROMPT, user, direction_schema())
            .await?;
        let performances = parse_direction(&out).map_err(LlmError::Transient)?;
        Ok(Direction {
            performances,
            input_tokens: t.input,
            output_tokens: t.output,
            cost_usd: t.cost,
        })
    }
}

/// A segment offered to the direction pass.
#[derive(Debug, Clone)]
pub struct DirectionSegment {
    pub id: i64,
    pub paragraph: usize,
    pub kind: SegmentKind,
    pub speaker: String,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Direction {
    pub performances: BTreeMap<i64, String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
}

/// Usage of a Messages API response, priced from the list-price table.
fn api_usage(v: &Value, requested_model: &str) -> Vec<CallUsage> {
    let u = &v["usage"];
    if !u.is_object() {
        return vec![];
    }
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let model = v["model"].as_str().unwrap_or(requested_model).to_string();
    let (input, output) = (n("input_tokens"), n("output_tokens"));
    let (cache_read, cache_write) = (
        n("cache_read_input_tokens"),
        n("cache_creation_input_tokens"),
    );
    vec![CallUsage {
        cost_usd: crate::costs::claude_cost_usd(&model, input, output, cache_read, cache_write),
        model,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }]
}

/// Characters of the fixed part of each request (system prompt + schema),
/// for the token estimate: (attribution, audio tags).
pub fn prompt_overhead_chars() -> (usize, usize) {
    (
        SYSTEM_PROMPT.len() + output_schema().to_string().len(),
        DIRECTION_PROMPT.len() + direction_schema().to_string().len(),
    )
}

const DIRECTION_PROMPT: &str = "You direct the voice performance of an audiobook read by ElevenLabs \
text-to-speech (Eleven v3/v4). You do this by inserting audio tags: short cues in square brackets that \
the voice performs instead of reading aloud.

Useful tags (you may also combine qualities, e.g. [whispering, fearful]):
- emotion and attitude: [thoughtful], [sad], [angry], [excited], [nervous], [curious], [sarcastic], \
[resigned], [serious], [warmly], [coldly], [cheerfully], [tense]
- voice: [whispers], [softly], [quietly], [shouting], [loudly]
- pacing: [short pause], [pause], [long pause], [slowly], [rushed]
- reactions: [sighs], [laughs], [chuckles], [gasps], [clears throat], [exhales], [gulps]

Rules:
- Only insert tags. Every word, punctuation mark and quote character of the segment must stay exactly as \
given and in order. Never add, remove, translate or correct anything.
- A tag shapes the text after it, so put it before the words it applies to. Never end a segment with a tag.
- Tag narration as well as dialogue: narration tags shape the narrator's pacing and mood (a [long pause] \
before a dramatic beat, [thoughtful] before reflection); dialogue tags shape the character's delivery.
- Only voice performance: no sound effects, music or stage directions.
- Fit the tags to the scene and the requested density, and vary them; a repeated tag loses its effect.
- Return only the segments you add at least one tag to, with the full segment text including your tags.";

fn direction_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["segments"],
        "properties": {
            "segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "performance"],
                    "properties": {
                        "id": {"type": "integer"},
                        "performance": {"type": "string"}
                    }
                }
            }
        }
    })
}

fn density_hint(density: &str) -> &'static str {
    match density {
        "light" => {
            "light: only where the text clearly calls for it (a whisper, a shout, a dramatic pause); most segments get no tag"
        }
        "rich" => {
            "rich: an expressive performance, with cues on most dialogue and pacing pauses in the narration"
        }
        _ => {
            "moderate: mark emotional shifts and dramatic beats, roughly one tag every few sentences"
        }
    }
}

pub fn build_direction_prompt(
    lang: &Language,
    tags: &AudioTagSettings,
    context: &[String],
    segments: &[DirectionSegment],
) -> String {
    let mut s = format!(
        "Language: {} ({}). Write tags in English.\nDensity: {}.\n",
        lang.name,
        lang.code,
        density_hint(&tags.density)
    );
    if !tags.direction.trim().is_empty() {
        s.push_str(&format!("Director's notes: {}\n", tags.direction.trim()));
    }
    if !context.is_empty() {
        s.push_str("\nPreceding text, for context only:\n");
        for c in context {
            s.push_str(c);
            s.push_str("\n\n");
        }
    }
    s.push_str("\nSegments (id | paragraph | speaker):\n");
    for seg in segments {
        let who = match seg.kind {
            SegmentKind::Narration => seg.speaker.clone(),
            SegmentKind::Dialogue => format!("{}, dialogue", seg.speaker),
        };
        s.push_str(&format!(
            "[S{} | P{} | {}] {}\n",
            seg.id, seg.paragraph, who, seg.text
        ));
    }
    s
}

#[derive(Deserialize)]
struct RawPerformance {
    id: i64,
    performance: String,
}

#[derive(Deserialize)]
struct RawDirection {
    segments: Vec<RawPerformance>,
}

pub fn parse_direction(text: &str) -> Result<BTreeMap<i64, String>, String> {
    let raw: RawDirection =
        serde_json::from_str(text.trim()).map_err(|e| format!("invalid JSON: {e}"))?;
    Ok(raw
        .segments
        .into_iter()
        .map(|p| (p.id, p.performance))
        .collect())
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
    fn direction_prompt_and_output() {
        let segs = vec![DirectionSegment {
            id: 42,
            paragraph: 3,
            kind: SegmentKind::Dialogue,
            speaker: "vale".into(),
            text: "\"Hold position.\"".into(),
        }];
        let tags = AudioTagSettings {
            direction: "Military sci-fi; the narrator stays calm.".into(),
            ..Default::default()
        };
        let p = build_direction_prompt(by_code("en"), &tags, &["Earlier.".into()], &segs);
        assert!(p.contains("[S42 | P3 | vale, dialogue] \"Hold position.\""));
        assert!(p.contains("Director's notes: Military sci-fi"));
        assert!(p.contains("Density: moderate"));
        let out = parse_direction(
            r#"{"segments":[{"id":42,"performance":"\"[tense] Hold position.\""}]}"#,
        )
        .unwrap();
        assert_eq!(out[&42], "\"[tense] Hold position.\"");
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
