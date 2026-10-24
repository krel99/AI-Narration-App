//! Background workers. The UI thread sends [`Command`]s over a channel and
//! receives [`Event`]s back; everything slow (Claude, ElevenLabs, ffmpeg)
//! runs here on a tokio runtime. All state lives in SQLite, so every stage
//! can resume after a restart.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::Semaphore;

use crate::audio;
use crate::db::{Db, Library, NewTake};
use crate::hash::{request_hash, take_hash};
use crate::lang;
use crate::llm::{self, ClaudeClient, LlmError, PromptParagraph};
use crate::model::*;
use crate::segment::{self, DraftSegment, UNKNOWN_SPEAKER};
use crate::tags;
use crate::tts::{ElevenLabsClient, TtsError, TtsRequest, VoiceInfo};

mod account;
mod autotrim;
mod claude;
mod dialogue;
mod retail;
mod takecheck;
mod voices;

/// Seed of a segment's first take. Fixed, so identical lines with the same
/// voice and settings ("he said,") share one cached take.
pub const FIRST_TAKE_SEED: u32 = 1;

#[derive(Debug, Clone, Default)]
pub struct ApiKeys {
    pub anthropic: String,
    pub elevenlabs: String,
    /// Where the Claude passes run.
    pub claude_backend: crate::config::ClaudeBackend,
    /// The configured `claude` binary (Claude Code backend).
    pub claude_code_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum Command {
    /// Attribute every paragraph of the chapter whose segments are all still
    /// untouched drafts (or that has none yet). With `resume`, only the
    /// paragraphs a stopped pass left (Continue segmenting).
    SegmentChapter {
        chapter_id: Id,
        resume: bool,
    },
    /// Ask Claude for audio tags on the chapter's draft segments (replacing
    /// earlier tags on those drafts).
    DirectChapter {
        chapter_id: Id,
    },
    /// Generate audio. `reroll` forces a new random seed.
    Generate {
        segment_ids: Vec<Id>,
        reroll: bool,
    },
    ExportChapter {
        chapter_id: Id,
        draft: bool,
    },
    ExportBook {
        project_id: Id,
    },
    /// "<Book title> - Retail Sample.mp3" from approved takes, starting at
    /// `start_segment` (or the chapter start), at most `max_ms` long.
    ExportSample {
        chapter_id: Id,
        start_segment: Option<Id>,
        max_ms: u64,
    },
    /// The voices in the user's account (answered by `Event::Voices`).
    FetchVoices,
    /// Look up assigned voice ids that are not in the account list
    /// (answered by `Event::VoicesLookedUp`).
    LookupVoices {
        ids: Vec<String>,
    },
    /// A voice's preview audio, downloaded once and cached (answered by
    /// `Event::VoicePreview`).
    PreviewVoice {
        voice: Box<VoiceInfo>,
        language: String,
    },
    /// The ElevenLabs balance and model rates (answered by `Event::Account`).
    FetchAccount,
    /// Check the Claude Code CLI (answered by `Event::ClaudeCode`).
    DetectClaudeCode {
        path: Option<PathBuf>,
    },
    /// Detect and store the silence trim of the current take of every
    /// unapproved segment (metadata only, no credits).
    AutoTrimChapter {
        chapter_id: Id,
    },
    /// Local take check (Whisper): queueing, settings and models.
    Check(crate::check::CheckCommand),
    SetKeys(ApiKeys),
    /// Stop queued work: no further request is sent (requests already in
    /// flight finish and are kept).
    Cancel,
}

#[derive(Debug)]
pub enum Event {
    Info(String),
    Error(String),
    SegmentationProgress {
        chapter_id: Id,
        done: usize,
        total: usize,
    },
    SegmentationFinished {
        chapter_id: Id,
        llm_paragraphs: usize,
        fallback_paragraphs: usize,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A segment's segments/takes/status changed in the database.
    SegmentsChanged {
        chapter_id: Id,
    },
    GenerationStarted {
        segment_ids: Vec<Id>,
    },
    SegmentGenerated {
        segment_id: Id,
    },
    GenerationFailed {
        segment_id: Id,
        error: String,
    },
    GenerationFinished {
        generated: usize,
        cached: usize,
        failed: usize,
        credits: i64,
    },
    ExportFinished {
        files: Vec<PathBuf>,
        gaps: usize,
        duration_ms: u64,
    },
    /// An export (chapter, book or retail sample) failed. The only failure
    /// that ends the UI's export state.
    ExportFailed(String),
    /// The account's voices, or why they could not be loaded.
    Voices(std::result::Result<Vec<VoiceInfo>, String>),
    VoicesLookedUp {
        found: Vec<VoiceInfo>,
        /// Ids ElevenLabs has no voice for.
        missing: Vec<String>,
    },
    VoicePreview {
        voice_id: String,
        result: std::result::Result<audio::Pcm, String>,
    },
    DirectionProgress {
        chapter_id: Id,
        done: usize,
        total: usize,
    },
    DirectionFinished {
        chapter_id: Id,
        tagged: usize,
        rejected: usize,
        input_tokens: u64,
        output_tokens: u64,
    },
    Account(crate::tts::account::AccountInfo),
    ClaudeCode(Box<crate::claude_code::Detection>),
    /// An exported chapter (or, without a chapter, the retail sample) was
    /// checked against the ACX/Findaway spec; the report is stored.
    AcxChecked {
        summary: String,
        verdict: crate::acx::Verdict,
    },
    /// A take of this segment got a (new) check result.
    TakeChecked {
        segment_id: Id,
    },
    /// Take-check queue: `done` of `total` since it was last empty; 0 of 0
    /// when the checker is idle.
    CheckProgress {
        done: usize,
        total: usize,
    },
    ModelDownload {
        model: String,
        received: u64,
        total: Option<u64>,
    },
    ModelDownloadFinished {
        model: String,
        error: Option<String>,
    },
}

pub struct WorkerHandle {
    tx: Sender<Command>,
    pub events: Receiver<Event>,
}

impl WorkerHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

#[derive(Clone)]
struct Emitter {
    tx: Sender<Event>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Emitter {
    fn emit(&self, e: Event) {
        let _ = self.tx.send(e);
        (self.repaint)();
    }
}

#[derive(Clone)]
struct Ctx {
    lib: Library,
    db: Arc<Mutex<Db>>,
    http: reqwest::Client,
    keys: Arc<RwLock<ApiKeys>>,
    out: Emitter,
    /// Bumped by Cancel; jobs stop when it no longer matches their start value.
    epoch: Arc<AtomicU64>,
    endpoints: Endpoints,
    checker: takecheck::Checker,
    /// Segments a running generation will store takes for. The segmentation
    /// and audio-tag passes leave them alone (checked under the database
    /// lock, as the generation registers them), so a take is never deleted
    /// or detached by a pass that read the chapter before it existed.
    busy: Arc<Mutex<HashSet<Id>>>,
}

/// Marks segments busy for as long as it lives (a paragraph's generation
/// task), panics included.
struct BusyGuard {
    busy: Arc<Mutex<HashSet<Id>>>,
    ids: Vec<Id>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        for id in &self.ids {
            busy.remove(id);
        }
    }
}

/// API endpoints; overridden in tests to point at local fakes.
#[derive(Clone)]
struct Endpoints {
    anthropic: String,
    elevenlabs: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            anthropic: llm::API_URL.into(),
            elevenlabs: crate::tts::API_BASE.into(),
        }
    }
}

impl Ctx {
    fn db<T>(&self, f: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
        let mut db = self
            .db
            .lock()
            .map_err(|_| anyhow!("database lock poisoned"))?;
        f(&mut db)
    }

    fn cancelled(&self, started: u64) -> bool {
        self.epoch.load(Ordering::SeqCst) != started
    }

    /// The segments being generated right now (see `busy`). Lock it inside
    /// `db(..)`, never the other way round.
    fn busy(&self) -> std::sync::MutexGuard<'_, HashSet<Id>> {
        self.busy.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn spawn(
    lib: Library,
    keys: ApiKeys,
    repaint: impl Fn() + Send + Sync + 'static,
) -> Result<WorkerHandle> {
    let (cmd_tx, cmd_rx) = channel::<Command>();
    let (ev_tx, ev_rx) = channel::<Event>();
    let db = lib.open()?;
    let ctx = Ctx {
        lib,
        db: Arc::new(Mutex::new(db)),
        http: reqwest::Client::builder().build()?,
        keys: Arc::new(RwLock::new(keys)),
        out: Emitter {
            tx: ev_tx,
            repaint: Arc::new(repaint),
        },
        epoch: Arc::new(AtomicU64::new(0)),
        endpoints: Endpoints::default(),
        checker: Default::default(),
        busy: Default::default(),
    };
    takecheck::start(&ctx, crate::check::whisper_loader());
    std::thread::Builder::new()
        .name("workers".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            while let Ok(cmd) = cmd_rx.recv() {
                let ctx = ctx.clone();
                match cmd {
                    Command::Cancel => {
                        ctx.epoch.fetch_add(1, Ordering::SeqCst);
                        ctx.out.emit(Event::Info("Cancelled queued work.".into()));
                    }
                    Command::SetKeys(k) => {
                        if let Ok(mut keys) = ctx.keys.write() {
                            *keys = k;
                        }
                    }
                    cmd => {
                        rt.spawn(async move {
                            if let Err(e) = run(&ctx, cmd).await {
                                ctx.out.emit(Event::Error(format!("{e:#}")));
                            }
                        });
                    }
                }
            }
        })?;
    Ok(WorkerHandle {
        tx: cmd_tx,
        events: ev_rx,
    })
}

async fn run(ctx: &Ctx, cmd: Command) -> Result<()> {
    match cmd {
        Command::SegmentChapter { chapter_id, resume } => {
            let started = ctx.epoch.load(Ordering::SeqCst);
            segment_chapter(ctx, chapter_id, resume).await?;
            if ctx.cancelled(started) {
                // Cancel stopped the pass: no tag pass after it either.
                return Ok(());
            }
            let project = ctx.db(|db| db.project(db.chapter(chapter_id)?.project_id))?;
            if project.settings.audio_tags.enabled && lang::model_supports_tags(&project.tts_model)
            {
                direct_chapter(ctx, chapter_id, true).await?;
            }
            Ok(())
        }
        Command::DirectChapter { chapter_id } => direct_chapter(ctx, chapter_id, false).await,
        Command::Generate {
            segment_ids,
            reroll,
        } => generate(ctx, segment_ids, reroll).await,
        Command::ExportChapter { chapter_id, draft } => {
            export_outcome(ctx, retail::export_chapter(ctx, chapter_id, draft).await)
        }
        Command::ExportBook { project_id } => {
            export_outcome(ctx, retail::export_book(ctx, project_id).await)
        }
        Command::ExportSample {
            chapter_id,
            start_segment,
            max_ms,
        } => export_outcome(
            ctx,
            retail::export_sample(ctx, chapter_id, start_segment, max_ms).await,
        ),
        Command::FetchVoices => voices::fetch_voices(ctx).await,
        Command::LookupVoices { ids } => voices::lookup_voices(ctx, ids).await,
        Command::PreviewVoice { voice, language } => {
            voices::preview_voice(ctx, *voice, language).await
        }
        Command::FetchAccount => account::fetch_account(ctx).await,
        Command::DetectClaudeCode { path } => {
            claude::detect_claude_code(ctx, path).await;
            Ok(())
        }
        Command::AutoTrimChapter { chapter_id } => {
            autotrim::auto_trim_chapter(ctx, chapter_id).await
        }
        Command::Check(c) => takecheck::run_command(ctx, c).await,
        Command::Cancel | Command::SetKeys(_) => Ok(()),
    }
}

/// Export failures get their own event, so the UI ends its export state on
/// them and never on an unrelated job's error.
fn export_outcome(ctx: &Ctx, result: Result<()>) -> Result<()> {
    if let Err(e) = result {
        ctx.out
            .emit(Event::ExportFailed(format!("Export failed: {e:#}")));
    }
    Ok(())
}

fn backoff(attempt: u32) -> Duration {
    let base = 1u64 << attempt.min(5);
    Duration::from_millis(base * 1000 + rand::random::<u64>() % 500)
}

// ---- segmentation -------------------------------------------------------

/// Map a speaker key from the LLM onto the registry: exact key, then alias or
/// display name. Unknown names stay as they are and get registered.
fn resolve_speaker(raw: &str, registry: &[Character]) -> String {
    if raw == NARRATOR || raw == UNKNOWN_SPEAKER || raw.is_empty() {
        return if raw.is_empty() {
            UNKNOWN_SPEAKER.into()
        } else {
            raw.into()
        };
    }
    if registry.iter().any(|c| c.key == raw) {
        return raw.into();
    }
    for c in registry {
        if llm::normalize_key(&c.display_name) == raw
            || c.aliases.iter().any(|a| llm::normalize_key(a) == raw)
        {
            return c.key.clone();
        }
    }
    raw.into()
}

/// Store a paragraph's new segments, unless it got audio in the meantime
/// (any take, also on a draft, or a segment `busy` being generated): then it
/// is left exactly as it is and this returns false. No paid take is ever
/// deleted by re-segmenting.
fn store_segments(
    db: &mut Db,
    busy: &HashSet<Id>,
    paragraph: &Paragraph,
    segs: &[DraftSegment],
) -> Result<bool> {
    if db
        .paragraph_segments(paragraph.id)?
        .iter()
        .any(|s| busy.contains(&s.id))
    {
        db.set_attribution_pending(&[paragraph.id], false)?;
        return Ok(false);
    }
    let rows: Vec<(SegmentKind, String, String)> = segs
        .iter()
        .map(|d| {
            (
                d.kind,
                d.speaker.clone(),
                d.text(&paragraph.text).to_string(),
            )
        })
        .collect();
    db.replace_untouched_paragraph(paragraph.id, &rows)
}

/// Store `segs` for `paragraph` (see [`store_segments`]); counts it in
/// `kept` when it got audio meanwhile and was left alone.
fn store_or_keep(
    ctx: &Ctx,
    paragraph: &Paragraph,
    segs: &[DraftSegment],
    kept: &mut usize,
) -> Result<()> {
    let stored = ctx.db(|db| {
        let busy = ctx.busy();
        store_segments(db, &busy, paragraph, segs)
    })?;
    if !stored {
        log::info!(
            "P{}: got audio while being segmented; left as it is",
            paragraph.order + 1
        );
        *kept += 1;
    }
    Ok(())
}

async fn segment_chapter(ctx: &Ctx, chapter_id: Id, resume: bool) -> Result<()> {
    let started = ctx.epoch.load(Ordering::SeqCst);
    let (chapter, project, paragraphs, existing, takes, busy, waiting) = ctx.db(|db| {
        let chapter = db.chapter(chapter_id)?;
        let project = db.project(chapter.project_id)?;
        let paragraphs = db.paragraphs(chapter_id)?;
        let mut existing: HashMap<Id, Vec<Segment>> = HashMap::new();
        for s in db.chapter_segments(chapter_id)? {
            existing.entry(s.paragraph_id).or_default().push(s);
        }
        let takes = db.take_counts(chapter_id)?;
        let busy = ctx.busy().clone();
        let waiting: HashSet<Id> = db.attribution_pending(chapter_id)?.into_iter().collect();
        Ok((chapter, project, paragraphs, existing, takes, busy, waiting))
    })?;
    let lang = lang::by_code(&project.language);
    let settings = project.settings.segmentation.clone();

    // Only paragraphs that are still untouched drafts may be (re)segmented:
    // no take on any segment (a draft keeps its paid takes after a tag,
    // speaker or text edit) and nothing being generated. Anything with audio
    // is left alone so no credits are thrown away; storing checks again.
    let editable = |p: &Paragraph| {
        existing.get(&p.id).is_none_or(|segs| {
            segs.iter().all(|s| {
                s.status == SegmentStatus::Draft
                    && s.current_take_id.is_none()
                    && !takes.contains_key(&s.id)
                    && !busy.contains(&s.id)
            })
        })
    };
    let mut targets: Vec<usize> = Vec::new();
    let mut kept = 0usize;
    let mut not_editable: Vec<Id> = Vec::new();
    for (i, p) in paragraphs.iter().enumerate() {
        // Continue segmenting: only what a stopped pass left.
        if resume && !waiting.contains(&p.id) {
            continue;
        }
        if !editable(p) {
            if waiting.contains(&p.id) {
                not_editable.push(p.id);
            }
            continue;
        }
        if segment::has_dialogue(&p.text, lang) {
            targets.push(i);
        } else {
            // Pure narration needs no LLM call.
            let segs = segment::parse_paragraph(&p.text, lang);
            store_or_keep(ctx, p, &segs, &mut kept)?;
        }
    }
    // Flag the dialogue paragraphs until they are stored, so a pass that
    // stops (usage limit, sign-in, Cancel) can be continued without sending
    // the finished ones again.
    let target_ids: Vec<Id> = targets.iter().map(|&i| paragraphs[i].id).collect();
    ctx.db(|db| {
        db.set_attribution_pending(&not_editable, false)?;
        db.set_attribution_pending(&target_ids, true)
    })?;

    let total = targets.len();
    let mut done = 0usize;
    let mut fallback = 0usize;
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    ctx.out.emit(Event::SegmentationProgress {
        chapter_id,
        done,
        total,
    });

    // Without a usable backend, dialogue falls back to the parser.
    let claude = if total > 0 {
        match claude::claude_client(ctx).await {
            Ok(c) => Some(c),
            Err(why) => {
                ctx.out.emit(Event::Error(format!(
                    "{why} Dialogue was split by the parser only; speakers are 'unknown'."
                )));
                None
            }
        }
    } else {
        None
    };
    // Set when Claude cannot answer at all (usage limit, signed out): the
    // rest of the chapter is left as it is instead of falling back.
    let mut stopped: Option<String> = None;

    // Windows of `chunk_paragraphs` consecutive paragraphs.
    let chunk = settings.chunk_paragraphs.max(1);
    let mut windows: Vec<Vec<usize>> = Vec::new();
    for &t in &targets {
        match windows.last_mut() {
            Some(w) if t / chunk == w[0] / chunk => w.push(t),
            _ => windows.push(vec![t]),
        }
    }

    for window in windows {
        if ctx.cancelled(started) {
            break;
        }
        let mut pending: BTreeSet<usize> = window.iter().copied().collect();
        let mut gave_up: Vec<usize> = Vec::new();
        let mut tries: HashMap<usize, u32> = HashMap::new();
        // After a too-long response, send one target paragraph at a time.
        let mut single = false;
        while !pending.is_empty() && !ctx.cancelled(started) {
            let Some(claude) = claude.as_ref() else {
                break;
            };
            let batch: Vec<usize> = if single {
                pending.iter().take(1).copied().collect()
            } else {
                pending.iter().copied().collect()
            };
            let first = batch[0];
            let last = *batch.last().unwrap();
            let lo = first.saturating_sub(settings.overlap_paragraphs);
            let registry = ctx.db(|db| db.characters(project.id))?;
            let known = ctx.db(|db| {
                let mut m: HashMap<Id, Vec<String>> = HashMap::new();
                for s in db.chapter_segments(chapter_id)? {
                    let v = m.entry(s.paragraph_id).or_default();
                    if s.kind == SegmentKind::Dialogue && !v.contains(&s.speaker) {
                        v.push(s.speaker);
                    }
                }
                Ok(m)
            })?;
            let prompt: Vec<PromptParagraph> = (lo..=last)
                .map(|i| PromptParagraph {
                    number: i + 1,
                    text: paragraphs[i].text.clone(),
                    target: batch.contains(&i),
                    known_speakers: if batch.contains(&i) {
                        vec![]
                    } else {
                        known.get(&paragraphs[i].id).cloned().unwrap_or_default()
                    },
                })
                .collect();

            let res = claude.attribute(&settings, lang, &registry, &prompt).await;
            claude::record_usage(
                ctx,
                claude,
                project.id,
                chapter_id,
                claude::PASS_ATTRIBUTION,
            );
            match res {
                Ok(result) => {
                    log::info!(
                        "attribution P{}–P{}: {} input / {} output tokens, cost {:?} USD",
                        first + 1,
                        last + 1,
                        result.input_tokens,
                        result.output_tokens,
                        result.cost_usd
                    );
                    input_tokens += result.input_tokens;
                    output_tokens += result.output_tokens;
                    let registry = ctx.db(|db| {
                        for c in &result.characters {
                            let name = if c.display_name.trim().is_empty() {
                                &c.key
                            } else {
                                &c.display_name
                            };
                            db.upsert_character(project.id, &c.key, name, &c.aliases)?;
                        }
                        db.characters(project.id)
                    })?;
                    for &i in &batch {
                        let p = &paragraphs[i];
                        let Some(proposed) = result.paragraphs.get(&(i + 1)) else {
                            log::warn!("P{}: no segments returned", i + 1);
                            continue;
                        };
                        let mut proposed = proposed.clone();
                        for s in &mut proposed {
                            s.speaker = resolve_speaker(&s.speaker, &registry);
                        }
                        match segment::validate_proposed(&p.text, &proposed) {
                            Ok(segs) => {
                                ctx.db(|db| {
                                    for s in &segs {
                                        if s.speaker != NARRATOR
                                            && s.speaker != UNKNOWN_SPEAKER
                                            && !registry.iter().any(|c| c.key == s.speaker)
                                        {
                                            db.upsert_character(
                                                project.id,
                                                &s.speaker,
                                                &s.speaker,
                                                &[],
                                            )?;
                                        }
                                    }
                                    Ok(())
                                })?;
                                store_or_keep(ctx, p, &segs, &mut kept)?;
                                pending.remove(&i);
                                done += 1;
                            }
                            Err(why) => log::warn!("P{} rejected: {why}", i + 1),
                        }
                    }
                    ctx.out.emit(Event::SegmentationProgress {
                        chapter_id,
                        done,
                        total,
                    });
                    ctx.out.emit(Event::SegmentsChanged { chapter_id });
                }
                Err(LlmError::TooLong) if batch.len() > 1 => {
                    single = true;
                    continue;
                }
                Err(e @ LlmError::Transient(..)) => {
                    log::warn!("{e}");
                    tokio::time::sleep(backoff(tries.get(&first).copied().unwrap_or(0))).await;
                }
                Err(e) if e.stops_pass() => {
                    stopped = Some(e.to_string());
                    break;
                }
                Err(e) => {
                    ctx.out.emit(Event::Error(format!(
                        "Segmentation of P{}–P{}: {e}",
                        first + 1,
                        last + 1
                    )));
                    break;
                }
            }
            // Count a try for every paragraph of the batch still pending.
            for i in batch {
                if pending.contains(&i) {
                    let t = tries.entry(i).or_default();
                    *t += 1;
                    if *t > settings.max_retries {
                        pending.remove(&i);
                        gave_up.push(i);
                    }
                }
            }
        }
        pending.extend(gave_up);
        if ctx.cancelled(started) || stopped.is_some() {
            // Leave the rest as it is, flagged: Continue segmenting resumes
            // with exactly these paragraphs.
            break;
        }
        // Whatever the LLM could not do validly falls back to the parser.
        for &i in &pending {
            let p = &paragraphs[i];
            let segs = segment::parse_paragraph(&p.text, lang);
            store_or_keep(ctx, p, &segs, &mut kept)?;
            fallback += 1;
            done += 1;
        }
        ctx.out.emit(Event::SegmentationProgress {
            chapter_id,
            done,
            total,
        });
        ctx.out.emit(Event::SegmentsChanged { chapter_id });
    }

    ctx.db(|db| db.set_chapter_status(chapter.id, ChapterStatus::Segmented))?;
    let left = ctx.db(|db| db.attribution_pending(chapter_id))?.len();
    ctx.out.emit(Event::SegmentsChanged { chapter_id });
    ctx.out.emit(Event::SegmentationFinished {
        chapter_id,
        llm_paragraphs: done - fallback,
        fallback_paragraphs: fallback,
        input_tokens,
        output_tokens,
    });
    if kept > 0 {
        ctx.out.emit(Event::Info(format!(
            "{kept} paragraphs got audio while they were being segmented and were left as they are (no take was discarded)."
        )));
    }
    let remaining = if left == 1 {
        "1 dialogue paragraph is".to_string()
    } else {
        format!("{left} dialogue paragraphs are")
    };
    if let Some(why) = stopped {
        return Err(anyhow!(
            "{why} {remaining} not attributed yet and left as they are; Continue segmenting attributes only those, once Claude is available again."
        ));
    }
    if left > 0 && ctx.cancelled(started) {
        ctx.out.emit(Event::Info(format!(
            "Segmentation of \"{}\" stopped by Cancel: {remaining} not attributed yet (Continue segmenting finishes them); no audio tags were added.",
            chapter.title
        )));
    }
    Ok(())
}

// ---- audio tags -----------------------------------------------------------

/// Insert audio tags into the chapter's draft segments. With `untagged_only`
/// (the automatic run after segmentation) segments that already carry tags
/// are left alone; otherwise their tags are replaced.
async fn direct_chapter(ctx: &Ctx, chapter_id: Id, untagged_only: bool) -> Result<()> {
    let started = ctx.epoch.load(Ordering::SeqCst);
    let claude = claude::claude_client(ctx)
        .await
        .map_err(|why| anyhow!("{why} No audio tags were added."))?;
    let (project, paragraphs, segments) = ctx.db(|db| {
        let chapter = db.chapter(chapter_id)?;
        Ok((
            db.project(chapter.project_id)?,
            db.paragraphs(chapter_id)?,
            db.chapter_segments(chapter_id)?,
        ))
    })?;
    let lang = lang::by_code(&project.language);
    let settings = project.settings.segmentation.clone();
    let para_index: HashMap<Id, usize> = paragraphs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id, i))
        .collect();
    // Only drafts without audio: tagging changes what is spoken.
    let targets: Vec<&Segment> = segments
        .iter()
        .filter(|s| s.status == SegmentStatus::Draft && s.current_take_id.is_none())
        .filter(|s| !(untagged_only && s.performance.is_some()))
        .collect();
    let total = targets.len();
    let chunk = settings.chunk_paragraphs.max(1);

    // Work queue of paragraph windows; a too-long answer splits its window.
    let mut windows: Vec<(usize, usize)> = (0..paragraphs.len())
        .step_by(chunk)
        .map(|lo| (lo, (lo + chunk).min(paragraphs.len())))
        .rev()
        .collect();
    let (mut done, mut tagged, mut rejected) = (0usize, 0usize, 0usize);
    // Segments that got audio (or were edited) while Claude was answering.
    let mut skipped = 0usize;
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    // Why the pass stopped early: reported after DirectionFinished, so the
    // error (e.g. a usage limit with its reset time) is the last message.
    let mut failed: Option<String> = None;
    ctx.out.emit(Event::DirectionProgress {
        chapter_id,
        done,
        total,
    });
    while let Some((lo, hi)) = windows.pop() {
        if ctx.cancelled(started) {
            break;
        }
        let batch: Vec<&Segment> = targets
            .iter()
            .copied()
            .filter(|s| {
                para_index
                    .get(&s.paragraph_id)
                    .is_some_and(|&p| p >= lo && p < hi)
            })
            .collect();
        if batch.is_empty() {
            continue;
        }
        let context: Vec<String> = paragraphs[lo.saturating_sub(settings.overlap_paragraphs)..lo]
            .iter()
            .map(|p| p.text.clone())
            .collect();
        let offered: Vec<llm::DirectionSegment> = batch
            .iter()
            .map(|s| llm::DirectionSegment {
                id: s.id,
                paragraph: para_index[&s.paragraph_id] + 1,
                kind: s.kind,
                speaker: s.speaker.clone(),
                text: s.text.clone(),
            })
            .collect();
        let mut attempt = 0;
        let result = loop {
            let r = claude
                .direct(
                    &settings,
                    &project.settings.audio_tags,
                    lang,
                    &context,
                    &offered,
                )
                .await;
            claude::record_usage(ctx, &claude, project.id, chapter_id, claude::PASS_TAGS);
            match r {
                Err(e @ LlmError::Transient(..)) if attempt < settings.max_retries => {
                    log::warn!("{e}");
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                }
                other => break other,
            }
        };
        match result {
            Ok(direction) => {
                log::info!(
                    "audio tags P{}–P{}: {} input / {} output tokens, cost {:?} USD",
                    lo + 1,
                    hi,
                    direction.input_tokens,
                    direction.output_tokens,
                    direction.cost_usd
                );
                input_tokens += direction.input_tokens;
                output_tokens += direction.output_tokens;
                ctx.db(|db| {
                    let busy = ctx.busy();
                    for s in &batch {
                        // Read again: the answer can come minutes after the
                        // targets were chosen. Tags go only onto a segment
                        // that is still the draft without audio Claude saw;
                        // writing them onto one generated (or being
                        // generated) meanwhile would detach its paid take.
                        let unchanged = db.segment(s.id).is_ok_and(|now| {
                            now.status == SegmentStatus::Draft
                                && now.current_take_id.is_none()
                                && now.text == s.text
                                && now.performance == s.performance
                                && !busy.contains(&s.id)
                        });
                        if !unchanged {
                            skipped += 1;
                            continue;
                        }
                        let new = match direction.performances.get(&s.id) {
                            Some(perf) => match tags::validate(&s.text, perf) {
                                Ok(p) => p,
                                Err(why) => {
                                    log::warn!("segment {}: tags rejected: {why}", s.id);
                                    rejected += 1;
                                    continue;
                                }
                            },
                            None => None,
                        };
                        tagged += new.is_some() as usize;
                        db.set_segment_performance(s.id, new.as_deref())?;
                    }
                    Ok(())
                })?;
                done += batch.len();
            }
            Err(LlmError::TooLong) if hi - lo > 1 => {
                let mid = lo + (hi - lo) / 2;
                windows.push((mid, hi));
                windows.push((lo, mid));
                continue;
            }
            Err(e) => {
                failed = Some(format!("Audio tags for P{}–P{}: {e}", lo + 1, hi));
                break;
            }
        }
        ctx.out.emit(Event::DirectionProgress {
            chapter_id,
            done,
            total,
        });
        ctx.out.emit(Event::SegmentsChanged { chapter_id });
    }
    ctx.out.emit(Event::DirectionFinished {
        chapter_id,
        tagged,
        rejected,
        input_tokens,
        output_tokens,
    });
    if skipped > 0 {
        ctx.out.emit(Event::Info(format!(
            "{skipped} segments got audio or were edited while their audio tags were being written; they were left as they are (their takes stay current)."
        )));
    }
    match failed {
        Some(why) => Err(anyhow!(why)),
        None => Ok(()),
    }
}

// ---- generation ---------------------------------------------------------

struct Job {
    segment_id: Id,
    chapter_id: Id,
}

/// A job that was not sent because of Cancel (not a failure).
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// One generation's counters, and which segments it reported: every segment
/// it started gets exactly one SegmentGenerated or GenerationFailed, even
/// when a task dies, so none is left spinning in the UI.
#[derive(Default)]
struct Stats {
    /// (generated, cached, failed, credits)
    counts: Mutex<(usize, usize, usize, i64)>,
    reported: Mutex<HashSet<Id>>,
}

impl Stats {
    fn counts(&self) -> std::sync::MutexGuard<'_, (usize, usize, usize, i64)> {
        self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn report(&self, ctx: &Ctx, e: Event, segment_id: Id) {
        self.reported
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(segment_id);
        ctx.out.emit(e);
    }

    /// Credits spent on something that is not a take (a discarded dialogue).
    fn add_credits(&self, credits: i64) {
        self.counts().3 += credits;
    }

    /// `segment_id` got a take (`cached`: without a request).
    fn generated(&self, ctx: &Ctx, segment_id: Id, cached: bool, credits: i64) {
        {
            let mut c = self.counts();
            c.0 += 1;
            c.1 += cached as usize;
            c.3 += credits;
        }
        self.report(ctx, Event::SegmentGenerated { segment_id }, segment_id);
    }

    fn failed(&self, ctx: &Ctx, segment_id: Id, error: String) {
        self.counts().2 += 1;
        self.report(
            ctx,
            Event::GenerationFailed { segment_id, error },
            segment_id,
        );
    }

    /// Not sent because of Cancel.
    fn cancelled(&self, ctx: &Ctx, segment_id: Id) {
        self.report(
            ctx,
            Event::GenerationFailed {
                segment_id,
                error: "cancelled".into(),
            },
            segment_id,
        );
    }

    fn was_reported(&self, segment_id: Id) -> bool {
        self.reported
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&segment_id)
    }
}

async fn generate(ctx: &Ctx, segment_ids: Vec<Id>, reroll: bool) -> Result<()> {
    let started = ctx.epoch.load(Ordering::SeqCst);
    let key = ctx
        .keys
        .read()
        .map(|k| k.elevenlabs.clone())
        .unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow!("set the ElevenLabs API key in Settings first"));
    }
    let tts = Arc::new(ElevenLabsClient::new(
        ctx.http.clone(),
        key,
        &ctx.endpoints.elevenlabs,
    ));

    // Group by paragraph, in reading order, so stitching sees the previous
    // segment's request id; paragraphs run in parallel.
    let mut groups: Vec<(Id, Vec<Job>)> = Vec::new();
    let mut concurrency = 1;
    ctx.db(|db| {
        let mut seen = HashSet::new();
        let mut by_chapter: HashMap<Id, Vec<Id>> = HashMap::new();
        for id in &segment_ids {
            if seen.insert(*id) {
                // A segment removed since (merged, re-segmented) is skipped.
                match db.segment_chapter_id(*id) {
                    Ok(c) => by_chapter.entry(c).or_default().push(*id),
                    Err(e) => log::info!("segment {id} is not generated: {e:#}"),
                }
            }
        }
        // Registered under the database lock, as the segmentation and tag
        // passes check it there. A segment another generation is working on
        // is not requested twice.
        let mut busy = ctx.busy();
        for (chapter_id, ids) in by_chapter {
            let wanted: HashSet<Id> = ids.into_iter().collect();
            let project = db.project(db.chapter(chapter_id)?.project_id)?;
            concurrency = project.settings.concurrency.max(1);
            for s in db.chapter_segments(chapter_id)? {
                if !wanted.contains(&s.id)
                    || s.status == SegmentStatus::Approved
                    || busy.contains(&s.id)
                {
                    continue;
                }
                let job = Job {
                    segment_id: s.id,
                    chapter_id,
                };
                match groups.last_mut() {
                    Some((pid, jobs)) if *pid == s.paragraph_id => jobs.push(job),
                    _ => groups.push((s.paragraph_id, vec![job])),
                }
            }
        }
        // Only now that nothing can fail: the guards below release them.
        for (_, jobs) in &groups {
            busy.extend(jobs.iter().map(|j| j.segment_id));
        }
        Ok(())
    })?;
    let guards: Vec<BusyGuard> = groups
        .iter()
        .map(|(_, jobs)| BusyGuard {
            busy: ctx.busy.clone(),
            ids: jobs.iter().map(|j| j.segment_id).collect(),
        })
        .collect();

    let ids: Vec<Id> = groups
        .iter()
        .flat_map(|(_, j)| j.iter().map(|j| j.segment_id))
        .collect();
    ctx.out.emit(Event::GenerationStarted {
        segment_ids: ids.clone(),
    });
    // Multi-speaker paragraphs that go through Text-to-Dialogue.
    let through_dialogue = Arc::new(dialogue::plan_request(ctx, &ids, reroll));
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let stats = Arc::new(Stats::default());

    let mut handles = Vec::new();
    for ((paragraph_id, jobs), guard) in groups.into_iter().zip(guards) {
        let ctx = ctx.clone();
        let tts = tts.clone();
        let semaphore = semaphore.clone();
        let stats = stats.clone();
        let as_dialogue = through_dialogue.contains(&paragraph_id);
        let plan = through_dialogue.clone();
        handles.push(tokio::spawn(async move {
            // The paragraph's segments stay busy until its task ends.
            let _guard = guard;
            // What dialogue does not (or cannot) generate goes per segment.
            let jobs = if as_dialogue {
                dialogue::generate_paragraph(&ctx, &semaphore, started, &stats, &plan, jobs).await
            } else {
                jobs
            };
            for job in jobs {
                if ctx.cancelled(started) {
                    stats.cancelled(&ctx, job.segment_id);
                    continue;
                }
                match generate_one(&ctx, &tts, &semaphore, started, &job, reroll).await {
                    Ok((cached, credits)) => {
                        stats.generated(&ctx, job.segment_id, cached, credits);
                    }
                    Err(e) if e.is::<Cancelled>() => stats.cancelled(&ctx, job.segment_id),
                    Err(e) => stats.failed(&ctx, job.segment_id, format!("{e:#}")),
                }
                ctx.out.emit(Event::SegmentsChanged {
                    chapter_id: job.chapter_id,
                });
            }
        }));
    }
    for h in handles {
        if let Err(e) = h.await {
            log::error!("a generation task ended early: {e}");
        }
    }
    for &id in &ids {
        if !stats.was_reported(id) {
            stats.failed(ctx, id, "internal error (see the log)".into());
        }
    }
    let (generated, cached, failed, credits) = *stats.counts();
    ctx.out.emit(Event::GenerationFinished {
        generated,
        cached,
        failed,
        credits,
    });
    Ok(())
}

/// What became of a finished take.
enum Placed {
    /// It is the segment's current take.
    Current,
    /// The segment was approved meanwhile: it keeps its approved take.
    Approved,
    /// Its speaker, text, tags or settings changed meanwhile: the take is
    /// for a version that no longer exists.
    Changed,
}

/// Make `take_id` the current take of segment `segment_id`, but only while
/// the segment is still what was requested (`hash` with `seed`) and not
/// approved: a take that finishes after the user approved the segment, or
/// changed its speaker or tags, never replaces what they decided. The take
/// stays in the take list either way. Call inside [`Db::immediate`].
fn place_take(
    db: &Db,
    project_id: Id,
    segment_id: Id,
    take_id: Id,
    seed: u32,
    hash: &str,
) -> Result<Placed> {
    let now = db.segment(segment_id)?;
    if now.status == SegmentStatus::Approved {
        return Ok(Placed::Approved);
    }
    let project = db.project(project_id)?;
    if request_hash(&project, &now, seed).as_deref() != Some(hash) {
        return Ok(Placed::Changed);
    }
    db.set_current_take(segment_id, take_id)?;
    Ok(Placed::Current)
}

/// Tell the user why a finished take did not become current.
fn report_placed(ctx: &Ctx, segment_id: Id, placed: &Placed) {
    let why = match placed {
        Placed::Current => return,
        Placed::Approved => {
            "finished after the segment was approved: the approved take stays current"
        }
        Placed::Changed => {
            "finished after the segment's speaker, text, tags or voice settings changed: it is not used"
        }
    };
    let number = ctx
        .db(|db| Ok(db.paragraph(db.segment(segment_id)?.paragraph_id)?.order + 1))
        .map(|n| format!("¶ {n}"))
        .unwrap_or_else(|_| "a segment".into());
    ctx.out.emit(Event::Info(format!(
        "A take for {number} {why}. The new take is kept in the segment's take list."
    )));
}

/// Generate (or reuse) one take. Returns (cache hit, credits used).
async fn generate_one(
    ctx: &Ctx,
    tts: &ElevenLabsClient,
    semaphore: &Semaphore,
    started: u64,
    job: &Job,
    reroll: bool,
) -> Result<(bool, i64)> {
    // Everything the request needs, read fresh: an earlier segment of this
    // paragraph may just have produced the request id we stitch onto.
    let (project, seg, prev_text, next_text, prev_ids, used_seeds) = ctx.db(|db| {
        let seg = db.segment(job.segment_id)?;
        let project = db.project(db.chapter(job.chapter_id)?.project_id)?;
        let all = db.chapter_segments(job.chapter_id)?;
        let pos = all
            .iter()
            .position(|s| s.id == seg.id)
            .ok_or_else(|| anyhow!("segment vanished"))?;
        let prev_text = pos.checked_sub(1).map(|i| all[i].text.clone());
        let next_text = all.get(pos + 1).map(|s| s.text.clone());
        let voice = project
            .voice_for(&seg.speaker)
            .unwrap_or_default()
            .to_string();
        let mut prev_ids = Vec::new();
        for s in all[..pos]
            .iter()
            .rev()
            .take_while(|s| s.paragraph_id == seg.paragraph_id)
        {
            if prev_ids.len() == 3 {
                break;
            }
            if project.voice_for(&s.speaker) != Some(voice.as_str()) {
                continue;
            }
            if let Some(t) = s.current_take_id {
                let take = db.take(t)?;
                // A dialogue take carries the dialogue request's id, which is
                // not a text-to-speech generation to stitch onto.
                if take.dialogue_group.is_none()
                    && let Some(rid) = take.request_id
                {
                    prev_ids.push(rid);
                }
            }
        }
        prev_ids.reverse();
        let used_seeds: HashSet<u32> = db.takes(seg.id)?.iter().map(|t| t.seed).collect();
        Ok((project, seg, prev_text, next_text, prev_ids, used_seeds))
    })?;

    if seg.status == SegmentStatus::Approved {
        return Err(anyhow!("segment is approved (locked)"));
    }
    let voice_id = project
        .voice_for(&seg.speaker)
        .ok_or_else(|| anyhow!("unassigned voice for speaker '{}'", seg.speaker))?
        .to_string();
    let settings = seg
        .settings_override
        .clone()
        .unwrap_or_else(|| project.default_voice_settings.clone());
    let format = project.settings.output_format.clone();

    let seed = if !reroll && !used_seeds.contains(&FIRST_TAKE_SEED) {
        FIRST_TAKE_SEED
    } else {
        loop {
            let s: u32 = rand::random();
            if !used_seeds.contains(&s) {
                break s;
            }
        }
    };
    let spoken = seg.spoken_text(&project.tts_model).to_string();
    let hash = take_hash(
        &spoken,
        &voice_id,
        &settings,
        &project.tts_model,
        seed,
        &format,
    );
    let rel_path = format!("{hash}.wav");
    let abs_path = ctx.lib.audio_path(&rel_path);

    // Cache: identical inputs reuse the existing audio.
    if let Some(existing) = ctx.db(|db| db.take_by_hash(&hash))?
        && ctx.lib.audio_path(&existing.audio_path).exists()
    {
        let (take_id, placed) = ctx.db(|db| {
            db.immediate(|db| {
                let take_id = if existing.segment_id == seg.id {
                    existing.id
                } else {
                    db.insert_take(&NewTake {
                        segment_id: seg.id,
                        seed,
                        request_id: existing.request_id.as_deref(),
                        audio_path: &existing.audio_path,
                        hash: &hash,
                        credits_used: 0,
                        credits_exact: true,
                    })?
                };
                Ok((
                    take_id,
                    place_take(db, project.id, seg.id, take_id, seed, &hash)?,
                ))
            })
        })?;
        report_placed(ctx, seg.id, &placed);
        if take_id != existing.id {
            autotrim::trim_new_take(ctx, &project, take_id).await;
        }
        if matches!(placed, Placed::Current) {
            takecheck::new_take(ctx, take_id);
        }
        return Ok((true, 0));
    }

    let mut req = TtsRequest {
        voice_id,
        output_format: format.clone(),
        text: spoken.clone(),
        model_id: project.tts_model.clone(),
        voice_settings: settings,
        seed,
        language_code: lang::model_accepts_language_code(&project.tts_model)
            .then(|| project.language.clone()),
        previous_text: prev_text,
        next_text,
        previous_request_ids: if project.settings.request_stitching {
            prev_ids
        } else {
            vec![]
        },
    };

    let mut attempt = 0u32;
    let resp = loop {
        let result = {
            let _permit = semaphore.acquire().await?;
            // Cancel stops every request not sent yet, retries included:
            // jobs wait here for a free slot.
            if ctx.cancelled(started) {
                return Err(Cancelled.into());
            }
            tts.synthesize(&req).await
        };
        match result {
            Ok(r) => break r,
            Err(TtsError::RateLimited {
                retry_after,
                message,
            }) if attempt < 6 => {
                log::info!("429 ({message}); backing off");
                tokio::time::sleep(retry_after.unwrap_or_else(|| backoff(attempt))).await;
            }
            Err(TtsError::Transient(m)) if attempt < 3 => {
                log::warn!("ElevenLabs: {m}");
                tokio::time::sleep(backoff(attempt)).await;
            }
            // Stale or unsupported request ids: retry once without stitching.
            Err(TtsError::Rejected {
                status: 400 | 422, ..
            }) if !req.previous_request_ids.is_empty() => {
                req.previous_request_ids.clear();
            }
            Err(e) => return Err(e.into()),
        }
        attempt += 1;
    };

    let pcm = match audio::pcm_format_rate(&format) {
        Some(rate) => audio::from_pcm_bytes(&resp.audio, rate),
        None => tokio::task::spawn_blocking(move || audio::decode_mp3(resp.audio)).await??,
    };
    let path = abs_path.clone();
    tokio::task::spawn_blocking(move || audio::write_wav(&path, &pcm)).await??;
    // Billed rates differ per plan, so a missing header means "estimated", not
    // "text length": estimate with the learned rate and flag it as inexact.
    let chars = spoken.chars().count();
    let (credits, credits_exact) = match resp.character_cost {
        Some(c) => (c, true),
        None => (
            ctx.db(|db| Ok(db.credit_rate(&project.tts_model).credits(chars)))?,
            false,
        ),
    };
    let stored = ctx.db(|db| {
        db.immediate(|db| {
            if !db.segment_exists(seg.id)? {
                return Ok(None);
            }
            let take_id = db.insert_take(&NewTake {
                segment_id: seg.id,
                seed,
                request_id: resp.request_id.as_deref(),
                audio_path: &rel_path,
                hash: &hash,
                credits_used: credits,
                credits_exact,
            })?;
            // What this take cost per character teaches the credit estimates.
            // Bookkeeping only: the paid take is kept even if this fails.
            if let Err(e) = db.record_take_cost(take_id, &project.tts_model, chars as i64) {
                log::warn!("recording the cost of take {take_id}: {e:#}");
            }
            Ok(Some((
                take_id,
                place_take(db, project.id, seg.id, take_id, seed, &hash)?,
            )))
        })
    })?;
    let Some((take_id, placed)) = stored else {
        // Deleted while the request ran (its chapter or project): nothing
        // to attach the paid audio to.
        ctx.out.emit(Event::Info(format!(
            "A segment was deleted while its take was being generated ({credits} credits); the audio file stays in the library's audio folder."
        )));
        return Ok((false, credits));
    };
    report_placed(ctx, seg.id, &placed);
    autotrim::trim_new_take(ctx, &project, take_id).await;
    if matches!(placed, Placed::Current) {
        takecheck::new_take(ctx, take_id);
    }
    Ok((false, credits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    mod costs;
    mod races;
    mod retail;
    mod voices;

    #[test]
    fn speakers_resolve_through_aliases() {
        let registry = vec![Character {
            id: 1,
            project_id: 1,
            key: "vortram".into(),
            display_name: "Vortram the Elder".into(),
            aliases: vec!["the old man".into()],
        }];
        assert_eq!(resolve_speaker("vortram", &registry), "vortram");
        assert_eq!(resolve_speaker("the_old_man", &registry), "vortram");
        assert_eq!(resolve_speaker("vortram_the_elder", &registry), "vortram");
        assert_eq!(resolve_speaker("ann", &registry), "ann");
        assert_eq!(resolve_speaker("", &registry), UNKNOWN_SPEAKER);
        assert_eq!(resolve_speaker(NARRATOR, &registry), NARRATOR);
    }

    type Reply = (u16, Vec<(String, String)>, Vec<u8>);
    type RequestLog = Arc<Mutex<Vec<(String, String)>>>;

    /// Minimal HTTP/1.1 server: records (path, body) and answers with `handler`.
    pub(super) fn serve(
        handler: impl Fn(usize, &str, &str) -> Reply + Send + 'static,
    ) -> (String, RequestLog) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log: RequestLog = Arc::default();
        let log2 = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8_lossy(&body).to_string();
                let n = {
                    let mut l = log2.lock().unwrap();
                    l.push((path.clone(), body.clone()));
                    l.len() - 1
                };
                let (status, headers, payload) = handler(n, &path, &body);
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    payload.len()
                );
                for (k, v) in headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&payload);
            }
        });
        (url, log)
    }

    pub(super) fn test_ctx(
        lib: &Library,
        anthropic: &str,
        elevenlabs: &str,
    ) -> (Ctx, Receiver<Event>) {
        let (tx, rx) = channel();
        let ctx = Ctx {
            lib: lib.clone(),
            db: Arc::new(Mutex::new(lib.open().unwrap())),
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            // The pipeline tests talk to a fake Messages API; the Claude
            // Code tests switch the backend themselves.
            keys: Arc::new(RwLock::new(ApiKeys {
                anthropic: "test".into(),
                elevenlabs: "test".into(),
                claude_backend: crate::config::ClaudeBackend::ApiKey,
                claude_code_path: None,
            })),
            out: Emitter {
                tx,
                repaint: Arc::new(|| {}),
            },
            epoch: Arc::new(AtomicU64::new(0)),
            endpoints: Endpoints {
                anthropic: anthropic.into(),
                elevenlabs: elevenlabs.into(),
            },
            checker: Default::default(),
            busy: Default::default(),
        };
        (ctx, rx)
    }

    pub(super) fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    const TEXT: &str = "\"It is,\" started Vortram, \"truly magnificent!\"\n\nThe end.";

    /// Import TEXT, split it with the parser and give the quotes to Vortram.
    /// These tests exercise per-segment generation, so the project's
    /// multi-speaker dialogue (on by default) is turned off; it has its own
    /// tests in worker/dialogue/tests.rs.
    pub(super) fn import(db: &mut Db, project_id: Id) -> Id {
        let mut p = db.project(project_id).unwrap();
        if p.settings.dialogue.enabled {
            p.settings.dialogue.enabled = false;
            db.update_project(&p).unwrap();
        }
        let cid = db.import_chapter(project_id, "One", TEXT).unwrap();
        for p in db.paragraphs(cid).unwrap() {
            let segs = segment::parse_paragraph(&p.text, lang::by_code("en"));
            store_segments(db, &HashSet::new(), &p, &segs).unwrap();
        }
        for s in db.chapter_segments(cid).unwrap() {
            if s.kind == SegmentKind::Dialogue {
                db.set_segment_speaker(s.id, "vortram").unwrap();
            }
        }
        cid
    }

    #[test]
    fn generation_stitches_caches_and_rerolls() {
        let (url, requests) = serve(|n, _path, body| {
            if n == 0 {
                return (
                    429,
                    vec![("retry-after".into(), "0".into())],
                    b"{\"detail\":{\"message\":\"busy\"}}".to_vec(),
                );
            }
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let chars = v["text"].as_str().unwrap().chars().count();
            let pcm: Vec<u8> = (0..240i16).flat_map(|s| (s * 50).to_le_bytes()).collect();
            (
                200,
                vec![
                    ("request-id".into(), format!("req-{n}")),
                    ("character-cost".into(), chars.to_string()),
                ],
                pcm,
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, events) = test_ctx(&lib, "http://unused", &url);
        let (cid, segs) = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = "pcm_24000".into();
                db.update_project(&p)?;
                let cid = import(db, pid);
                Ok((cid, db.chapter_segments(cid)?))
            })
            .unwrap();
        assert_eq!(segs.len(), 4);
        let ids: Vec<Id> = segs.iter().map(|s| s.id).collect();

        rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
        let reqs = requests.lock().unwrap().clone();
        assert_eq!(reqs.len(), 5, "one 429 plus four takes");

        let after = ctx.db(|db| db.chapter_segments(cid)).unwrap();
        assert!(
            after
                .iter()
                .all(|s| s.status == SegmentStatus::Generated && s.current_take_id.is_some())
        );
        let take0 = ctx
            .db(|db| db.take(after[0].current_take_id.unwrap()))
            .unwrap();
        assert_eq!(take0.seed, FIRST_TAKE_SEED);
        assert_eq!(take0.credits_used, "\"It is,\"".chars().count() as i64);
        assert!(take0.credits_exact, "character-cost header was present");
        assert!(lib.audio_path(&take0.audio_path).exists());

        // The second quote stitches onto the first (same voice) and gets its neighbours' text.
        let body = |text: &str| -> serde_json::Value {
            reqs.iter()
                .filter_map(|(path, b)| {
                    let mut v: serde_json::Value = serde_json::from_str(b).ok()?;
                    v["path"] = path.clone().into();
                    Some(v)
                })
                .find(|v| v["text"] == text)
                .unwrap()
        };
        let quote2 = body("\"truly magnificent!\"");
        assert_eq!(quote2["path"], "/text-to-speech/vv?output_format=pcm_24000");
        assert_eq!(
            quote2["previous_request_ids"],
            serde_json::json!([take0.request_id.clone().unwrap()])
        );
        assert_eq!(quote2["previous_text"], "started Vortram,");
        assert_eq!(quote2["next_text"], "The end.");
        assert_eq!(quote2["seed"], FIRST_TAKE_SEED);
        let tag = body("started Vortram,");
        assert!(
            tag.get("previous_request_ids").is_none(),
            "different voice: no stitching"
        );

        // Same text, voices and settings in another chapter: all cache hits.
        let cid2 = ctx
            .db(|db| Ok(import(db, db.chapter(cid)?.project_id)))
            .unwrap();
        let ids2: Vec<Id> = ctx
            .db(|db| db.chapter_segments(cid2))
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        rt().block_on(generate(&ctx, ids2.clone(), false)).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 5, "no new requests");
        let stats = ctx
            .db(|db| db.chapter_stats(cid2, &db.project(db.chapter(cid2)?.project_id)?))
            .unwrap();
        assert_eq!(stats.generated, 4);
        assert_eq!(stats.credits_used, 0);

        // A re-roll uses a fresh seed and keeps the old take for comparison.
        rt().block_on(generate(&ctx, vec![ids[0]], true)).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 6);
        let takes = ctx.db(|db| db.takes(ids[0])).unwrap();
        assert_eq!(takes.len(), 2);
        let current = ctx
            .db(|db| db.segment(ids[0]))
            .unwrap()
            .current_take_id
            .unwrap();
        assert_eq!(current, takes[1].id);
        assert_ne!(takes[1].seed, FIRST_TAKE_SEED);

        // Approved segments are skipped; unassigned voices fail without a request.
        ctx.db(|db| {
            db.set_segment_status(ids[1], SegmentStatus::Approved)?;
            db.set_segment_speaker(ids[2], "nobody")
        })
        .unwrap();
        rt().block_on(generate(&ctx, vec![ids[1], ids[2]], true))
            .unwrap();
        assert_eq!(requests.lock().unwrap().len(), 6);
        let failed: Vec<String> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::GenerationFailed { segment_id, error } if segment_id == ids[2] => {
                    Some(error)
                }
                _ => None,
            })
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].contains("unassigned voice"), "{}", failed[0]);
    }

    fn claude_reply(output: serde_json::Value) -> Reply {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": output.to_string()}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        (
            200,
            vec![("content-type".into(), "application/json".into())],
            body.to_string().into_bytes(),
        )
    }

    const STORY: &str = "The night was cold.\n\n\"Who goes there?\" asked the guard.\n\n\"Only me,\" said the old man.";

    #[test]
    fn segmentation_validates_retries_and_resolves_aliases() {
        let (url, requests) = serve(|n, _, body| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let prompt = v["messages"][0]["content"].as_str().unwrap();
            if n == 0 {
                assert!(prompt.contains("[P1 context]"));
                assert!(prompt.contains("[P2 TARGET]") && prompt.contains("[P3 TARGET]"));
                assert!(prompt.contains("- vortram: Vortram; the old man"));
                claude_reply(serde_json::json!({
                    "segments": [
                        {"paragraph": 2, "kind": "dialogue", "speaker": "guard", "text": "\"Who goes there?\""},
                        {"paragraph": 2, "kind": "narration", "speaker": "narrator", "text": "asked the guard."},
                        // Dropped word: must be rejected and retried.
                        {"paragraph": 3, "kind": "dialogue", "speaker": "vortram", "text": "\"Only me,\""},
                        {"paragraph": 3, "kind": "narration", "speaker": "narrator", "text": "said the man."}
                    ],
                    "characters": [{"key": "guard", "display_name": "Guard", "aliases": ["the guard"]}]
                }))
            } else {
                assert!(prompt.contains("[P3 TARGET]") && !prompt.contains("[P2 TARGET]"));
                assert!(prompt.contains("[P2 context; speakers: guard]"));
                claude_reply(serde_json::json!({
                    "segments": [
                        {"paragraph": 3, "kind": "dialogue", "speaker": "the old man", "text": "\"Only me,\""},
                        {"paragraph": 3, "kind": "narration", "speaker": "narrator", "text": "said the old man."}
                    ],
                    "characters": []
                }))
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, _events) = test_ctx(&lib, &url, "http://unused");
        let (pid, cid) = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                db.upsert_character(pid, "vortram", "Vortram", &["the old man".into()])?;
                Ok((pid, db.import_chapter(pid, "One", STORY)?))
            })
            .unwrap();

        rt().block_on(segment_chapter(&ctx, cid, false)).unwrap();
        let reqs = requests.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2);
        let first: serde_json::Value = serde_json::from_str(&reqs[0].1).unwrap();
        assert_eq!(first["fallbacks"], "default");
        assert_eq!(first["output_config"]["format"]["type"], "json_schema");

        let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
        let got: Vec<(&str, &str)> = segs
            .iter()
            .map(|s| (s.speaker.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (NARRATOR, "The night was cold."),
                ("guard", "\"Who goes there?\""),
                (NARRATOR, "asked the guard."),
                ("vortram", "\"Only me,\""),
                (NARRATOR, "said the old man."),
            ]
        );
        let registry = ctx.db(|db| db.characters(pid)).unwrap();
        assert!(
            registry
                .iter()
                .any(|c| c.key == "guard" && c.aliases == vec!["the guard".to_string()])
        );
        assert_eq!(
            ctx.db(|db| db.chapter(cid)).unwrap().status,
            ChapterStatus::Segmented
        );
    }

    const VALE: &str = "\"Hold position,\" said Lieutenant Vale. Commander Voss looked at the countdown. Three minutes.\n\n***\n\nIt would not.";

    #[test]
    fn audio_tags_are_validated_and_spoken() {
        let (claude, claude_reqs) = serve(|_, _, body| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let prompt = v["messages"][0]["content"].as_str().unwrap();
            assert!(v["system"].as_str().unwrap().contains("audio tags"));
            assert!(prompt.contains("Density: rich"));
            assert!(prompt.contains("Director's notes: Keep Voss calm."));
            let id = |text: &str| -> i64 {
                let line = prompt.lines().find(|l| l.ends_with(text)).unwrap();
                line[2..line.find(' ').unwrap()].parse().unwrap()
            };
            claude_reply(serde_json::json!({"segments": [
                {"id": id("\"Hold position,\""), "performance": "\"[tense] Hold position,\""},
                {"id": id("Three minutes."), "performance": "said Lieutenant Vale. [thoughtful] Commander Voss looked at the countdown. [long pause] Three minutes."},
                // Rewrites a word: must be rejected.
                {"id": id("It would not."), "performance": "[short pause] It did not."}
            ]}))
        });
        let (eleven, eleven_reqs) = serve(|n, _, _| {
            (
                200,
                vec![("request-id".into(), format!("r{n}"))],
                vec![0u8; 480],
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, events) = test_ctx(&lib, &claude, &eleven);
        let cid = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vale".into(), "vv".into());
                p.settings.audio_tags.density = "rich".into();
                p.settings.audio_tags.direction = "Keep Voss calm.".into();
                p.settings.dialogue.enabled = false;
                db.update_project(&p)?;
                let cid = db.import_chapter(pid, "One", VALE)?;
                for par in db.paragraphs(cid)? {
                    let segs = segment::parse_paragraph(&par.text, lang::by_code("en"));
                    store_segments(db, &HashSet::new(), &par, &segs)?;
                }
                for s in db.chapter_segments(cid)? {
                    if s.kind == SegmentKind::Dialogue {
                        db.set_segment_speaker(s.id, "vale")?;
                    }
                }
                Ok(cid)
            })
            .unwrap();

        rt().block_on(direct_chapter(&ctx, cid, false)).unwrap();
        assert_eq!(claude_reqs.lock().unwrap().len(), 1);
        let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
        let perf: Vec<Option<&str>> = segs.iter().map(|s| s.performance.as_deref()).collect();
        assert_eq!(
            perf,
            vec![
                Some("\"[tense] Hold position,\""),
                Some(
                    "said Lieutenant Vale. [thoughtful] Commander Voss looked at the countdown. [long pause] Three minutes."
                ),
                None,
            ]
        );
        let finished = events.try_iter().find_map(|e| match e {
            Event::DirectionFinished {
                tagged, rejected, ..
            } => Some((tagged, rejected)),
            _ => None,
        });
        assert_eq!(finished, Some((2, 1)));

        // A tag-capable model receives the tagged text; the hash follows it.
        let ids: Vec<Id> = segs.iter().map(|s| s.id).collect();
        rt().block_on(generate(&ctx, ids, false)).unwrap();
        let sent: Vec<String> = eleven_reqs
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| {
                serde_json::from_str::<serde_json::Value>(b).unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(sent.contains(&"\"[tense] Hold position,\"".to_string()));
        assert!(sent.contains(&"It would not.".to_string()));
        // That fake server sends no character-cost header: credits are estimates.
        let takes = ctx.db(|db| db.takes(segs[0].id)).unwrap();
        assert!(!takes[0].credits_exact);
        let stats = ctx
            .db(|db| db.chapter_stats(cid, &db.project(db.chapter(cid)?.project_id)?))
            .unwrap();
        assert!(stats.credits_estimated);
        let narr: serde_json::Value = eleven_reqs
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| serde_json::from_str::<serde_json::Value>(b).unwrap())
            .find(|v| {
                v["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("said Lieutenant Vale.")
            })
            .unwrap();
        assert!(
            narr["text"]
                .as_str()
                .unwrap()
                .contains("[long pause] Three minutes.")
        );
        assert_eq!(
            narr["previous_text"], "\"Hold position,\"",
            "context stays plain text"
        );

        // Automatic runs leave already-tagged drafts alone.
        let before = claude_reqs.lock().unwrap().len();
        ctx.db(|db| {
            for s in db.chapter_segments(cid)? {
                db.set_segment_performance(s.id, Some(&format!("[softly] {}", s.text)))?;
            }
            Ok(())
        })
        .unwrap();
        rt().block_on(direct_chapter(&ctx, cid, true)).unwrap();
        assert_eq!(
            claude_reqs.lock().unwrap().len(),
            before,
            "nothing left to tag"
        );
    }

    #[test]
    fn segmentation_falls_back_to_parser_on_fatal_error() {
        let (url, requests) = serve(|_, _, _| {
            (
                400,
                vec![],
                b"{\"error\":{\"message\":\"bad request\"}}".to_vec(),
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, events) = test_ctx(&lib, &url, "http://unused");
        let cid = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                db.import_chapter(pid, "One", STORY)
            })
            .unwrap();
        rt().block_on(segment_chapter(&ctx, cid, false)).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
        assert_eq!(segs.len(), 5);
        assert_eq!(
            segs.iter().filter(|s| s.speaker == UNKNOWN_SPEAKER).count(),
            2
        );
        let finished = events.try_iter().find_map(|e| match e {
            Event::SegmentationFinished {
                fallback_paragraphs,
                ..
            } => Some(fallback_paragraphs),
            _ => None,
        });
        assert_eq!(finished, Some(2));
    }
}
