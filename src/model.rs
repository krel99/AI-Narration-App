//! Domain types. These mirror the SQLite tables in `db.rs`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type Id = i64;

/// Speaker key used for all narration segments (and dialogue tags by default).
pub const NARRATOR: &str = "narrator";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentKind {
    Narration,
    Dialogue,
}

impl SegmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentKind::Narration => "narration",
            SegmentKind::Dialogue => "dialogue",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "narration" => Some(SegmentKind::Narration),
            "dialogue" => Some(SegmentKind::Dialogue),
            _ => None,
        }
    }
}

/// draft → generated → approved | rejected. Rejected segments are re-rolled
/// with a new seed (which moves them back to generated); approved ones are locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentStatus {
    Draft,
    Generated,
    Approved,
    Rejected,
}

impl SegmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentStatus::Draft => "draft",
            SegmentStatus::Generated => "generated",
            SegmentStatus::Approved => "approved",
            SegmentStatus::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(SegmentStatus::Draft),
            "generated" => Some(SegmentStatus::Generated),
            "approved" => Some(SegmentStatus::Approved),
            "rejected" => Some(SegmentStatus::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChapterStatus {
    /// Text loaded and split into paragraphs; no segments yet.
    Imported,
    /// Segments exist (from the LLM pass or the parser fallback).
    Segmented,
    /// At least one export has been written.
    Exported,
}

impl ChapterStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChapterStatus::Imported => "imported",
            ChapterStatus::Segmented => "segmented",
            ChapterStatus::Exported => "exported",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "imported" => Some(ChapterStatus::Imported),
            "segmented" => Some(ChapterStatus::Segmented),
            "exported" => Some(ChapterStatus::Exported),
            _ => None,
        }
    }
}

/// ElevenLabs `voice_settings`. Part of the take hash, so changing any value
/// produces a new take instead of reusing a cached one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceSettings {
    pub stability: f32,
    pub similarity_boost: f32,
    pub style: f32,
    pub use_speaker_boost: bool,
    pub speed: f32,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            stability: 0.5,
            similarity_boost: 0.75,
            style: 0.0,
            use_speaker_boost: true,
            speed: 1.0,
        }
    }
}

/// Silence inserted between segments when assembling a chapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SilenceSettings {
    pub within_paragraph_ms: u32,
    pub between_paragraphs_ms: u32,
    pub scene_break_ms: u32,
    pub chapter_lead_in_ms: u32,
    pub chapter_tail_ms: u32,
}

impl Default for SilenceSettings {
    fn default() -> Self {
        Self {
            within_paragraph_ms: 0,
            between_paragraphs_ms: 700,
            scene_break_ms: 2000,
            chapter_lead_in_ms: 750,
            chapter_tail_ms: 2000,
        }
    }
}

/// Settings for the LLM attribution pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SegmentationSettings {
    pub model: String,
    /// `output_config.effort`: low | medium | high | xhigh | max.
    pub effort: String,
    /// Paragraphs attributed per request.
    pub chunk_paragraphs: usize,
    /// Preceding paragraphs sent as read-only context with each chunk.
    pub overlap_paragraphs: usize,
    /// Retries per paragraph when exact-text validation fails, before the
    /// deterministic parser takes over.
    pub max_retries: u32,
}

impl Default for SegmentationSettings {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            effort: "medium".into(),
            chunk_paragraphs: 30,
            overlap_paragraphs: 4,
            max_retries: 2,
        }
    }
}

/// How Claude inserts ElevenLabs audio tags (`[whispers]`, `[long pause]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioTagSettings {
    /// Add tags automatically after segmentation (tag-capable models only).
    pub enabled: bool,
    /// light | moderate | rich
    pub density: String,
    /// Free-form direction for the whole project ("keep the narrator calm").
    pub direction: String,
}

impl Default for AudioTagSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            density: "moderate".into(),
            direction: String::new(),
        }
    }
}

/// Project-level settings that are not first-class columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectSettings {
    /// ElevenLabs `output_format`, e.g. `pcm_44100` or the `mp3_44100_192` fallback.
    pub output_format: String,
    /// Send previous_request_ids for prosody continuity.
    pub request_stitching: bool,
    /// Concurrent TTS requests; set to the plan's concurrency limit.
    pub concurrency: usize,
    /// Pressing R (reject) immediately queues a re-roll.
    pub reroll_on_reject: bool,
    pub silence: SilenceSettings,
    pub segmentation: SegmentationSettings,
    pub audio_tags: AudioTagSettings,
    /// Book metadata, cover and delivery options for the retail export.
    pub publication: crate::retail::Publication,
    /// Silence trimming of takes (automatic trimming is off by default).
    pub trim: crate::trim::TrimSettings,
    /// Multi-speaker paragraphs as one Text-to-Dialogue request (on by default).
    pub dialogue: crate::dialogue::DialogueSettings,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            output_format: "pcm_44100".into(),
            request_stitching: true,
            concurrency: 3,
            reroll_on_reject: true,
            silence: SilenceSettings::default(),
            segmentation: SegmentationSettings::default(),
            audio_tags: AudioTagSettings::default(),
            publication: Default::default(),
            trim: crate::trim::TrimSettings::default(),
            dialogue: crate::dialogue::DialogueSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: Id,
    pub name: String,
    /// Language code from `lang::LANGUAGES`; decides quote styles and the default model.
    pub language: String,
    pub tts_model: String,
    /// speaker key → ElevenLabs voice_id. Must include `narrator`.
    pub voice_map: BTreeMap<String, String>,
    pub default_voice_settings: VoiceSettings,
    pub settings: ProjectSettings,
}

impl Project {
    pub fn voice_for(&self, speaker: &str) -> Option<&str> {
        self.voice_map
            .get(speaker)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
    }
}

/// Character registry entry. Kept across chunks so aliases ("the old man")
/// resolve to the same key.
#[derive(Debug, Clone, PartialEq)]
pub struct Character {
    pub id: Id,
    pub project_id: Id,
    pub key: String,
    pub display_name: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub id: Id,
    pub project_id: Id,
    pub order: i64,
    pub title: String,
    pub source_text: String,
    pub status: ChapterStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Paragraph {
    pub id: Id,
    pub chapter_id: Id,
    pub order: i64,
    /// Source text; the concatenated segment texts must reproduce it.
    pub text: String,
    /// A scene-break marker (`***`, `#`, …) preceded this paragraph.
    pub scene_break_before: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub id: Id,
    pub paragraph_id: Id,
    pub order: i64,
    pub kind: SegmentKind,
    pub speaker: String,
    pub text: String,
    pub settings_override: Option<VoiceSettings>,
    /// The text with audio tags inserted (`[whispers] Get down.`), sent to
    /// tag-capable models instead of `text`. Without its tags it equals `text`.
    pub performance: Option<String>,
    pub status: SegmentStatus,
    pub current_take_id: Option<Id>,
}

impl Segment {
    /// What the TTS model receives: the tagged performance when the model can
    /// perform tags, the plain text otherwise.
    pub fn spoken_text(&self, tts_model: &str) -> &str {
        match &self.performance {
            Some(p) if crate::lang::model_supports_tags(tts_model) => p,
            _ => &self.text,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Take {
    pub id: Id,
    pub segment_id: Id,
    pub seed: u32,
    pub request_id: Option<String>,
    /// Path relative to the library's audio directory.
    pub audio_path: String,
    pub hash: String,
    /// Credits charged for this take; 0 when it reused a cached take.
    pub credits_used: i64,
    /// `credits_used` came from ElevenLabs' `character-cost` header. When the
    /// header is missing it is an estimate (billed rates differ per plan).
    pub credits_exact: bool,
    pub created_at: String,
    /// Kept range `(start_ms, end_ms)` of the WAV; `None` = untrimmed. Metadata
    /// only: the file is never modified and the take hash does not change.
    pub trim_ms: Option<(u32, u32)>,
    /// Set when the take was cut from a multi-speaker dialogue request: the
    /// group hash shared by that paragraph's takes. `None` for single-voice TTS.
    pub dialogue_group: Option<String>,
}
