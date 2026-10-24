//! The take checker: ONE dedicated thread that checks takes one at a time
//! from a queue of take ids. It reads the take, checks its length, and (when
//! a Whisper model is installed) has it transcribed and compares the words
//! with the segment's text. The transcription runs on a second thread at the
//! lowest practical priority; database work stays on the first one. It has
//! its own database connection, so it never holds up generation or the UI,
//! and it only stores a result: it never changes a segment or spends
//! credits.
//!
//! Also here: the take-check commands and the model download job.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow};

use super::{Ctx, Emitter, Event};
use crate::audio;
use crate::check::{
    self, AsrError, CheckCommand, CheckConfig, CheckStatus, Expected, Loader, TakeCheck,
    Transcriber, Words, models, resample,
};
use crate::db::{Db, Library};
use crate::lang;
use crate::model::*;

/// The loaded model is dropped after this long without work (it holds
/// 0.35–2 GB).
const IDLE_UNLOAD: Duration = Duration::from_secs(300);

/// Handle to the checker, shared by every clone of the worker context.
/// Until [`start`] runs (tests that do not need it), jobs only queue up.
#[derive(Clone, Default)]
pub(super) struct Checker {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// Aborts the take being transcribed (Stop).
    abort: AtomicBool,
    loader: Mutex<Option<Arc<Loader>>>,
    downloading: Mutex<Option<String>>,
    cancel_download: AtomicBool,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    queued: HashSet<Id>,
    config: CheckConfig,
    /// Drop the loaded model before the next take (a download finished, the
    /// file was deleted, or another model was chosen).
    reload: bool,
    /// Progress since the queue was last empty.
    done: usize,
    total: usize,
    /// Bumped by Stop: a job taken before it no longer counts as progress.
    epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Job {
    take_id: Id,
    /// Check again even if a result exists (Re-check).
    force: bool,
}

impl Inner {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn loader(&self) -> Option<Arc<Loader>> {
        self.loader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The model words are checked with right now, or "" when only the length
/// can be checked (no Whisper in this build, unknown or missing model).
fn model_key(lib: &Library, has_loader: bool, cfg: &CheckConfig) -> String {
    match models::info(&cfg.model) {
        Some(m) if has_loader && models::installed(lib, m) != models::Installed::No => {
            cfg.model.clone()
        }
        _ => String::new(),
    }
}

/// Why words cannot be checked with `cfg` (for the result's note).
fn not_checked_reason(has_loader: bool, cfg: &CheckConfig) -> String {
    if !has_loader {
        "this build has no Whisper".into()
    } else if models::info(&cfg.model).is_none() {
        format!("unknown model “{}”", cfg.model)
    } else {
        "no Whisper model is installed (Settings → Take checking)".into()
    }
}

/// Progress events are sent while the queue is locked (a channel send never
/// blocks), so they arrive in the order of the queue changes.
fn enqueue(ctx: &Ctx, ids: impl IntoIterator<Item = Id>, force: bool) -> usize {
    let inner = &ctx.checker.inner;
    let added = {
        let mut q = inner.queue();
        let mut added = 0;
        for take_id in ids {
            let job = Job { take_id, force };
            if q.queued.contains(&take_id) {
                if force {
                    // Re-check: move it to the front.
                    q.jobs.retain(|j| j.take_id != take_id);
                    q.jobs.push_front(job);
                }
                continue;
            }
            q.queued.insert(take_id);
            if force {
                q.jobs.push_front(job);
            } else {
                q.jobs.push_back(job);
            }
            q.total += 1;
            added += 1;
        }
        if added > 0 || force {
            ctx.out.emit(Event::CheckProgress {
                done: q.done,
                total: q.total,
            });
        }
        added
    };
    if added > 0 || force {
        inner.wake.notify_all();
    }
    added
}

/// After a take row is stored (new or cache hit): queue it when automatic
/// checking is on. A take that already has a result is skipped by the
/// checker.
pub(super) fn new_take(ctx: &Ctx, take_id: Id) {
    if ctx.checker.inner.queue().config.enabled {
        enqueue(ctx, [take_id], false);
    }
}

pub(super) async fn run_command(ctx: &Ctx, cmd: CheckCommand) -> Result<()> {
    let inner = &ctx.checker.inner;
    match cmd {
        CheckCommand::Config(cfg) => {
            let mut q = inner.queue();
            if q.config.model != cfg.model {
                q.reload = true;
            }
            q.config = cfg;
            Ok(())
        }
        CheckCommand::Chapter { chapter_id } => {
            let cfg = inner.queue().config.clone();
            let key = model_key(&ctx.lib, inner.loader().is_some(), &cfg);
            let ids = ctx.db(|db| db.takes_to_check(chapter_id, &key))?;
            let n = enqueue(ctx, ids, false);
            let what = if key.is_empty() {
                "length check only: no Whisper model".to_string()
            } else {
                format!("Whisper {key}")
            };
            ctx.out.emit(Event::Info(if n == 0 {
                "Take check: every current take of this chapter is already checked (or queued)."
                    .to_string()
            } else {
                let takes = if n == 1 { "take" } else { "takes" };
                format!("Take check: {n} {takes} queued ({what}).")
            }));
            Ok(())
        }
        CheckCommand::Segment { segment_id } => {
            let take = ctx
                .db(|db| db.segment(segment_id))?
                .current_take_id
                .ok_or_else(|| anyhow!("this segment has no audio to check"))?;
            enqueue(ctx, [take], true);
            Ok(())
        }
        CheckCommand::Stop => {
            {
                let mut q = inner.queue();
                q.jobs.clear();
                q.queued.clear();
                q.done = 0;
                q.total = 0;
                q.epoch += 1;
                // Aborts the take being checked, not one queued later.
                inner.abort.store(true, Ordering::SeqCst);
                ctx.out.emit(Event::CheckProgress { done: 0, total: 0 });
            }
            ctx.out.emit(Event::Info("Take check stopped.".into()));
            Ok(())
        }
        CheckCommand::Download { model } => download(ctx, &model).await,
        CheckCommand::CancelDownload => {
            inner.cancel_download.store(true, Ordering::SeqCst);
            Ok(())
        }
        CheckCommand::Delete { model } => {
            let m = models::info(&model).ok_or_else(|| anyhow!("unknown model {model}"))?;
            if inner
                .downloading
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
                == Some(m.name)
            {
                return Err(anyhow!(
                    "{model} is being downloaded; cancel the download first"
                ));
            }
            inner.queue().reload = true;
            models::delete(&ctx.lib, m)?;
            ctx.out
                .emit(Event::Info(format!("Deleted the Whisper model {model}.")));
            Ok(())
        }
    }
}

async fn download(ctx: &Ctx, model: &str) -> Result<()> {
    let inner = &ctx.checker.inner;
    // Every request ends with ModelDownloadFinished (the UI shows it as
    // running from the click on).
    let refuse = |error: String| {
        ctx.out.emit(Event::ModelDownloadFinished {
            model: model.to_string(),
            error: Some(error),
        });
        Ok(())
    };
    let Some(m) = models::info(model) else {
        return refuse(format!("unknown model {model}"));
    };
    {
        let mut d = inner.downloading.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(other) = d.as_deref() {
            return refuse(format!("{other} is already being downloaded"));
        }
        *d = Some(m.name.to_string());
    }
    inner.cancel_download.store(false, Ordering::SeqCst);
    let out = ctx.out.clone();
    let name = m.name.to_string();
    let result = models::download(
        &ctx.http,
        models::HF_BASE,
        &ctx.lib,
        m,
        &inner.cancel_download,
        |received, total| {
            out.emit(Event::ModelDownload {
                model: name.clone(),
                received,
                total,
            })
        },
    )
    .await;
    *inner.downloading.lock().unwrap_or_else(|e| e.into_inner()) = None;
    match result {
        Ok(d) => {
            inner.queue().reload = true;
            ctx.out.emit(Event::ModelDownloadFinished {
                model: name,
                error: None,
            });
            ctx.out.emit(Event::Info(format!(
                "Downloaded the Whisper model {} ({} MiB{}).",
                m.name,
                d.bytes >> 20,
                if d.verified {
                    ", checksum verified"
                } else {
                    "; no checksum was available to verify it"
                }
            )));
        }
        Err(e) => {
            ctx.out.emit(Event::ModelDownloadFinished {
                model: name,
                error: Some(format!("{e:#}")),
            });
        }
    }
    Ok(())
}

/// Start the checker. `loader` is `None` when built without Whisper (only
/// the length check runs then).
///
/// Two threads: "take-check" reads takes, runs the cheap checks and stores
/// results at normal priority (it mostly waits), and "take-check-asr" loads
/// the model and transcribes at the lowest priority. Database work never runs
/// on the idle-priority thread: starved by a busy CPU while holding SQLite's
/// write lock, it could make generation's own writes (storing a paid take)
/// time out.
pub(super) fn start(ctx: &Ctx, loader: Option<Arc<Loader>>) {
    let inner = ctx.checker.inner.clone();
    *inner.loader.lock().unwrap_or_else(|e| e.into_inner()) = loader;
    let (lib, out) = (ctx.lib.clone(), ctx.out.clone());
    let spawned = std::thread::Builder::new()
        .name("take-check".into())
        .spawn(move || thread_main(inner, lib, out));
    if let Err(e) = spawned {
        log::warn!("could not start the take checker: {e}");
    }
}

/// The loaded model.
struct Loaded {
    model: String,
    transcriber: Box<dyn Transcriber>,
}

/// Model state of the transcription thread.
struct Backend {
    loader: Option<Arc<Loader>>,
    loaded: Option<Loaded>,
    /// A model that failed to load, and why; not retried until reloaded.
    failed: Option<(String, String)>,
}

impl Backend {
    fn transcriber(&mut self, lib: &Library, model: &str) -> Result<&mut dyn Transcriber, String> {
        if self.loaded.as_ref().is_some_and(|l| l.model != model) {
            self.loaded = None;
        }
        if self.loaded.is_none() {
            if let Some((m, e)) = &self.failed
                && m == model
            {
                return Err(e.clone());
            }
            let loader = self.loader.clone().ok_or("this build has no Whisper")?;
            let info = models::info(model).ok_or_else(|| format!("unknown model {model}"))?;
            let path = models::model_path(lib, info);
            let loaded = models::verify_before_load(&path, info)
                .and_then(|()| loader(&path))
                .map_err(|e| format!("{e:#}"));
            match loaded {
                Ok(t) => {
                    log::info!("take check: loaded Whisper {model}");
                    self.loaded = Some(Loaded {
                        model: model.to_string(),
                        transcriber: t,
                    });
                }
                Err(e) => {
                    log::warn!("take check: {e}");
                    self.failed = Some((model.to_string(), e.clone()));
                    return Err(e);
                }
            }
        }
        Ok(self.loaded.as_mut().expect("loaded").transcriber.as_mut())
    }
}

/// One take for the transcription thread.
struct AsrJob {
    model: String,
    /// Drop the loaded model (and a remembered load failure) first.
    reload: bool,
    /// The take as stored; resampled on the low-priority thread.
    pcm: audio::Pcm,
    lang: String,
    threads: usize,
}

enum AsrReply {
    Heard(check::Transcript),
    /// The model is English-only and the project is not.
    EnglishOnly,
    Failed(String),
    Cancelled,
}

/// The low-priority transcription thread, (re)started on first use.
struct Asr {
    inner: Arc<Inner>,
    lib: Library,
    tx: Option<std::sync::mpsc::Sender<(AsrJob, std::sync::mpsc::Sender<AsrReply>)>>,
}

impl Asr {
    /// Transcribe on the low-priority thread and wait for the result.
    fn run(&mut self, job: AsrJob) -> AsrReply {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            let (inner, lib) = (self.inner.clone(), self.lib.clone());
            let spawned = std::thread::Builder::new()
                .name("take-check-asr".into())
                .spawn(move || asr_main(inner, lib, rx));
            if let Err(e) = spawned {
                return AsrReply::Failed(format!("could not start the Whisper thread: {e}"));
            }
            self.tx = Some(tx);
        }
        if let Some(tx) = &self.tx
            && tx.send((job, reply_tx)).is_ok()
            && let Ok(r) = reply_rx.recv()
        {
            return r;
        }
        // The thread is gone (a panic): start a new one next time.
        self.tx = None;
        AsrReply::Failed("the Whisper thread stopped unexpectedly".into())
    }
}

fn asr_main(
    inner: Arc<Inner>,
    lib: Library,
    rx: std::sync::mpsc::Receiver<(AsrJob, std::sync::mpsc::Sender<AsrReply>)>,
) {
    check::background_priority();
    let mut backend = Backend {
        loader: inner.loader(),
        loaded: None,
        failed: None,
    };
    loop {
        let (job, reply_to) = match rx.recv_timeout(IDLE_UNLOAD) {
            Ok(j) => j,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if backend.loaded.take().is_some() {
                    log::info!("take check: idle, model unloaded");
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if job.reload {
            backend.loaded = None;
            backend.failed = None;
        }
        let reply = match backend.transcriber(&lib, &job.model) {
            Err(e) => AsrReply::Failed(e),
            Ok(t) if !t.is_multilingual() && job.lang != "en" => AsrReply::EnglishOnly,
            Ok(t) => {
                let pcm16k = resample::pad_for_whisper(resample::to_whisper_input(
                    &job.pcm.samples,
                    job.pcm.rate,
                ));
                match t.transcribe(&pcm16k, &job.lang, job.threads.max(1), &inner.abort) {
                    Ok(tr) => AsrReply::Heard(tr),
                    Err(AsrError::Cancelled) => AsrReply::Cancelled,
                    Err(AsrError::Other(e)) => AsrReply::Failed(format!("{e:#}")),
                }
            }
        };
        let _ = reply_to.send(reply);
    }
}

fn thread_main(inner: Arc<Inner>, lib: Library, out: Emitter) {
    let mut db: Option<Db> = None;
    let has_loader = inner.loader().is_some();
    let mut asr = Asr {
        inner: inner.clone(),
        lib: lib.clone(),
        tx: None,
    };
    // A reload requested while only lengths were checked waits for the next
    // transcription.
    let mut reload = false;
    loop {
        let (job, cfg, epoch) = {
            let mut q = inner.queue();
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    q.queued.remove(&job.take_id);
                    // Under the lock: a Stop after this aborts this job.
                    inner.abort.store(false, Ordering::SeqCst);
                    reload |= std::mem::take(&mut q.reload);
                    break (job, q.config.clone(), q.epoch);
                }
                q = inner.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        if db.is_none() {
            match lib.open() {
                Ok(d) => db = Some(d),
                Err(e) => log::warn!("take check: opening the library: {e:#}"),
            }
        }
        let checked = match &db {
            Some(db) => {
                let mut env = Env {
                    db,
                    lib: &lib,
                    has_loader,
                    asr: &mut asr,
                    reload: &mut reload,
                };
                check_take(&mut env, job, &cfg).unwrap_or_else(|e| {
                    log::warn!("take check of take {}: {e:#}", job.take_id);
                    None
                })
            }
            None => None,
        };
        if let Some(segment_id) = checked {
            out.emit(Event::TakeChecked { segment_id });
        }
        let mut q = inner.queue();
        if q.epoch == epoch {
            q.done = (q.done + 1).min(q.total);
        }
        if q.jobs.is_empty() {
            q.done = 0;
            q.total = 0;
        }
        out.emit(Event::CheckProgress {
            done: q.done,
            total: q.total,
        });
    }
}

/// What `check_take` works with.
struct Env<'a> {
    db: &'a Db,
    lib: &'a Library,
    has_loader: bool,
    asr: &'a mut Asr,
    reload: &'a mut bool,
}

/// Check one take and store the result. Returns its segment when a result
/// was stored (nothing is stored when the take is gone, already checked, or
/// the check was aborted).
fn check_take(env: &mut Env, job: Job, cfg: &CheckConfig) -> Result<Option<Id>> {
    let db = env.db;
    let Ok(take) = db.take(job.take_id) else {
        return Ok(None);
    };
    let key = model_key(env.lib, env.has_loader, cfg);
    if !job.force {
        if !check::needs_check(db.take_check(take.id)?.as_ref(), &key) {
            return Ok(None);
        }
        // A cache hit plays the same file with the same text: reuse its check.
        if let Some(shared) = db.shared_take_check(take.id, &key)? {
            db.set_take_check(&TakeCheck {
                take_id: take.id,
                ..shared
            })?;
            return Ok(Some(take.segment_id));
        }
    }
    let seg = db.segment(take.segment_id)?;
    let project = db.project(db.chapter(db.segment_chapter_id(seg.id)?)?.project_id)?;
    let store = |status, transcript, wer, issues, model: &str| -> Result<Option<Id>> {
        db.set_take_check(&TakeCheck {
            take_id: take.id,
            status,
            transcript,
            wer,
            issues,
            model: model.to_string(),
            checked_at: String::new(),
        })?;
        Ok(Some(seg.id))
    };
    // The generated audio as it is, untrimmed: the check judges the take's
    // file (shared by cache hits), not the editable trim.
    let path: PathBuf = env.lib.audio_path(&take.audio_path);
    let pcm = match audio::read_wav(&path) {
        Ok(p) => p,
        Err(e) => {
            return store(
                CheckStatus::Error,
                None,
                None,
                vec![check::Issue {
                    kind: check::IssueKind::Error,
                    flag: false,
                    message: format!("The audio could not be read: {e:#}"),
                }],
                "",
            );
        }
    };
    let speed = seg
        .settings_override
        .as_ref()
        .unwrap_or(&project.default_voice_settings)
        .speed;
    let expected = Expected {
        text: &seg.text,
        performance: seg
            .performance
            .as_deref()
            .filter(|_| lang::model_supports_tags(&project.tts_model)),
        lang: &project.language,
    };
    let length = check::length_check(
        seg.text.chars().count(),
        expected.tag_count(),
        speed,
        check::speech_ms(&pcm),
    );
    let words = if key.is_empty() {
        Words::NotChecked(not_checked_reason(env.has_loader, cfg))
    } else {
        let reply = env.asr.run(AsrJob {
            model: key.clone(),
            reload: std::mem::take(env.reload),
            pcm,
            lang: project.language.clone(),
            threads: cfg.threads,
        });
        match reply {
            AsrReply::Heard(tr) => Words::Heard(tr),
            AsrReply::EnglishOnly => Words::NotChecked(format!(
                "the model is English-only and the project is in {}",
                lang::by_code(&project.language).name
            )),
            AsrReply::Failed(e) => Words::Failed(e),
            AsrReply::Cancelled => return Ok(None),
        }
    };
    // The model is recorded only when it actually heard the take, so a
    // length-only or failed check is redone once Whisper works.
    let model_used = if matches!(words, Words::Heard(_)) {
        key
    } else {
        String::new()
    };
    let ev = check::evaluate(
        &expected,
        &length,
        words,
        &check::text::Thresholds::default(),
    );
    store(ev.status, ev.transcript, ev.wer, ev.issues, &model_used)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::Receiver;
    use std::time::Instant;

    use sha2::Digest;

    use super::super::generate;
    use super::super::tests::{import, rt, serve, test_ctx};
    use super::*;
    use crate::check::{HeardSegment, Transcript};

    /// The texts of `import`'s chapter, in reading order.
    const TEXTS: [&str; 4] = [
        "\"It is,\"",
        "started Vortram,",
        "\"truly magnificent!\"",
        "The end.",
    ];
    const RATE: usize = 24_000;

    /// Fake ElevenLabs audio for segment `k`: 200 ms silence, a tone of
    /// 1000 + 200k ms, 300 ms silence. Its length identifies the segment.
    fn take_audio(k: usize) -> Vec<u8> {
        let ms = |ms: usize| RATE * ms / 1000;
        let mut v = vec![0i16; ms(200)];
        v.extend((0..ms(1000 + 200 * k)).map(|i| ((i as f32 * 0.05).sin() * 6000.0) as i16));
        v.extend(vec![0i16; ms(300)]);
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn eleven() -> String {
        serve(|n, _, body| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let k = TEXTS.iter().position(|t| *t == v["text"]).unwrap();
            (
                200,
                vec![
                    ("request-id".into(), format!("r{n}")),
                    ("character-cost".into(), "5".into()),
                ],
                take_audio(k),
            )
        })
        .0
    }

    /// Hears "segment k" from the audio's length (see `take_audio`).
    struct Fake {
        heard: Arc<dyn Fn(usize) -> String + Send + Sync>,
        calls: Arc<AtomicUsize>,
    }

    impl Transcriber for Fake {
        fn is_multilingual(&self) -> bool {
            true
        }
        fn transcribe(
            &mut self,
            pcm16k: &[f32],
            lang: &str,
            threads: usize,
            cancel: &AtomicBool,
        ) -> Result<Transcript, AsrError> {
            assert_eq!(lang, "en");
            assert!(threads >= 1);
            if cancel.load(Ordering::SeqCst) {
                return Err(AsrError::Cancelled);
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let ms = pcm16k.len() / 16;
            let k = (ms.saturating_sub(1500) + 100) / 200;
            Ok(Transcript {
                segments: vec![HeardSegment {
                    text: (self.heard)(k),
                    no_speech_prob: 0.0,
                }],
            })
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        lib: Library,
        ctx: Ctx,
        events: Receiver<Event>,
        calls: Arc<AtomicUsize>,
        loads: Arc<AtomicUsize>,
        cid: Id,
        segs: Vec<Id>,
    }

    /// A project whose chapter `import` made, a fake transcriber that gets
    /// segment 2 wrong, and (optionally) an installed "small" model.
    fn setup(install: bool) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, events) = test_ctx(&lib, "http://unused", &eleven());
        let (cid, segs) = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = "pcm_24000".into();
                db.update_project(&p)?;
                let cid = import(db, pid);
                let segs = db.chapter_segments(cid)?.iter().map(|s| s.id).collect();
                Ok((cid, segs))
            })
            .unwrap();
        if install {
            install_model(&lib);
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let loads = Arc::new(AtomicUsize::new(0));
        let (c2, l2) = (calls.clone(), loads.clone());
        let loader: Arc<Loader> = Arc::new(move |path: &std::path::Path| {
            assert!(path.ends_with("models/ggml-small.bin"));
            l2.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Fake {
                heard: Arc::new(|k| match k {
                    0 => " It is.".into(),
                    1 => " started Vortram.".into(),
                    2 => " Thank you so much for coming.".into(),
                    _ => " The end.".into(),
                }),
                calls: c2.clone(),
            }) as Box<dyn Transcriber>)
        });
        start(&ctx, Some(loader));
        Setup {
            _dir: dir,
            lib,
            ctx,
            events,
            calls,
            loads,
            cid,
            segs,
        }
    }

    fn install_model(lib: &Library) {
        let path = models::model_path(lib, models::info("small").unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"fake weights").unwrap();
        models::mark_verified_for_tests(&path);
    }

    /// Wait until `n` TakeChecked events arrived and the queue is idle.
    fn wait_checked(events: &Receiver<Event>, n: usize) -> Vec<Id> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut checked = Vec::new();
        let mut idle = false;
        while checked.len() < n || !idle {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                Ok(Event::TakeChecked { segment_id }) => {
                    checked.push(segment_id);
                    idle = false;
                }
                Ok(Event::CheckProgress { total, .. }) => idle = total == 0,
                Ok(_) => {}
                Err(_) => panic!("timed out: {} of {n} checked", checked.len()),
            }
        }
        checked
    }

    fn checks(s: &Setup) -> HashMap<Id, TakeCheck> {
        s.ctx.db(|db| db.chapter_checks(s.cid)).unwrap()
    }

    #[test]
    fn new_takes_are_checked_in_the_background() {
        let s = setup(true);
        rt().block_on(generate(&s.ctx, s.segs.clone(), false))
            .unwrap();
        let mut checked = wait_checked(&s.events, 4);
        checked.sort();
        assert_eq!(checked, s.segs);
        assert_eq!(s.calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            s.loads.load(Ordering::SeqCst),
            1,
            "the model is loaded once"
        );
        let by_seg = checks(&s);
        let status: Vec<CheckStatus> = s.segs.iter().map(|id| by_seg[id].status).collect();
        assert_eq!(
            status,
            [
                CheckStatus::Ok,
                CheckStatus::Ok,
                CheckStatus::Flagged,
                CheckStatus::Ok
            ]
        );
        let flagged = &by_seg[&s.segs[2]];
        assert_eq!(flagged.model, "small");
        assert_eq!(
            flagged.transcript.as_deref(),
            Some("Thank you so much for coming.")
        );
        assert!(
            flagged.reason().unwrap().contains("short line"),
            "{:?}",
            flagged.issues
        );
        // The length check ran too (1.0 s of tone for 8 characters is fine).
        let ok = &by_seg[&s.segs[0]];
        assert!(
            ok.issues
                .iter()
                .any(|i| i.kind == check::IssueKind::Length && !i.flag)
        );
        // Generation never waited for the check, and statuses are untouched.
        let segs = s.ctx.db(|db| db.chapter_segments(s.cid)).unwrap();
        assert!(segs.iter().all(|x| x.status == SegmentStatus::Generated));

        // Same lines in another chapter: cache hits reuse the checks without
        // transcribing again.
        let cid2 = s
            .ctx
            .db(|db| Ok(import(db, db.chapter(s.cid)?.project_id)))
            .unwrap();
        let ids2: Vec<Id> = s
            .ctx
            .db(|db| db.chapter_segments(cid2))
            .unwrap()
            .iter()
            .map(|x| x.id)
            .collect();
        rt().block_on(generate(&s.ctx, ids2.clone(), false))
            .unwrap();
        wait_checked(&s.events, 4);
        assert_eq!(s.calls.load(Ordering::SeqCst), 4, "no new transcriptions");
        let by_seg2 = s.ctx.db(|db| db.chapter_checks(cid2)).unwrap();
        assert_eq!(by_seg2[&ids2[2]].status, CheckStatus::Flagged);
        assert_ne!(by_seg2[&ids2[2]].take_id, by_seg[&s.segs[2]].take_id);
    }

    #[test]
    fn without_a_model_only_the_length_is_checked() {
        let s = setup(false);
        rt().block_on(generate(&s.ctx, s.segs.clone(), false))
            .unwrap();
        wait_checked(&s.events, 4);
        assert_eq!(s.loads.load(Ordering::SeqCst), 0);
        let by_seg = checks(&s);
        for id in &s.segs {
            let c = &by_seg[id];
            assert_eq!(
                (c.status, c.model.as_str(), c.transcript.as_ref()),
                (CheckStatus::Skipped, "", None)
            );
            assert!(
                c.issues[0].message.contains("no Whisper model"),
                "{:?}",
                c.issues
            );
        }
        // "Check chapter" with nothing new to use queues nothing.
        rt().block_on(run_command(
            &s.ctx,
            CheckCommand::Chapter { chapter_id: s.cid },
        ))
        .unwrap();
        assert!(s.ctx.checker.inner.queue().jobs.is_empty());
        // Once a model is installed, "Check chapter" upgrades them.
        install_model(&s.lib);
        rt().block_on(run_command(
            &s.ctx,
            CheckCommand::Chapter { chapter_id: s.cid },
        ))
        .unwrap();
        wait_checked(&s.events, 4);
        let by_seg = checks(&s);
        assert_eq!(by_seg[&s.segs[2]].status, CheckStatus::Flagged);
        assert_eq!(by_seg[&s.segs[0]].model, "small");
        assert_eq!(s.calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn dismiss_recheck_disable_and_stop() {
        let s = setup(true);
        // Automatic checking off: new takes are not queued...
        let off = CheckConfig {
            enabled: false,
            ..CheckConfig::default()
        };
        rt().block_on(run_command(&s.ctx, CheckCommand::Config(off)))
            .unwrap();
        rt().block_on(generate(&s.ctx, s.segs.clone(), false))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(checks(&s).is_empty());
        // ...but "Check chapter" works.
        rt().block_on(run_command(
            &s.ctx,
            CheckCommand::Chapter { chapter_id: s.cid },
        ))
        .unwrap();
        wait_checked(&s.events, 4);
        let flagged = checks(&s)[&s.segs[2]].clone();
        assert_eq!(flagged.status, CheckStatus::Flagged);
        // Dismissed flags survive "Check chapter".
        assert!(
            s.ctx
                .db(|db| db.dismiss_take_check(flagged.take_id))
                .unwrap()
        );
        rt().block_on(run_command(
            &s.ctx,
            CheckCommand::Chapter { chapter_id: s.cid },
        ))
        .unwrap();
        assert!(s.ctx.checker.inner.queue().jobs.is_empty());
        assert_eq!(checks(&s)[&s.segs[2]].status, CheckStatus::Dismissed);
        // Re-check transcribes again and replaces the dismissal.
        rt().block_on(run_command(
            &s.ctx,
            CheckCommand::Segment {
                segment_id: s.segs[2],
            },
        ))
        .unwrap();
        wait_checked(&s.events, 1);
        assert_eq!(checks(&s)[&s.segs[2]].status, CheckStatus::Flagged);
        assert_eq!(s.calls.load(Ordering::SeqCst), 5);
        // Stop empties the queue.
        {
            let mut q = s.ctx.checker.inner.queue();
            q.config.enabled = true;
            // Hold the lock so nothing is processed while queueing.
            q.jobs.extend(s.segs.iter().map(|&take_id| Job {
                take_id,
                force: true,
            }));
        }
        rt().block_on(run_command(&s.ctx, CheckCommand::Stop))
            .unwrap();
        let q = s.ctx.checker.inner.queue();
        assert!(q.jobs.is_empty() && q.queued.is_empty() && q.total == 0);
    }

    #[test]
    fn a_model_that_fails_to_load_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, events) = test_ctx(&lib, "http://unused", &eleven());
        let segs = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = "pcm_24000".into();
                db.update_project(&p)?;
                let cid = import(db, pid);
                Ok(db
                    .chapter_segments(cid)?
                    .iter()
                    .map(|s| s.id)
                    .collect::<Vec<_>>())
            })
            .unwrap();
        install_model(&lib);
        let loads = Arc::new(AtomicUsize::new(0));
        let l2 = loads.clone();
        start(
            &ctx,
            Some(Arc::new(move |_: &std::path::Path| {
                l2.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("bad magic"))
            })),
        );
        rt().block_on(generate(&ctx, segs.clone(), false)).unwrap();
        wait_checked(&events, 4);
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "a failed load is not retried per take"
        );
        let c = ctx
            .db(|db| db.take_check(db.segment(segs[0])?.current_take_id.unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(c.status, CheckStatus::Error);
        assert!(c.issues.iter().any(|i| i.message.contains("bad magic")));
        assert!(c.issues.iter().any(|i| i.kind == check::IssueKind::Length));
        assert_eq!(
            c.model, "",
            "Whisper heard nothing, so no model is recorded"
        );
        // The take itself is untouched.
        assert_eq!(
            ctx.db(|db| db.segment(segs[0])).unwrap().status,
            SegmentStatus::Generated
        );
        // A take whose length flags while Whisper fails is still redone by
        // "Check chapter" once Whisper works (it was stored as "flagged").
        let take = ctx.db(|db| db.take(c.take_id)).unwrap();
        crate::audio::write_wav(
            &lib.audio_path(&take.audio_path),
            &crate::audio::Pcm {
                rate: RATE as u32,
                samples: vec![0; RATE],
            },
        )
        .unwrap();
        rt().block_on(run_command(
            &ctx,
            CheckCommand::Segment {
                segment_id: segs[0],
            },
        ))
        .unwrap();
        wait_checked(&events, 1);
        let c = ctx.db(|db| db.take_check(take.id)).unwrap().unwrap();
        assert_eq!((c.status, c.model.as_str()), (CheckStatus::Flagged, ""));
        assert_eq!(c.reason(), Some("The take is silent."));
        let cid = ctx.db(|db| db.segment_chapter_id(segs[0])).unwrap();
        assert!(
            ctx.db(|db| db.takes_to_check(cid, "small"))
                .unwrap()
                .contains(&take.id)
        );
    }

    /// The scheduling policy of the calling thread (`/proc`).
    #[cfg(target_os = "linux")]
    fn policy_of(task: &str) -> u32 {
        let s = std::fs::read_to_string(format!("/proc/self/task/{task}/stat")).unwrap();
        let f: Vec<&str> = s[s.rfind(')').unwrap() + 2..].split_whitespace().collect();
        f[41 - 3].parse().unwrap()
    }

    #[cfg(target_os = "linux")]
    fn my_policy() -> u32 {
        policy_of(&unsafe { libc::gettid() }.to_string())
    }

    /// Blocks in `transcribe` while `hold` is set, then returns Cancelled if
    /// the cancel flag is set; records the scheduling policy it runs at.
    struct Slow {
        hold: Arc<AtomicBool>,
        started: Arc<AtomicUsize>,
        policies: Arc<Mutex<Vec<u32>>>,
    }

    impl Transcriber for Slow {
        fn is_multilingual(&self) -> bool {
            true
        }
        fn transcribe(
            &mut self,
            _pcm16k: &[f32],
            _lang: &str,
            _threads: usize,
            cancel: &AtomicBool,
        ) -> Result<Transcript, AsrError> {
            #[cfg(target_os = "linux")]
            self.policies.lock().unwrap().push(my_policy());
            self.started.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(20);
            while self.hold.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if cancel.load(Ordering::SeqCst) {
                return Err(AsrError::Cancelled);
            }
            Ok(Transcript {
                segments: vec![HeardSegment {
                    text: " It is.".into(),
                    no_speech_prob: 0.0,
                }],
            })
        }
    }

    #[test]
    fn stop_aborts_the_running_take_and_only_whisper_runs_at_idle_priority() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, events) = test_ctx(&lib, "http://unused", &eleven());
        let off = CheckConfig {
            enabled: false,
            ..CheckConfig::default()
        };
        rt().block_on(run_command(&ctx, CheckCommand::Config(off)))
            .unwrap();
        let (cid, segs) = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = "pcm_24000".into();
                db.update_project(&p)?;
                let cid = import(db, pid);
                let segs: Vec<Id> = db.chapter_segments(cid)?.iter().map(|s| s.id).collect();
                Ok((cid, segs))
            })
            .unwrap();
        rt().block_on(generate(&ctx, segs.clone(), false)).unwrap();
        install_model(&lib);
        let hold = Arc::new(AtomicBool::new(true));
        let started = Arc::new(AtomicUsize::new(0));
        let policies = Arc::new(Mutex::new(Vec::new()));
        let (h, st, pol) = (hold.clone(), started.clone(), policies.clone());
        start(
            &ctx,
            Some(Arc::new(move |_: &std::path::Path| {
                #[cfg(target_os = "linux")]
                pol.lock().unwrap().push(my_policy());
                Ok(Box::new(Slow {
                    hold: h.clone(),
                    started: st.clone(),
                    policies: pol.clone(),
                }) as Box<dyn Transcriber>)
            })),
        );
        rt().block_on(run_command(&ctx, CheckCommand::Chapter { chapter_id: cid }))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while started.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "transcription never started");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The checker's database thread is not at idle priority; Whisper is.
        #[cfg(target_os = "linux")]
        {
            let mut db_threads = 0;
            for t in std::fs::read_dir("/proc/self/task").unwrap() {
                let t = t.unwrap().file_name().into_string().unwrap();
                let Ok(comm) = std::fs::read_to_string(format!("/proc/self/task/{t}/comm")) else {
                    continue;
                };
                if comm.trim() == "take-check" {
                    db_threads += 1;
                    assert_eq!(policy_of(&t), libc::SCHED_OTHER as u32);
                }
            }
            assert!(db_threads >= 1);
            let p = policies.lock().unwrap().clone();
            assert_eq!(p.len(), 2, "load and transcribe: {p:?}");
            assert!(p.iter().all(|&x| x == libc::SCHED_IDLE as u32), "{p:?}");
        }
        // Stop aborts the take being transcribed (segment 0's), and a
        // Re-check queued meanwhile is not aborted by it.
        rt().block_on(run_command(&ctx, CheckCommand::Stop))
            .unwrap();
        rt().block_on(run_command(
            &ctx,
            CheckCommand::Segment {
                segment_id: segs[1],
            },
        ))
        .unwrap();
        hold.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut checked = Vec::new();
        loop {
            match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Event::TakeChecked { segment_id }) => checked.push(segment_id),
                Ok(Event::CheckProgress { done, total }) => {
                    if checked.is_empty() {
                        // The aborted take is not progress of the new queue.
                        assert!(
                            total == 0 || done < total,
                            "{done}/{total} before any result"
                        );
                    } else if total == 0 {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => panic!("the Re-check never finished"),
            }
        }
        assert_eq!(checked, vec![segs[1]]);
        assert_eq!(started.load(Ordering::SeqCst), 2, "the queue was emptied");
        let by_seg = ctx.db(|db| db.chapter_checks(cid)).unwrap();
        assert!(
            !by_seg.contains_key(&segs[0]),
            "an aborted check was stored"
        );
        assert_eq!(by_seg[&segs[1]].model, "small");
    }

    // ---- model download -------------------------------------------------

    fn payload() -> Vec<u8> {
        (0..300_000u32).map(|i| (i % 251) as u8).collect()
    }

    /// A fake Hugging Face: the tree listing (with `sha256`) and the file.
    fn hf(sha256: Option<String>, body: Vec<u8>) -> String {
        serve(move |_, path, _| {
            if path.starts_with("/api/models/ggerganov/whisper.cpp/tree/main") {
                return match &sha256 {
                    Some(h) => (
                        200,
                        vec![("content-type".into(), "application/json".into())],
                        serde_json::json!([
                            {"type": "file", "path": "README.md", "size": 10},
                            {"type": "file", "path": "ggml-small-q8_0.bin", "size": 134,
                             "lfs": {"oid": h, "size": body.len(), "pointerSize": 134}}
                        ])
                        .to_string()
                        .into_bytes(),
                    ),
                    None => (404, vec![], b"{}".to_vec()),
                };
            }
            assert!(
                path.starts_with("/ggerganov/whisper.cpp/resolve/main/ggml-"),
                "{path}"
            );
            (200, vec![], body.clone())
        })
        .0
    }

    async fn fetch(
        base: &str,
        lib: &Library,
        name: &str,
        cancel: bool,
    ) -> Result<models::Downloaded> {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let flag = AtomicBool::new(cancel);
        let seen = Mutex::new(Vec::new());
        let r = models::download(
            &http,
            base,
            lib,
            models::info(name).unwrap(),
            &flag,
            |a, b| seen.lock().unwrap().push((a, b)),
        )
        .await;
        if r.is_ok() {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.first(), Some(&(0, Some(payload().len() as u64))));
            assert_eq!(
                seen.last(),
                Some(&(payload().len() as u64, Some(payload().len() as u64)))
            );
        }
        r
    }

    #[test]
    fn downloads_are_verified_before_they_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let q8 = models::info("small-q8_0").unwrap();
        let good = hex::encode(sha2::Sha256::digest(payload()));
        // No published SHA-1: verified against Hugging Face's SHA-256.
        let d = rt()
            .block_on(fetch(
                &hf(Some(good.clone()), payload()),
                &lib,
                "small-q8_0",
                false,
            ))
            .unwrap();
        assert!(d.verified);
        assert_eq!(std::fs::read(&d.path).unwrap(), payload());
        assert_eq!(models::installed(&lib, q8), models::Installed::Verified);
        models::delete(&lib, q8).unwrap();
        // A wrong SHA-256: nothing is kept.
        let bad = "0".repeat(64);
        let e = rt()
            .block_on(fetch(&hf(Some(bad), payload()), &lib, "small-q8_0", false))
            .unwrap_err();
        assert!(format!("{e:#}").contains("SHA-256"), "{e:#}");
        assert_eq!(models::installed(&lib, q8), models::Installed::No);
        assert_eq!(
            std::fs::read_dir(models::models_dir(&lib)).unwrap().count(),
            0,
            "no .part left"
        );
        // No listing: kept, but unverified.
        let d = rt()
            .block_on(fetch(&hf(None, payload()), &lib, "small-q8_0", false))
            .unwrap();
        assert!(!d.verified);
        assert_eq!(models::installed(&lib, q8), models::Installed::Unverified);
        // A published SHA-1 that does not match.
        let e = rt()
            .block_on(fetch(&hf(None, payload()), &lib, "base", false))
            .unwrap_err();
        assert!(format!("{e:#}").contains("published SHA-1"), "{e:#}");
        assert_eq!(
            models::installed(&lib, models::info("base").unwrap()),
            models::Installed::No
        );
        // Cancelled: nothing is kept.
        assert!(
            rt().block_on(fetch(&hf(Some(good), payload()), &lib, "base", true))
                .is_err()
        );
        assert_eq!(
            models::installed(&lib, models::info("base").unwrap()),
            models::Installed::No
        );
    }
}
