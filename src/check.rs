//! Local take check: every take is transcribed on this machine with Whisper
//! and compared with the segment's text, plus a free length check. A check
//! only ever FLAGS a take for a listen; it never rejects one, never blocks
//! approval and never spends credits.
//!
//! The pure parts live here and in `text` (normalisation, alignment,
//! verdict) and `resample` (16 kHz input). `models` handles the ggml model
//! files, `whisper` (feature `whisper`) the whisper-rs backend. The
//! background checker thread is in `worker::takecheck`; the UI in
//! `app::check`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::audio::Pcm;
use crate::model::Id;

pub mod models;
pub mod resample;
pub mod text;
#[cfg(feature = "whisper")]
mod whisper;

/// Whether this build includes the Whisper backend (feature `whisper`).
pub const BUILT_WITH_WHISPER: bool = cfg!(feature = "whisper");

/// Machine-dependent settings (`AppConfig.check`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckConfig {
    /// Check every new take automatically. "Check chapter" and "Re-check"
    /// work either way.
    pub enabled: bool,
    /// Model name from [`models::MODELS`].
    pub model: String,
    /// Threads for Whisper (default: half the cores, at least 1).
    pub threads: usize,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: models::DEFAULT_MODEL.into(),
            threads: default_threads(),
        }
    }
}

/// Half the cores, between 1 and 8 (the research's recommendation for a
/// background job; whisper.cpp itself defaults to min(4, cores)).
pub fn default_threads() -> usize {
    let hw = std::thread::available_parallelism().map_or(2, |n| n.get());
    (hw / 2).clamp(1, 8)
}

/// Take-check commands for the worker (`worker::Command::Check`).
#[derive(Debug, Clone)]
pub enum CheckCommand {
    /// Queue the current takes of a chapter that have no result for the
    /// current model (failed checks are retried; dismissed flags are kept).
    Chapter {
        chapter_id: Id,
    },
    /// Check a segment's current take again, ahead of the queue.
    Segment {
        segment_id: Id,
    },
    /// Empty the queue and abort the take being transcribed.
    Stop,
    /// New settings (also sent once at startup).
    Config(CheckConfig),
    Download {
        model: String,
    },
    CancelDownload,
    Delete {
        model: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// The words (and length) look right; minor differences are allowed.
    Ok,
    /// Worth a listen.
    Flagged,
    /// Flagged, but the user decided it is fine.
    Dismissed,
    /// Whisper could not run (the length check may still have run).
    Error,
    /// Words not checked (no model, or built without Whisper); the length
    /// was checked and looks normal.
    Skipped,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Flagged => "flagged",
            CheckStatus::Dismissed => "dismissed",
            CheckStatus::Error => "error",
            CheckStatus::Skipped => "skipped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => CheckStatus::Ok,
            "flagged" => CheckStatus::Flagged,
            "dismissed" => CheckStatus::Dismissed,
            "error" => CheckStatus::Error,
            "skipped" => CheckStatus::Skipped,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueKind {
    Words,
    Length,
    Error,
    Note,
}

/// One finding of a check, stored as JSON in `take_check.issues`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub kind: IssueKind,
    /// This finding flags the take.
    pub flag: bool,
    pub message: String,
}

/// A stored check result (`take_check` row).
#[derive(Debug, Clone, PartialEq)]
pub struct TakeCheck {
    pub take_id: Id,
    pub status: CheckStatus,
    /// What Whisper heard (after dropping known hallucinations); `None` when
    /// the words were not checked.
    pub transcript: Option<String>,
    pub wer: Option<f32>,
    pub issues: Vec<Issue>,
    /// Model that produced the transcript; empty for a length-only check.
    pub model: String,
    pub checked_at: String,
}

impl TakeCheck {
    /// The first flagging finding (for hover texts).
    pub fn reason(&self) -> Option<&str> {
        self.issues
            .iter()
            .find(|i| i.flag)
            .map(|i| i.message.as_str())
    }

    pub fn is_flagged(&self) -> bool {
        self.status == CheckStatus::Flagged
    }
}

/// Whether a take with result `existing` needs checking when the current
/// setup transcribes with `model` (empty: words cannot be checked now, only
/// the length). Failed checks are retried; dismissed flags stay dismissed;
/// a result from another model is replaced, but never by a length-only one.
/// `db::Db::takes_to_check` applies the same rule in SQL.
pub fn needs_check(existing: Option<&TakeCheck>, model: &str) -> bool {
    match existing {
        None => true,
        Some(c) if c.status == CheckStatus::Error => true,
        Some(c) if c.status == CheckStatus::Dismissed || model.is_empty() => false,
        Some(c) => c.model != model,
    }
}

// ---- transcription backend ------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct HeardSegment {
    pub text: String,
    /// whisper.cpp computes this once per 30 s window and copies it onto
    /// every segment of that window.
    pub no_speech_prob: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Transcript {
    pub segments: Vec<HeardSegment>,
}

impl Transcript {
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.text.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug)]
pub enum AsrError {
    // Only the Whisper backend (and test fakes) can be cancelled.
    #[cfg_attr(not(feature = "whisper"), allow(dead_code))]
    Cancelled,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for AsrError {
    fn from(e: anyhow::Error) -> Self {
        AsrError::Other(e)
    }
}

/// Speech recognition, behind a trait so everything else is testable
/// without a model.
pub trait Transcriber: Send {
    fn is_multilingual(&self) -> bool;
    /// `pcm16k`: 16 kHz mono f32. `lang`: the chapter's language code.
    /// Returns `Cancelled` soon after `cancel` is set.
    fn transcribe(
        &mut self,
        pcm16k: &[f32],
        lang: &str,
        threads: usize,
        cancel: &AtomicBool,
    ) -> Result<Transcript, AsrError>;
}

/// Loads a model file into a transcriber.
pub type Loader = dyn Fn(&Path) -> anyhow::Result<Box<dyn Transcriber>> + Send + Sync;

/// The Whisper loader, or `None` when built without Whisper.
pub fn whisper_loader() -> Option<Arc<Loader>> {
    #[cfg(feature = "whisper")]
    {
        Some(Arc::new(|p: &Path| {
            Ok(Box::new(whisper::Whisper::load(p)?) as Box<dyn Transcriber>)
        }))
    }
    #[cfg(not(feature = "whisper"))]
    {
        None
    }
}

/// Lower the calling thread to the lowest practical priority (nice 19 and
/// SCHED_IDLE). On Linux both are per thread and inherited by threads it
/// spawns later, i.e. ggml's per-computation worker pool.
pub fn background_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain syscalls on the calling thread with valid arguments.
    unsafe {
        let tid = libc::gettid();
        if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 19) != 0 {
            log::debug!("setpriority: {}", std::io::Error::last_os_error());
        }
        let p = libc::sched_param { sched_priority: 0 };
        if libc::sched_setscheduler(0, libc::SCHED_IDLE, &p) != 0 {
            log::debug!("SCHED_IDLE: {}", std::io::Error::last_os_error());
        }
    }
}

// ---- length check -----------------------------------------------------------

/// Typical narration pace at speed 1.0, in characters per second (an
/// estimate: about 150 words a minute).
pub const CHARS_PER_SEC: f32 = 15.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthVerdict {
    Normal,
    Short,
    Long,
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LengthCheck {
    /// From the first to the last sound (leading/trailing silence excluded).
    pub speech_ms: u32,
    pub expected_ms: u32,
    pub chars: usize,
    pub verdict: LengthVerdict,
}

/// The sounding part of a take, in ms, or `None` when it is silent.
pub fn speech_ms(pcm: &Pcm) -> Option<u32> {
    let tight = crate::trim::TrimSettings {
        keep_head_ms: 0,
        keep_tail_ms: 0,
        ..Default::default()
    };
    crate::trim::detect(pcm, &tight).map(|(a, b)| b - a)
}

/// Is the take's length plausible for `chars` characters at `speed`? Each
/// audio tag may add a pause or a sound, so it widens the upper bound.
/// The bounds are deliberately wide heuristics: flag only the clearly odd.
pub fn length_check(chars: usize, tags: usize, speed: f32, speech: Option<u32>) -> LengthCheck {
    let expected_ms = (chars as f32 / (CHARS_PER_SEC * speed.clamp(0.5, 2.0)) * 1000.0) as u32;
    let speech_ms = speech.unwrap_or(0);
    let verdict = match speech {
        None if chars > 0 => LengthVerdict::Silent,
        None => LengthVerdict::Normal,
        Some(ms) if expected_ms >= 1500 && (ms as f32) < expected_ms as f32 * 0.5 => {
            LengthVerdict::Short
        }
        Some(ms) if ms as f32 > expected_ms as f32 * 2.0 + 1500.0 + 1000.0 * tags as f32 => {
            LengthVerdict::Long
        }
        Some(_) => LengthVerdict::Normal,
    };
    LengthCheck {
        speech_ms,
        expected_ms,
        chars,
        verdict,
    }
}

fn secs(ms: u32) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

impl LengthCheck {
    pub fn issue(&self) -> Issue {
        let detail = format!(
            "{} of speech for {} characters (about {} expected)",
            secs(self.speech_ms),
            self.chars,
            secs(self.expected_ms)
        );
        let (flag, message) = match self.verdict {
            LengthVerdict::Normal => (false, format!("Length: {detail}.")),
            LengthVerdict::Short => (
                true,
                format!("Unusually short: {detail}. Words may be missing."),
            ),
            LengthVerdict::Long => (
                true,
                format!("Unusually long: {detail}. It may repeat words or contain extra sounds."),
            ),
            LengthVerdict::Silent => (true, "The take is silent.".to_string()),
        };
        Issue {
            kind: IssueKind::Length,
            flag,
            message,
        }
    }
}

// ---- hallucinations ---------------------------------------------------------

/// Text Whisper is known to invent on (near-)silent audio. The Czech
/// entries are commonly reported but unverified here.
const HALLUCINATIONS: &[&str] = &[
    "thank you",
    "thank you very much",
    "thank you so much",
    "thanks for watching",
    "thank you for watching",
    "please subscribe",
    "subtitles by the amara org community",
    "you",
    "titulky vytvoril johnyx",
    "dekuji za pozornost",
];

/// whisper.cpp already skips a window when it thinks it is silence and the
/// text is unlikely (no_speech > 0.6 and avg logprob < -1). On top of that,
/// only a segment that is BOTH in a near-silent window and a known
/// hallucination is dropped (never one the text itself contains):
/// dropping on no_speech alone would erase real speech. Returns the heard
/// text and what was dropped.
pub fn drop_hallucinations(
    t: &Transcript,
    expected_text: &str,
    lang: text::Lang,
) -> (String, Vec<String>) {
    let expected = format!(" {} ", text::normalized(expected_text, lang));
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for s in &t.segments {
        let norm = text::normalized(&s.text, lang);
        if s.no_speech_prob >= 0.6
            && HALLUCINATIONS.contains(&norm.as_str())
            && !expected.contains(&format!(" {norm} "))
        {
            dropped.push(s.text.trim().to_string());
        } else {
            kept.push(s.clone());
        }
    }
    (Transcript { segments: kept }.text(), dropped)
}

// ---- evaluation -------------------------------------------------------------

/// What the take should say.
pub struct Expected<'a> {
    /// The segment's text (no audio tags); spans for highlighting index it.
    pub text: &'a str,
    /// The tagged performance, when the model performs tags.
    pub performance: Option<&'a str>,
    /// Project language code ("en", "cs", …).
    pub lang: &'a str,
}

impl Expected<'_> {
    pub fn tag_count(&self) -> usize {
        self.performance.map_or(0, |p| crate::tags::find(p).len())
    }
}

/// The outcome of the word check.
pub enum Words {
    Heard(Transcript),
    /// Not transcribed, and why (no model, built without Whisper, …).
    NotChecked(String),
    Failed(String),
}

pub struct Evaluation {
    pub status: CheckStatus,
    pub transcript: Option<String>,
    pub wer: Option<f32>,
    pub issues: Vec<Issue>,
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Combine the length check and the word check into a result.
pub fn evaluate(
    e: &Expected,
    length: &LengthCheck,
    words: Words,
    t: &text::Thresholds,
) -> Evaluation {
    let lang = text::Lang::from_code(e.lang);
    let mut issues = Vec::new();
    let (mut transcript, mut wer) = (None, None);
    let mut failed = false;
    let mut checked = false;
    match words {
        Words::Heard(tr) => {
            checked = true;
            let (heard, dropped) = drop_hallucinations(&tr, e.text, lang);
            let tags = e
                .performance
                .map(|p| text::tag_phrases(p, lang))
                .unwrap_or_default();
            let c = text::check(e.text, &heard, lang, &tags, t);
            let n = c.expected.len();
            let issue = match c.verdict {
                text::Verdict::Flag(why) => Issue {
                    kind: IssueKind::Words,
                    flag: true,
                    message: format!("{} ({} of {} words differ).", capitalised(why), c.errors, n),
                },
                text::Verdict::Minor => Issue {
                    kind: IssueKind::Words,
                    flag: false,
                    message: format!(
                        "Minor differences ({} of {} words), likely recognition noise.",
                        c.errors, n
                    ),
                },
                text::Verdict::Match => Issue {
                    kind: IssueKind::Words,
                    flag: false,
                    message: "The words match the text.".into(),
                },
            };
            issues.push(issue);
            for d in dropped {
                issues.push(Issue {
                    kind: IssueKind::Note,
                    flag: false,
                    message: format!(
                        "Ignored “{d}”: Whisper tends to invent it on near-silent audio."
                    ),
                });
            }
            wer = Some(c.wer);
            transcript = Some(heard);
        }
        Words::NotChecked(why) => issues.push(Issue {
            kind: IssueKind::Note,
            flag: false,
            message: format!("Words not checked: {why}."),
        }),
        Words::Failed(why) => {
            failed = true;
            issues.push(Issue {
                kind: IssueKind::Error,
                flag: false,
                message: format!("Whisper failed: {why}"),
            });
        }
    }
    issues.push(length.issue());
    let status = if issues.iter().any(|i| i.flag) {
        CheckStatus::Flagged
    } else if failed {
        CheckStatus::Error
    } else if !checked {
        CheckStatus::Skipped
    } else {
        CheckStatus::Ok
    };
    Evaluation {
        status,
        transcript,
        wer,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heard(text: &str, nsp: f32) -> Transcript {
        Transcript {
            segments: vec![HeardSegment {
                text: text.into(),
                no_speech_prob: nsp,
            }],
        }
    }

    const LINE: &str = "The rain had not stopped for three days.";

    fn expected() -> Expected<'static> {
        Expected {
            text: LINE,
            performance: None,
            lang: "en",
        }
    }

    #[test]
    fn length_bounds() {
        // 45 characters at 15 per second: 3 s expected.
        let ok = length_check(45, 0, 1.0, Some(3_200));
        assert_eq!((ok.expected_ms, ok.verdict), (3_000, LengthVerdict::Normal));
        assert_eq!(
            length_check(45, 0, 1.0, Some(1_400)).verdict,
            LengthVerdict::Short
        );
        assert_eq!(
            length_check(45, 0, 1.0, Some(7_600)).verdict,
            LengthVerdict::Long
        );
        // Each tag allows another second; speed scales the expectation.
        assert_eq!(
            length_check(45, 1, 1.0, Some(7_600)).verdict,
            LengthVerdict::Normal
        );
        assert_eq!(
            length_check(45, 0, 0.7, Some(7_600)).verdict,
            LengthVerdict::Normal
        );
        // Short lines are never "too short" (Whisper-free noise), only silent.
        assert_eq!(
            length_check(8, 0, 1.0, Some(150)).verdict,
            LengthVerdict::Normal
        );
        assert_eq!(length_check(8, 0, 1.0, None).verdict, LengthVerdict::Silent);
        assert_eq!(length_check(0, 0, 1.0, None).verdict, LengthVerdict::Normal);
        assert!(length_check(45, 0, 1.0, Some(1_400)).issue().flag);
        let msg = ok.issue().message;
        assert_eq!(
            msg,
            "Length: 3.2 s of speech for 45 characters (about 3.0 s expected)."
        );
    }

    #[test]
    fn speech_span_ignores_edge_silence() {
        let rate = 24_000;
        let mut samples = vec![0i16; rate / 2];
        samples.extend((0..rate).map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16));
        samples.extend(vec![0i16; rate / 4]);
        let pcm = Pcm {
            rate: rate as u32,
            samples,
        };
        assert_eq!(speech_ms(&pcm), Some(1_000));
        assert_eq!(
            speech_ms(&Pcm {
                rate: 24_000,
                samples: vec![0; 2400]
            }),
            None
        );
    }

    #[test]
    fn hallucinations_need_silence_and_a_known_phrase() {
        let lang = text::Lang::En;
        // Near-silent window + known phrase: dropped.
        let (h, d) = drop_hallucinations(&heard(" Thank you.", 0.8), "Go on.", lang);
        assert_eq!((h.as_str(), d), ("", vec!["Thank you.".to_string()]));
        // Confident speech is kept, even if it is a known phrase.
        assert_eq!(
            drop_hallucinations(&heard(" Thank you.", 0.1), "Go on.", lang).0,
            "Thank you."
        );
        // An unknown phrase in a silent window is kept for the alignment.
        assert_eq!(
            drop_hallucinations(&heard(" Go on.", 0.9), "Go on.", lang).0,
            "Go on."
        );
        // When the text itself says it, it is not a hallucination.
        assert_eq!(
            drop_hallucinations(&heard(" Thank you.", 0.9), "“Thank you,” she said.", lang).0,
            "Thank you."
        );
        // Several segments: only the hallucinated one goes.
        let t = Transcript {
            segments: vec![
                HeardSegment {
                    text: " Go on.".into(),
                    no_speech_prob: 0.7,
                },
                HeardSegment {
                    text: " Thanks for watching!".into(),
                    no_speech_prob: 0.7,
                },
            ],
        };
        assert_eq!(drop_hallucinations(&t, "Go on.", lang).0, "Go on.");
    }

    #[test]
    fn evaluation_statuses() {
        let t = text::Thresholds::default();
        let normal = length_check(LINE.chars().count(), 0, 1.0, Some(2_700));
        let e = expected();
        // Matching words, normal length.
        let ev = evaluate(
            &e,
            &normal,
            Words::Heard(heard(&format!(" {LINE}"), 0.0)),
            &t,
        );
        assert_eq!(ev.status, CheckStatus::Ok);
        assert_eq!(ev.wer, Some(0.0));
        assert_eq!(ev.transcript.as_deref(), Some(LINE));
        assert_eq!(ev.issues.len(), 2);
        assert!(!ev.issues[0].flag && ev.issues[0].message.contains("match"));
        // Missing words flag.
        let ev = evaluate(
            &e,
            &normal,
            Words::Heard(heard(" The rain had for days.", 0.0)),
            &t,
        );
        assert_eq!(ev.status, CheckStatus::Flagged);
        assert!(
            ev.issues[0].message.ends_with("(3 of 8 words differ)."),
            "{}",
            ev.issues[0].message
        );
        // A hallucination on a silent take: dropped, so nothing was heard.
        let ev = evaluate(&e, &normal, Words::Heard(heard(" Thank you.", 0.9)), &t);
        assert_eq!(ev.status, CheckStatus::Flagged);
        assert_eq!(ev.transcript.as_deref(), Some(""));
        assert!(
            ev.issues
                .iter()
                .any(|i| i.kind == IssueKind::Note && i.message.contains("Thank you."))
        );
        // No Whisper: length only.
        let ev = evaluate(
            &e,
            &normal,
            Words::NotChecked("no Whisper model installed".into()),
            &t,
        );
        assert_eq!(
            (ev.status, ev.transcript, ev.wer),
            (CheckStatus::Skipped, None, None)
        );
        let short = length_check(LINE.chars().count(), 0, 1.0, Some(900));
        // About 2.7 s expected, 0.9 s heard: flagged without Whisper.
        assert_eq!(
            evaluate(&e, &short, Words::NotChecked("x".into()), &t).status,
            CheckStatus::Flagged
        );
        let silent = length_check(LINE.chars().count(), 0, 1.0, None);
        assert_eq!(
            evaluate(&e, &silent, Words::NotChecked("x".into()), &t).status,
            CheckStatus::Flagged
        );
        // Whisper failed: error unless the length flags anyway.
        assert_eq!(
            evaluate(&e, &normal, Words::Failed("boom".into()), &t).status,
            CheckStatus::Error
        );
        assert_eq!(
            evaluate(&e, &silent, Words::Failed("boom".into()), &t).status,
            CheckStatus::Flagged
        );
        // A tag read aloud.
        let tagged = Expected {
            performance: Some("[whispers] The rain had not stopped for three days."),
            ..expected()
        };
        let ev = evaluate(
            &tagged,
            &normal,
            Words::Heard(heard(
                " Whispers. The rain had not stopped for three days.",
                0.0,
            )),
            &t,
        );
        assert_eq!(ev.status, CheckStatus::Flagged);
        assert_eq!(tagged.tag_count(), 1);
    }

    #[test]
    fn statuses_roundtrip() {
        for s in [
            CheckStatus::Ok,
            CheckStatus::Flagged,
            CheckStatus::Dismissed,
            CheckStatus::Error,
            CheckStatus::Skipped,
        ] {
            assert_eq!(CheckStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(CheckStatus::parse("nope"), None);
    }

    #[test]
    fn config_defaults() {
        let c = CheckConfig::default();
        assert!(c.enabled);
        assert_eq!(c.model, "small");
        assert!(c.threads >= 1 && c.threads <= 8);
        // Old config files without the section get the defaults.
        let c: CheckConfig = serde_json::from_str("{\"model\": \"base\"}").unwrap();
        assert!(c.enabled && c.threads >= 1);
        assert_eq!(c.model, "base");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn background_priority_is_per_thread() {
        fn sched() -> (i32, u32) {
            let tid = unsafe { libc::gettid() };
            let s = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).unwrap();
            let f: Vec<&str> = s[s.rfind(')').unwrap() + 2..].split_whitespace().collect();
            (f[19 - 3].parse().unwrap(), f[41 - 3].parse().unwrap())
        }
        let main = sched();
        let (worker, child) = std::thread::spawn(|| {
            background_priority();
            (sched(), std::thread::spawn(sched).join().unwrap())
        })
        .join()
        .unwrap();
        assert_eq!(worker.1, libc::SCHED_IDLE as u32);
        assert_eq!(child, worker, "threads it spawns inherit it");
        assert_eq!(sched(), main, "other threads are untouched");
    }
}
