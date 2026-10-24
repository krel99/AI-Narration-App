//! Multi-speaker paragraphs through ElevenLabs Text-to-Dialogue.
//!
//! A paragraph that goes back and forth between speakers can be generated as
//! one dialogue request instead of one text-to-speech call per segment, which
//! gives more natural turn-taking. The dialogue audio is cut back into one
//! take per segment in the pauses between the speakers, so per-segment review
//! still works.
//!
//! This module holds the pure parts: which paragraphs qualify, what is sent,
//! the hashes, the credit split and the context text. [`cut`] validates the
//! response and finds the cut points, [`client`] makes the HTTP call. The
//! worker side is `worker/dialogue.rs`.

pub mod client;
pub mod cut;

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::audio::Pcm;
use crate::lang;
use crate::model::*;
use crate::trim::{self, TrimSettings};

/// ElevenLabs' hard limit on distinct voices in one dialogue request.
pub const MAX_VOICES: usize = 10;
/// The documented total across all inputs "for reliable generation".
pub const MAX_CHARS_LIMIT: usize = 2000;
/// `previous_text` / `future_text` may hold at most 100 characters.
pub const CONTEXT_CHARS: usize = 100;
/// `previous_request_ids` holds at most 3 ids.
pub const MAX_PREVIOUS_REQUESTS: usize = 3;

/// Per-project dialogue settings (`ProjectSettings.dialogue`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DialogueSettings {
    /// Generate qualifying paragraphs as one dialogue request.
    pub enabled: bool,
    /// Largest paragraph (spoken characters, tags included) sent as one request.
    pub max_chars: usize,
}

impl Default for DialogueSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chars: 1800,
        }
    }
}

impl DialogueSettings {
    /// The character budget in use: never above the documented 2,000.
    pub fn budget(&self) -> usize {
        self.max_chars.clamp(1, MAX_CHARS_LIMIT)
    }
}

/// Why a paragraph keeps per-segment requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Disabled,
    Model,
    Reroll,
    /// Not every segment of the paragraph is in this request.
    Partial,
    /// A segment is not a draft, or already has a current take.
    NotDraft,
    Unassigned,
    /// A segment overrides the voice settings; dialogue has no per-input settings.
    OwnSettings,
    /// The project's speed or style is not the default; dialogue cannot apply them.
    VoiceSettings,
    OneVoice,
    TooManyVoices,
    TooLong,
    /// `[interrupting]` is known to collapse the dialogue's timings.
    Interrupting,
}

impl Skip {
    pub fn reason(self) -> &'static str {
        match self {
            Skip::Disabled => "dialogue generation is off",
            Skip::Model => "the model has no dialogue endpoint",
            Skip::Reroll => "re-rolls use per-segment requests",
            Skip::Partial => "not every segment of the paragraph was requested",
            Skip::NotDraft => "a segment already has audio",
            Skip::Unassigned => "a speaker has no voice",
            Skip::OwnSettings => "a segment has its own voice settings",
            Skip::VoiceSettings => "the voice speed or style is not the default",
            Skip::OneVoice => "only one voice",
            Skip::TooManyVoices => "more than 10 voices",
            Skip::TooLong => "longer than the dialogue character budget",
            Skip::Interrupting => "an [interrupting] tag, which breaks the dialogue's timings",
        }
    }
}

fn has_interrupting_tag(spoken: &str) -> bool {
    tags_in(spoken).any(|t| t.to_lowercase().contains("interrupt"))
}

fn tags_in(s: &str) -> impl Iterator<Item = &str> {
    crate::tags::find(s).into_iter().map(move |(a, b)| &s[a..b])
}

/// Whether the paragraph `para` (all its segments, in order) goes through
/// dialogue in a request for `requested`. Only first generations of whole
/// draft paragraphs with 2–10 voices that fit the budget qualify.
pub fn qualifies(
    project: &Project,
    para: &[&Segment],
    requested: &HashSet<Id>,
    reroll: bool,
) -> Result<(), Skip> {
    let settings = &project.settings.dialogue;
    let model = project.tts_model.as_str();
    if !settings.enabled {
        return Err(Skip::Disabled);
    }
    if !lang::model_supports_dialogue(model) {
        return Err(Skip::Model);
    }
    if reroll {
        return Err(Skip::Reroll);
    }
    if para.is_empty() || !para.iter().all(|s| requested.contains(&s.id)) {
        return Err(Skip::Partial);
    }
    if !para
        .iter()
        .all(|s| s.status == SegmentStatus::Draft && s.current_take_id.is_none())
    {
        return Err(Skip::NotDraft);
    }
    let mut voices: Vec<&str> = Vec::new();
    for s in para {
        let Some(v) = project.voice_for(&s.speaker) else {
            return Err(Skip::Unassigned);
        };
        // Counted as sent (`inputs` trims the id).
        let v = v.trim();
        if !voices.contains(&v) {
            voices.push(v);
        }
    }
    if voices.len() < 2 {
        return Err(Skip::OneVoice);
    }
    if voices.len() > MAX_VOICES {
        return Err(Skip::TooManyVoices);
    }
    let defaults = &project.default_voice_settings;
    if para
        .iter()
        .any(|s| s.settings_override.as_ref().is_some_and(|o| o != defaults))
    {
        return Err(Skip::OwnSettings);
    }
    if (defaults.speed - 1.0).abs() > 1e-3 || defaults.style.abs() > 1e-3 {
        return Err(Skip::VoiceSettings);
    }
    let chars: usize = para
        .iter()
        .map(|s| s.spoken_text(model).chars().count())
        .sum();
    if chars > settings.budget() {
        return Err(Skip::TooLong);
    }
    if para
        .iter()
        .any(|s| has_interrupting_tag(s.spoken_text(model)))
    {
        return Err(Skip::Interrupting);
    }
    Ok(())
}

/// `segments` (in reading order) grouped into paragraphs.
pub fn paragraphs(segments: &[Segment]) -> Vec<Vec<&Segment>> {
    let mut out: Vec<Vec<&Segment>> = Vec::new();
    for s in segments {
        match out.last_mut() {
            Some(p) if p[0].paragraph_id == s.paragraph_id => p.push(s),
            _ => out.push(vec![s]),
        }
    }
    out
}

/// The paragraphs of one chapter's `segments` that a request for `requested`
/// sends through dialogue: their segment ids, in reading order.
pub fn plan(
    project: &Project,
    segments: &[Segment],
    requested: &HashSet<Id>,
    reroll: bool,
) -> Vec<Vec<Id>> {
    paragraphs(segments)
        .into_iter()
        .filter(|p| p.iter().any(|s| requested.contains(&s.id)))
        .filter(|p| qualifies(project, p, requested, reroll).is_ok())
        .map(|p| p.iter().map(|s| s.id).collect())
        .collect()
}

/// One line of the dialogue: `inputs[i]` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Input {
    pub text: String,
    pub voice_id: String,
}

/// The paragraph's inputs, one per segment, in order (`None` if a voice is missing).
pub fn inputs(project: &Project, para: &[&Segment]) -> Option<Vec<Input>> {
    para.iter()
        .map(|s| {
            Some(Input {
                text: s.spoken_text(&project.tts_model).to_string(),
                voice_id: project.voice_for(&s.speaker)?.trim().to_string(),
            })
        })
        .collect()
}

/// The request's `settings` object. Dialogue takes only stability and
/// similarity, for the whole request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settings {
    pub stability: f32,
    /// Not sent to Eleven v3, which supports only stability for dialogue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
}

/// Eleven v3 accepts only 0 (creative), 0.5 (natural) and 1 (robust); other
/// values are reported to fail the request.
pub fn snap_v3_stability(x: f32) -> f32 {
    if !x.is_finite() {
        return 0.5;
    }
    if x < 0.25 {
        0.0
    } else if x > 0.75 {
        1.0
    } else {
        0.5
    }
}

pub fn settings_for(model: &str, vs: &VoiceSettings) -> Settings {
    if model.starts_with("eleven_v3") {
        Settings {
            stability: snap_v3_stability(vs.stability),
            similarity: None,
        }
    } else {
        Settings {
            stability: vs.stability.clamp(0.0, 1.0),
            similarity: Some(vs.similarity_boost.clamp(0.0, 1.0)),
        }
    }
}

#[derive(Serialize)]
struct GroupInputs<'a> {
    api: &'static str,
    inputs: &'a [Input],
    settings: &'a Settings,
    model: &'a str,
    seed: u32,
    output_format: &'a str,
}

/// Hash of one dialogue request: every input's text and voice, the settings,
/// model, seed and output format. Stored on each take as `dialogue_group`.
pub fn group_hash(
    inputs: &[Input],
    settings: &Settings,
    model: &str,
    seed: u32,
    output_format: &str,
) -> String {
    let g = GroupInputs {
        api: "text-to-dialogue",
        inputs,
        settings,
        model,
        seed,
        output_format,
    };
    hex::encode(Sha256::digest(
        serde_json::to_vec(&g).expect("dialogue inputs serialize"),
    ))
}

/// Take hash of input `index` of a dialogue: identical paragraphs reuse
/// their takes, and it never collides with a single-voice take hash.
pub fn take_hash(group: &str, index: usize) -> String {
    let json = serde_json::json!({ "dialogue_group": group, "input": index });
    hex::encode(Sha256::digest(json.to_string().as_bytes()))
}

/// Split `total` credits over the parts in proportion to `weights` (their
/// characters). The parts always sum to `total` exactly: the remainder goes
/// to the largest fractional shares (earlier parts on ties).
pub fn split_credits(total: i64, weights: &[usize]) -> Vec<i64> {
    let total = total.max(0) as i128;
    if weights.is_empty() {
        return vec![];
    }
    let sum: i128 = weights.iter().map(|&w| w as i128).sum();
    let weights: Vec<i128> = if sum == 0 {
        vec![1; weights.len()]
    } else {
        weights.iter().map(|&w| w as i128).collect()
    };
    let sum: i128 = weights.iter().sum();
    let mut parts: Vec<i128> = weights.iter().map(|w| total * w / sum).collect();
    let mut left = total - parts.iter().sum::<i128>();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(total * weights[i] % sum), i));
    for i in order {
        if left == 0 {
            break;
        }
        parts[i] += 1;
        left -= 1;
    }
    parts.into_iter().map(|p| p as i64).collect()
}

fn utf16_len(c: char) -> usize {
    c.len_utf16()
}

/// The end of the previous paragraph's text for `previous_text`: at most 100
/// characters (counted in UTF-16 units, so it fits however the server
/// counts), starting at a word.
pub fn previous_context(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut budget = CONTEXT_CHARS;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        if utf16_len(c) > budget {
            break;
        }
        budget -= utf16_len(c);
        start = i;
    }
    let mut tail = &text[start..];
    if start > 0
        && !text[..start].ends_with(' ')
        && let Some(sp) = tail.find(' ')
    {
        // Drop the partial first word.
        tail = &tail[sp + 1..];
    }
    let tail = tail.trim();
    (!tail.is_empty()).then(|| tail.to_string())
}

/// The start of the next paragraph's text for `future_text`: at most 100
/// characters (UTF-16 units), ending at a word.
pub fn future_context(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut budget = CONTEXT_CHARS;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        if utf16_len(c) > budget {
            break;
        }
        budget -= utf16_len(c);
        end = i + c.len_utf8();
    }
    let mut head = &text[..end];
    if end < text.len()
        && !text[end..].starts_with(' ')
        && let Some(sp) = head.rfind(' ')
    {
        // Drop the partial last word.
        head = &head[..sp];
    }
    let head = head.trim();
    (!head.is_empty()).then(|| head.to_string())
}

/// Automatic trim of a take cut from a dialogue. Only the paragraph's outer
/// edges are ElevenLabs' padding: the cuts between the takes lie in the
/// pauses between speakers, which are the dialogue's own timing and stay.
/// `None` when nothing would be cut.
pub fn edge_trim(
    pcm: &Pcm,
    settings: &TrimSettings,
    first: bool,
    last: bool,
) -> Option<(u32, u32)> {
    let (start, end) = trim::detect(pcm, settings)?;
    let dur = trim::duration_ms(pcm);
    let start = if first { start } else { 0 };
    let end = if last { end } else { dur };
    (end > start && (start, end) != (0, dur)).then_some((start, end))
}

#[cfg(test)]
pub(crate) mod testing {
    //! A synthetic dialogue response, shared by the cut tests and the
    //! worker's fake ElevenLabs server.

    use super::client::{Alignment, VoiceSegment};
    use crate::audio::Pcm;

    pub const LEAD_MS: u32 = 120;
    pub const GAP_MS: u32 = 250;
    pub const TAIL_MS: u32 = 200;

    /// A synthetic dialogue: 120 ms of silence, then each text as a tone of
    /// 40 ms per character (at least 300 ms), 250 ms of silence between the
    /// lines, 200 ms at the end.
    pub struct Synth {
        pub pcm: Pcm,
        /// Sample ranges of the tones.
        pub speech: Vec<(usize, usize)>,
        /// Contiguous voice segments, as the real API returns them: each
        /// line's segment runs until the next line starts.
        pub segments: Vec<VoiceSegment>,
        pub alignment: Alignment,
    }

    pub fn tone_ms(text: &str) -> u32 {
        (text.chars().count() as u32 * 40).max(300)
    }

    pub fn synth(texts: &[&str], voices: &[&str], rate: u32) -> Synth {
        let ms = |m: u32| (rate as u64 * m as u64 / 1000) as usize;
        let mut samples: Vec<i16> = vec![0; ms(LEAD_MS)];
        let mut speech = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            if i > 0 {
                samples.extend(std::iter::repeat_n(0, ms(GAP_MS)));
            }
            let start = samples.len();
            let n = ms(tone_ms(t));
            let freq = 180.0 + 60.0 * i as f64;
            samples.extend((0..n).map(|k| {
                ((k as f64 * freq * std::f64::consts::TAU / rate as f64).sin() * 8000.0) as i16
            }));
            speech.push((start, samples.len()));
        }
        samples.extend(std::iter::repeat_n(0, ms(TAIL_MS)));
        let secs = |s: usize| s as f64 / rate as f64;
        let mut segments = Vec::new();
        let mut alignment = Alignment::default();
        let mut offset = 0i64;
        for (i, t) in texts.iter().enumerate() {
            let (a, b) = speech[i];
            let start = if i == 0 { 0.0 } else { secs(a) };
            let end = match speech.get(i + 1) {
                Some(&(next, _)) => secs(next),
                None => secs(b) + 0.05,
            };
            let len = t.chars().count() as i64;
            segments.push(VoiceSegment {
                voice_id: voices[i].to_string(),
                start_time_seconds: start,
                end_time_seconds: end,
                character_start_index: offset,
                character_end_index: offset + len,
                dialogue_input_index: i as i64,
            });
            offset += len;
            let per = (secs(b) - secs(a)) / len.max(1) as f64;
            for (k, c) in t.chars().enumerate() {
                alignment.characters.push(c.to_string());
                alignment
                    .character_start_times_seconds
                    .push(secs(a) + per * k as f64);
                alignment
                    .character_end_times_seconds
                    .push(secs(a) + per * (k + 1) as f64);
            }
        }
        Synth {
            pcm: Pcm { rate, samples },
            speech,
            segments,
            alignment,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: Id, para: Id, speaker: &str, text: &str) -> Segment {
        Segment {
            id,
            paragraph_id: para,
            order: id,
            kind: if speaker == NARRATOR {
                SegmentKind::Narration
            } else {
                SegmentKind::Dialogue
            },
            speaker: speaker.into(),
            text: text.into(),
            settings_override: None,
            performance: None,
            status: SegmentStatus::Draft,
            current_take_id: None,
        }
    }

    fn project() -> Project {
        let mut voice_map = std::collections::BTreeMap::new();
        voice_map.insert(NARRATOR.to_string(), "vn".to_string());
        voice_map.insert("guard".to_string(), "vg".to_string());
        voice_map.insert("vortram".to_string(), "vv".to_string());
        // Same voice as the narrator: one voice, two speakers.
        voice_map.insert("echo".to_string(), "vn".to_string());
        Project {
            id: 1,
            name: "Book".into(),
            language: "en".into(),
            tts_model: "eleven_v4".into(),
            voice_map,
            default_voice_settings: VoiceSettings::default(),
            settings: ProjectSettings::default(),
        }
    }

    fn chapter() -> Vec<Segment> {
        vec![
            seg(1, 10, NARRATOR, "The night was cold."),
            seg(2, 11, "guard", "“Who goes there?”"),
            seg(3, 11, NARRATOR, "asked the guard."),
            seg(4, 11, "vortram", "“Only me.”"),
            seg(5, 12, NARRATOR, "He waited."),
            seg(6, 12, "echo", "“Hello?”"),
        ]
    }

    fn all(segs: &[Segment]) -> HashSet<Id> {
        segs.iter().map(|s| s.id).collect()
    }

    #[test]
    fn defaults_and_old_projects() {
        let d = DialogueSettings::default();
        assert!(d.enabled);
        assert_eq!(d.max_chars, 1800);
        // Settings saved before dialogue existed load with the defaults.
        let old: ProjectSettings =
            serde_json::from_str(r#"{"output_format":"pcm_24000"}"#).unwrap();
        assert_eq!(old.dialogue, DialogueSettings::default());
        let partial: ProjectSettings =
            serde_json::from_str(r#"{"dialogue":{"enabled":false}}"#).unwrap();
        assert_eq!(partial.dialogue.max_chars, 1800);
        assert!(!partial.dialogue.enabled);
        assert_eq!(
            DialogueSettings {
                enabled: true,
                max_chars: 99_999
            }
            .budget(),
            2000
        );
    }

    #[test]
    fn only_whole_multi_voice_draft_paragraphs_qualify() {
        let p = project();
        let segs = chapter();
        // Only paragraph 11 (guard, narrator, Vortram): paragraph 10 has one
        // segment, paragraph 12 two speakers on one voice.
        assert_eq!(plan(&p, &segs, &all(&segs), false), vec![vec![2, 3, 4]]);
        let paras = paragraphs(&segs);
        assert_eq!(paras.len(), 3);
        let req = all(&segs);
        assert_eq!(qualifies(&p, &paras[0], &req, false), Err(Skip::OneVoice));
        assert_eq!(qualifies(&p, &paras[2], &req, false), Err(Skip::OneVoice));

        // Re-rolls and partial requests use per-segment calls.
        assert_eq!(qualifies(&p, &paras[1], &req, true), Err(Skip::Reroll));
        let part: HashSet<Id> = [2, 3].into();
        assert_eq!(qualifies(&p, &paras[1], &part, false), Err(Skip::Partial));
        assert!(plan(&p, &segs, &part, false).is_empty());

        // A segment with audio, or not a draft.
        let mut s = segs.clone();
        s[3].current_take_id = Some(9);
        assert_eq!(
            qualifies(&p, &paragraphs(&s)[1], &req, false),
            Err(Skip::NotDraft)
        );
        let mut s = segs.clone();
        s[1].status = SegmentStatus::Rejected;
        assert_eq!(
            qualifies(&p, &paragraphs(&s)[1], &req, false),
            Err(Skip::NotDraft)
        );

        // No voice for a speaker.
        let mut s = segs.clone();
        s[3].speaker = "unknown".into();
        assert_eq!(
            qualifies(&p, &paragraphs(&s)[1], &req, false),
            Err(Skip::Unassigned)
        );

        // Model and setting.
        let mut q = p.clone();
        q.tts_model = "eleven_multilingual_v2".into();
        assert_eq!(qualifies(&q, &paras[1], &req, false), Err(Skip::Model));
        q.tts_model = "eleven_v4_turbo".into();
        assert_eq!(qualifies(&q, &paras[1], &req, false), Err(Skip::Model));
        q.tts_model = "eleven_v3".into();
        assert_eq!(qualifies(&q, &paras[1], &req, false), Ok(()));
        // Voice ids count as they are sent: trimmed.
        let mut q = p.clone();
        q.voice_map.insert("guard".into(), " vn ".into());
        q.voice_map.insert("vortram".into(), "vn".into());
        assert_eq!(qualifies(&q, &paras[1], &req, false), Err(Skip::OneVoice));
        let mut q = p.clone();
        q.settings.dialogue.enabled = false;
        assert_eq!(qualifies(&q, &paras[1], &req, false), Err(Skip::Disabled));
    }

    #[test]
    fn budget_voice_settings_and_tags() {
        let p = project();
        let segs = chapter();
        let req = all(&segs);
        let paras = paragraphs(&segs);
        let chars: usize = paras[1].iter().map(|s| s.text.chars().count()).sum();
        let mut q = p.clone();
        q.settings.dialogue.max_chars = chars;
        assert_eq!(qualifies(&q, &paras[1], &req, false), Ok(()));
        q.settings.dialogue.max_chars = chars - 1;
        assert_eq!(qualifies(&q, &paras[1], &req, false), Err(Skip::TooLong));

        // Tags count towards the budget (they are billed and sent).
        let mut s = segs.clone();
        s[1].performance = Some("“[whispers] Who goes there?”".into());
        let mut q = p.clone();
        q.settings.dialogue.max_chars = chars;
        assert_eq!(
            qualifies(&q, &paragraphs(&s)[1], &req, false),
            Err(Skip::TooLong)
        );
        // ... and [interrupting] keeps the paragraph per segment.
        s[1].performance = Some("“[interrupting] Who goes there?”".into());
        assert_eq!(
            qualifies(&p, &paragraphs(&s)[1], &req, false),
            Err(Skip::Interrupting)
        );

        // Per-segment voice settings and non-default speed or style.
        let mut s = segs.clone();
        s[2].settings_override = Some(VoiceSettings {
            stability: 0.2,
            ..Default::default()
        });
        assert_eq!(
            qualifies(&p, &paragraphs(&s)[1], &req, false),
            Err(Skip::OwnSettings)
        );
        // An override equal to the defaults is harmless.
        s[2].settings_override = Some(VoiceSettings::default());
        assert_eq!(qualifies(&p, &paragraphs(&s)[1], &req, false), Ok(()));
        let mut q = p.clone();
        q.default_voice_settings.speed = 1.1;
        assert_eq!(
            qualifies(&q, &paras[1], &req, false),
            Err(Skip::VoiceSettings)
        );

        // Eleven voices are too many.
        let mut q = p.clone();
        let mut many = Vec::new();
        for i in 0..11 {
            q.voice_map.insert(format!("c{i}"), format!("v{i}"));
            many.push(seg(100 + i, 50, &format!("c{i}"), "“Hi.”"));
        }
        let refs: Vec<&Segment> = many.iter().collect();
        let req: HashSet<Id> = many.iter().map(|s| s.id).collect();
        assert_eq!(qualifies(&q, &refs, &req, false), Err(Skip::TooManyVoices));
        assert_eq!(qualifies(&q, &refs[..10], &req, false), Ok(()));
    }

    #[test]
    fn inputs_settings_and_hashes() {
        let mut p = project();
        let mut segs = chapter();
        segs[1].performance = Some("“[whispers] Who goes there?”".into());
        let paras = paragraphs(&segs);
        let inputs = inputs(&p, &paras[1]).unwrap();
        assert_eq!(
            inputs,
            vec![
                Input {
                    text: "“[whispers] Who goes there?”".into(),
                    voice_id: "vg".into()
                },
                Input {
                    text: "asked the guard.".into(),
                    voice_id: "vn".into()
                },
                Input {
                    text: "“Only me.”".into(),
                    voice_id: "vv".into()
                },
            ]
        );

        let vs = VoiceSettings {
            stability: 0.62,
            similarity_boost: 0.8,
            ..Default::default()
        };
        let v4 = settings_for("eleven_v4", &vs);
        assert_eq!(
            serde_json::to_value(&v4).unwrap(),
            serde_json::json!({"stability": 0.62f32, "similarity": 0.8f32})
        );
        let v3 = settings_for("eleven_v3", &vs);
        assert_eq!(
            serde_json::to_value(&v3).unwrap(),
            serde_json::json!({"stability": 0.5})
        );
        assert_eq!(snap_v3_stability(0.1), 0.0);
        assert_eq!(snap_v3_stability(0.3), 0.5);
        assert_eq!(snap_v3_stability(0.75), 0.5);
        assert_eq!(snap_v3_stability(0.9), 1.0);
        assert_eq!(snap_v3_stability(f32::NAN), 0.5);

        let g = group_hash(&inputs, &v4, "eleven_v4", 1, "pcm_24000");
        assert_eq!(g, group_hash(&inputs, &v4, "eleven_v4", 1, "pcm_24000"));
        assert_ne!(g, group_hash(&inputs, &v4, "eleven_v4", 2, "pcm_24000"));
        assert_ne!(g, group_hash(&inputs, &v4, "eleven_v3", 1, "pcm_24000"));
        assert_ne!(g, group_hash(&inputs, &v4, "eleven_v4", 1, "pcm_44100"));
        assert_ne!(g, group_hash(&inputs, &v3, "eleven_v4", 1, "pcm_24000"));
        let mut other = inputs.clone();
        other[2].voice_id = "vx".into();
        assert_ne!(g, group_hash(&other, &v4, "eleven_v4", 1, "pcm_24000"));
        let mut other = inputs.clone();
        other[0].text = "“Who goes there?”".into();
        assert_ne!(g, group_hash(&other, &v4, "eleven_v4", 1, "pcm_24000"));
        let takes: HashSet<String> = (0..3).map(|i| take_hash(&g, i)).collect();
        assert_eq!(takes.len(), 3);
        // Never the same as a single-voice take of the same line.
        assert_ne!(
            take_hash(&g, 2),
            crate::hash::take_hash("“Only me.”", "vv", &vs, "eleven_v4", 1, "pcm_24000")
        );

        p.voice_map.insert("vortram".into(), "  ".into());
        assert!(super::inputs(&p, &paras[1]).is_none());
    }

    #[test]
    fn credits_split_exactly_by_characters() {
        assert_eq!(split_credits(47, &[17, 16, 10]), vec![19, 17, 11]);
        assert_eq!(split_credits(47, &[17, 16, 10]).iter().sum::<i64>(), 47);
        assert_eq!(split_credits(10, &[1, 1, 1]), vec![4, 3, 3]);
        assert_eq!(split_credits(0, &[5, 5]), vec![0, 0]);
        assert_eq!(split_credits(5, &[0, 0]), vec![3, 2]);
        assert_eq!(split_credits(-3, &[1, 2]), vec![0, 0]);
        assert!(split_credits(5, &[]).is_empty());
        for total in [1i64, 7, 99, 1801, 123_457] {
            let w = [3usize, 141, 59, 26, 5];
            let parts = split_credits(total, &w);
            assert_eq!(parts.iter().sum::<i64>(), total);
            for (p, &w) in parts.iter().zip(&w) {
                let exact = total as f64 * w as f64 / 234.0;
                assert!((*p as f64 - exact).abs() < 1.0, "{p} vs {exact}");
            }
        }
    }

    #[test]
    fn context_is_at_most_100_characters_on_word_boundaries() {
        assert_eq!(previous_context("Short one."), Some("Short one.".into()));
        assert_eq!(future_context("Short one."), Some("Short one.".into()));
        assert_eq!(previous_context("   "), None);
        let long = "word ".repeat(40) + "end.";
        let prev = previous_context(&long).unwrap();
        assert!(prev.chars().count() <= 100 && prev.ends_with("end."));
        assert!(prev.starts_with("word"), "{prev}");
        let fut = future_context(&long).unwrap();
        assert!(fut.chars().count() <= 100 && fut.starts_with("word"));
        assert!(fut.ends_with("word"), "{fut}");
        // Partial words are dropped at the cut.
        let text = format!("{} abcdefghij", "x".repeat(95));
        assert_eq!(previous_context(&text), Some("abcdefghij".into()));
        let text = format!("abcdefghij {}", "y".repeat(95));
        assert_eq!(future_context(&text), Some("abcdefghij".into()));
        // Line breaks become spaces; non-ASCII counts per character.
        let cz = "Řekl to tiše.\nA pak odešel – bez jediného slova.";
        assert_eq!(
            previous_context(cz).unwrap(),
            "Řekl to tiše. A pak odešel – bez jediného slova."
        );
        let many = "č".repeat(150);
        assert_eq!(previous_context(&many).unwrap().chars().count(), 100);
        // Characters outside the BMP take two UTF-16 units.
        let emoji = "🙂".repeat(80);
        assert_eq!(future_context(&emoji).unwrap().chars().count(), 50);
    }

    #[test]
    fn edge_trim_keeps_the_pauses_between_speakers() {
        use crate::trim::tests::{RATE, silence, tone};
        let pcm = Pcm {
            rate: RATE,
            samples: [silence(200), tone(300), silence(300)].concat(),
        };
        let s = TrimSettings::default();
        // Detected alone: 160–580 ms.
        assert_eq!(trim::detect(&pcm, &s), Some((160, 580)));
        assert_eq!(edge_trim(&pcm, &s, true, true), Some((160, 580)));
        assert_eq!(edge_trim(&pcm, &s, true, false), Some((160, 800)));
        assert_eq!(edge_trim(&pcm, &s, false, true), Some((0, 580)));
        assert_eq!(edge_trim(&pcm, &s, false, false), None);
        let quiet = Pcm {
            rate: RATE,
            samples: silence(300),
        };
        assert_eq!(edge_trim(&quiet, &s, true, true), None);
    }
}
