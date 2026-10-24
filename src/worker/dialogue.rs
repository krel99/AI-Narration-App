//! Generating a multi-speaker paragraph as one Text-to-Dialogue request and
//! cutting the result back into one take per segment.
//!
//! `generate` asks [`plan_request`] which of its paragraphs qualify and hands
//! each of those to [`generate_paragraph`] instead of the per-segment loop.
//! When the dialogue cannot be used (rejected, unreadable, or its timings
//! fail validation), the paragraph's jobs are returned and go through the
//! per-segment path as before.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use tokio::sync::Semaphore;

use super::{Ctx, Event, FIRST_TAKE_SEED, Job, backoff, takecheck};
use crate::audio::{self, Pcm};
use crate::db::{Db, DialogueGroup, DialoguePart};
use crate::dialogue::client::{self, CallError, Request};
use crate::dialogue::{self, Input, MAX_PREVIOUS_REQUESTS, Skip, cut};
use crate::lang;
use crate::model::*;
use crate::trim;
use crate::tts::TtsError;

/// `generate`'s counters: (generated, cached, failed, credits).
pub(super) type Stats = Mutex<(usize, usize, usize, i64)>;

/// After this many billed dialogues are discarded in one Generate run, its
/// remaining paragraphs go per segment straight away: a systematic problem
/// (say, a change in the API's answers) must not bill every paragraph twice.
const MAX_DISCARDED_PER_RUN: usize = 2;

/// The paragraphs of one Generate run that go through dialogue.
#[derive(Default)]
pub(super) struct RunPlan {
    paragraphs: HashSet<Id>,
    /// Billed dialogues discarded so far in this run.
    discarded: AtomicUsize,
}

impl RunPlan {
    pub(super) fn contains(&self, paragraph_id: &Id) -> bool {
        self.paragraphs.contains(paragraph_id)
    }
}

/// The paragraphs of this request that go through dialogue. Planning
/// problems only mean per-segment generation.
pub(super) fn plan_request(ctx: &Ctx, segment_ids: &[Id], reroll: bool) -> RunPlan {
    if reroll {
        return RunPlan::default();
    }
    let requested: HashSet<Id> = segment_ids.iter().copied().collect();
    let planned = ctx.db(|db| {
        let mut chapters: Vec<Id> = Vec::new();
        for &id in &requested {
            let c = db.segment_chapter_id(id)?;
            if !chapters.contains(&c) {
                chapters.push(c);
            }
        }
        let mut out = HashSet::new();
        for chapter_id in chapters {
            let project = db.project(db.chapter(chapter_id)?.project_id)?;
            let segments = db.chapter_segments(chapter_id)?;
            for para in dialogue::plan(&project, &segments, &requested, false) {
                out.insert(db.segment(para[0])?.paragraph_id);
            }
        }
        Ok(out)
    });
    RunPlan {
        paragraphs: planned.unwrap_or_else(|e| {
            log::warn!("planning dialogue paragraphs: {e:#}");
            HashSet::new()
        }),
        discarded: AtomicUsize::new(0),
    }
}

enum Outcome {
    /// Every segment got its take.
    Done { cached: bool, credits: i64 },
    /// Generate the paragraph per segment; `spent` says what the discarded
    /// dialogue cost.
    PerSegment { why: String, spent: Spent },
}

/// What a dialogue that is not used cost.
enum Spent {
    /// Nothing was sent (the paragraph no longer qualifies): nothing to report.
    Nothing,
    /// ElevenLabs refused the request: not billed.
    Refused,
    /// Answered and billed: the `character-cost` header (`exact`), or the
    /// characters as an estimate.
    Billed { credits: i64, exact: bool },
}

/// Generate one qualifying paragraph through dialogue. Returns the jobs that
/// still need the per-segment path (all of them on a fallback, none
/// otherwise).
pub(super) async fn generate_paragraph(
    ctx: &Ctx,
    semaphore: &Semaphore,
    started: u64,
    stats: &Stats,
    plan: &RunPlan,
    jobs: Vec<Job>,
) -> Vec<Job> {
    if jobs.is_empty()
        || ctx.cancelled(started)
        || plan.discarded.load(Ordering::SeqCst) >= MAX_DISCARDED_PER_RUN
    {
        return jobs;
    }
    let chapter_id = jobs[0].chapter_id;
    match run(ctx, semaphore, plan, &jobs).await {
        Ok(Outcome::Done { cached, credits }) => {
            if let Ok(mut s) = stats.lock() {
                s.0 += jobs.len();
                if cached {
                    s.1 += jobs.len();
                }
                s.3 += credits;
            }
            for job in &jobs {
                ctx.out.emit(Event::SegmentGenerated {
                    segment_id: job.segment_id,
                });
            }
            ctx.out.emit(Event::SegmentsChanged { chapter_id });
            vec![]
        }
        Ok(Outcome::PerSegment { why, spent }) => {
            log::warn!("dialogue paragraph: {why}; generating per segment");
            if !matches!(spent, Spent::Nothing) {
                let mut paid = match spent {
                    Spent::Billed { credits, exact } => {
                        // The discarded dialogue counts in this run's credits used.
                        if let Ok(mut s) = stats.lock() {
                            s.3 += credits;
                        }
                        let about = if exact { "" } else { "about " };
                        format!(
                            " The dialogue was billed ({about}{credits} credits); the segments are billed again."
                        )
                    }
                    _ => String::new(),
                };
                if matches!(spent, Spent::Billed { .. })
                    && plan.discarded.fetch_add(1, Ordering::SeqCst) + 1 == MAX_DISCARDED_PER_RUN
                {
                    paid.push_str(&format!(
                        " After {MAX_DISCARDED_PER_RUN} such paragraphs, the rest of this generation goes per segment."
                    ));
                }
                // The paragraph as the Review list numbers it (¶ 3).
                let number = ctx
                    .db(|db| {
                        Ok(db
                            .paragraph(db.segment(jobs[0].segment_id)?.paragraph_id)?
                            .order
                            + 1)
                    })
                    .map(|n| format!("¶ {n}"))
                    .unwrap_or_else(|_| "a paragraph".into());
                ctx.out.emit(Event::Info(format!(
                    "Dialogue not used for {number} ({why}); generating its {} segments one by one.{paid}",
                    jobs.len()
                )));
            }
            jobs
        }
        Err(e) => {
            if let Ok(mut s) = stats.lock() {
                s.2 += jobs.len();
            }
            for job in &jobs {
                ctx.out.emit(Event::GenerationFailed {
                    segment_id: job.segment_id,
                    error: format!("{e:#}"),
                });
            }
            ctx.out.emit(Event::SegmentsChanged { chapter_id });
            vec![]
        }
    }
}

/// The request id of `paragraph_id`'s dialogue, when every segment's current
/// take came from that one dialogue request.
fn dialogue_request_of(db: &Db, paragraph_id: Id) -> Result<Option<String>> {
    let mut rid: Option<String> = None;
    for s in db.paragraph_segments(paragraph_id)? {
        let Some(t) = s.current_take_id else {
            return Ok(None);
        };
        let take = db.take(t)?;
        match (take.dialogue_group, take.request_id) {
            (Some(_), Some(r)) if rid.as_ref().is_none_or(|x| *x == r) => rid = Some(r),
            _ => return Ok(None),
        }
    }
    Ok(rid)
}

/// The cost of a billed dialogue that is discarded: the header, or the
/// characters as an estimate (as for the takes' credits).
fn billed(character_cost: Option<i64>, inputs: &[Input]) -> Spent {
    match character_cost {
        Some(credits) => Spent::Billed {
            credits,
            exact: true,
        },
        None => Spent::Billed {
            credits: inputs.iter().map(|i| i.text.chars().count() as i64).sum(),
            exact: false,
        },
    }
}

/// Group and take hashes for `seed`.
fn hashes(
    inputs: &[Input],
    settings: &dialogue::Settings,
    model: &str,
    seed: u32,
    format: &str,
) -> (String, Vec<String>) {
    let group = dialogue::group_hash(inputs, settings, model, seed, format);
    let takes = (0..inputs.len())
        .map(|i| dialogue::take_hash(&group, i))
        .collect();
    (group, takes)
}

async fn run(ctx: &Ctx, semaphore: &Semaphore, plan: &RunPlan, jobs: &[Job]) -> Result<Outcome> {
    let chapter_id = jobs[0].chapter_id;
    // Read fresh: the paragraph may have changed since the request was planned.
    let (project, segs, previous_text, future_text, previous_ids) = ctx.db(|db| {
        let project = db.project(db.chapter(chapter_id)?.project_id)?;
        let paragraph_id = db.segment(jobs[0].segment_id)?.paragraph_id;
        let paragraphs = db.paragraphs(chapter_id)?;
        let pos = paragraphs
            .iter()
            .position(|p| p.id == paragraph_id)
            .ok_or_else(|| anyhow!("paragraph vanished"))?;
        let previous_text = pos
            .checked_sub(1)
            .and_then(|i| dialogue::previous_context(&paragraphs[i].text));
        let future_text = paragraphs
            .get(pos + 1)
            .and_then(|p| dialogue::future_context(&p.text));
        // Earlier dialogue requests right before this paragraph, closest last.
        let mut previous_ids = Vec::new();
        if project.settings.request_stitching {
            for p in paragraphs[..pos].iter().rev() {
                if previous_ids.len() == MAX_PREVIOUS_REQUESTS {
                    break;
                }
                match dialogue_request_of(db, p.id)? {
                    Some(rid) => previous_ids.push(rid),
                    None => break,
                }
            }
            previous_ids.reverse();
        }
        let segs = db.paragraph_segments(paragraph_id)?;
        Ok((project, segs, previous_text, future_text, previous_ids))
    })?;
    let refs: Vec<&Segment> = segs.iter().collect();
    let requested: HashSet<Id> = jobs.iter().map(|j| j.segment_id).collect();
    if let Err(skip) = dialogue::qualifies(&project, &refs, &requested, false) {
        return Ok(Outcome::PerSegment {
            why: skip.reason().into(),
            spent: Spent::Nothing,
        });
    }
    let Some(inputs) = dialogue::inputs(&project, &refs) else {
        return Ok(Outcome::PerSegment {
            why: Skip::Unassigned.reason().into(),
            spent: Spent::Nothing,
        });
    };
    let model = project.tts_model.clone();
    let format = project.settings.output_format.clone();
    let settings = dialogue::settings_for(&model, &project.default_voice_settings);

    // Cache: an identical paragraph reuses its takes without a request.
    let (mut seed, (mut group, mut take_hashes)) = (
        FIRST_TAKE_SEED,
        hashes(&inputs, &settings, &model, FIRST_TAKE_SEED, &format),
    );
    let lib = ctx.lib.clone();
    let existing: Vec<Option<Take>> = ctx.db(|db| {
        take_hashes
            .iter()
            .map(|h| db.take_by_hash(h))
            .collect::<Result<_>>()
    })?;
    let usable = |t: &Option<Take>| {
        t.as_ref()
            .is_some_and(|t| lib.audio_path(&t.audio_path).exists())
    };
    if existing.iter().all(usable) {
        let takes: Vec<Take> = existing.into_iter().flatten().collect();
        store_cache_hit(ctx, &project, &segs, &group, seed, takes).await?;
        return Ok(Outcome::Done {
            cached: true,
            credits: 0,
        });
    }
    let taken = |db: &Db, hashes: &[String]| -> Result<bool> {
        for h in hashes {
            if db.take_by_hash(h)?.is_some() || lib.audio_path(&format!("{h}.wav")).exists() {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if ctx.db(|db| taken(db, &take_hashes))? {
        // Part of an earlier identical paragraph is gone: generate it anew
        // under a fresh seed, so no file is overwritten and one hash keeps
        // meaning one audio file.
        let mut fresh = None;
        for _ in 0..8 {
            let s: u32 = rand::random();
            let h = hashes(&inputs, &settings, &model, s, &format);
            if !ctx.db(|db| taken(db, &h.1))? {
                fresh = Some((s, h));
                break;
            }
        }
        let (s, h) = fresh.ok_or_else(|| anyhow!("no unused seed for the dialogue"))?;
        seed = s;
        (group, take_hashes) = h;
    }

    let key = ctx
        .keys
        .read()
        .map(|k| k.elevenlabs.clone())
        .unwrap_or_default();
    let mut req = Request {
        output_format: format.clone(),
        inputs: inputs.clone(),
        model_id: model.clone(),
        language_code: lang::model_accepts_language_code(&model).then(|| project.language.clone()),
        settings,
        seed,
        previous_text,
        future_text,
        previous_request_ids: previous_ids,
    };
    let mut attempt = 0u32;
    let reply = loop {
        let result = {
            let _permit = semaphore.acquire().await?;
            // Checked when the request would go out: paragraphs wait here
            // for a free slot while earlier dialogues are being validated.
            if plan.discarded.load(Ordering::SeqCst) >= MAX_DISCARDED_PER_RUN {
                return Ok(Outcome::PerSegment {
                    why: "earlier dialogues of this generation could not be cut".into(),
                    spent: Spent::Nothing,
                });
            }
            client::convert_with_timestamps(&ctx.http, &key, &ctx.endpoints.elevenlabs, &req).await
        };
        match result {
            Ok(r) => break r,
            Err(CallError::Api(TtsError::RateLimited {
                retry_after,
                message,
            })) if attempt < 6 => {
                log::info!("429 ({message}); backing off");
                tokio::time::sleep(retry_after.unwrap_or_else(|| backoff(attempt))).await;
            }
            Err(CallError::Api(TtsError::Transient(m))) if attempt < 3 => {
                log::warn!("ElevenLabs dialogue: {m}");
                tokio::time::sleep(backoff(attempt)).await;
            }
            // Continuity fields are "not supported by every model": retry once without.
            Err(CallError::Api(TtsError::Rejected {
                status: 400 | 422,
                message,
            })) if req.has_stitching() => {
                log::info!(
                    "dialogue request rejected with continuity fields ({message}); retrying without"
                );
                req.drop_stitching();
            }
            // A bad key fails per segment too: report it once.
            Err(CallError::Api(e @ TtsError::Rejected { status: 401, .. })) => {
                return Err(e.into());
            }
            Err(CallError::Api(e @ TtsError::Rejected { .. })) => {
                return Ok(Outcome::PerSegment {
                    why: format!("the dialogue request was refused: {e}"),
                    spent: Spent::Refused,
                });
            }
            // Out of retries for a temporary error: per-segment calls would
            // only meet the same outage, so the segments fail (Generate retries).
            Err(CallError::Api(e)) => return Err(e.into()),
            Err(CallError::Unusable {
                message,
                character_cost,
            }) => {
                return Ok(Outcome::PerSegment {
                    why: format!("unusable dialogue response: {message}"),
                    spent: billed(character_cost, &inputs),
                });
            }
        }
        attempt += 1;
    };

    let chars: Vec<usize> = inputs.iter().map(|i| i.text.chars().count()).collect();
    let (total, credits_exact) = match reply.character_cost {
        Some(c) => (c, true),
        // Billed rates differ per plan: estimate from the characters.
        None => (chars.iter().sum::<usize>() as i64, false),
    };
    let request_id = reply.request_id.clone();
    let reply_cost = reply.character_cost;

    // Decode, validate, cut and write the takes off the async threads.
    let trim_settings = project.settings.trim.clone();
    let lib = ctx.lib.clone();
    let hashes_for_files = take_hashes.clone();
    let cut_inputs = inputs.clone();
    let pieces = tokio::task::spawn_blocking(move || -> Result<Result<Vec<Piece>, String>> {
        let pcm = match audio::pcm_format_rate(&format) {
            Some(rate) => audio::from_pcm_bytes(&reply.audio, rate),
            None => match audio::decode_mp3(reply.audio) {
                Ok(p) => p,
                Err(e) => return Ok(Err(format!("the audio does not decode: {e:#}"))),
            },
        };
        let cuts = match cut::cut_points(
            &pcm,
            &cut_inputs,
            &reply.voice_segments,
            reply.alignment.as_ref(),
        ) {
            Ok(c) => c,
            Err(why) => return Ok(Err(why)),
        };
        for note in &cuts.notes {
            log::info!("dialogue: {note}");
        }
        write_pieces(&lib, &pcm, &cuts.bounds, &hashes_for_files, &trim_settings).map(Ok)
    })
    .await??;
    let pieces = match pieces {
        Ok(p) => p,
        Err(why) => {
            return Ok(Outcome::PerSegment {
                why: format!("the dialogue could not be cut reliably: {why}"),
                spent: billed(reply_cost, &inputs),
            });
        }
    };

    let shares = dialogue::split_credits(total, &chars);
    let parts: Vec<DialoguePart> = segs
        .iter()
        .zip(&pieces)
        .zip(&take_hashes)
        .zip(&shares)
        .map(|(((s, piece), hash), &credits_used)| DialoguePart {
            segment_id: s.id,
            audio_path: &piece.audio_path,
            hash,
            credits_used,
            existing: None,
            trim_ms: piece.trim_ms,
        })
        .collect();
    let stored = ctx.db(|db| {
        db.store_dialogue_takes(
            &DialogueGroup {
                group: &group,
                seed,
                request_id: request_id.as_deref(),
                credits_exact,
            },
            &parts,
        )
    })?;
    for (take_id, _) in stored {
        takecheck::new_take(ctx, take_id);
    }
    Ok(Outcome::Done {
        cached: false,
        credits: total,
    })
}

/// One cut take on disk.
struct Piece {
    audio_path: String,
    trim_ms: Option<(u32, u32)>,
}

/// Write each input's slice of `pcm` as `<hash>.wav` (never over an existing
/// file) and work out its automatic trim.
fn write_pieces(
    lib: &crate::db::Library,
    pcm: &Pcm,
    bounds: &[usize],
    hashes: &[String],
    trim_settings: &trim::TrimSettings,
) -> Result<Vec<Piece>> {
    let n = hashes.len();
    let mut out = Vec::with_capacity(n);
    for (i, hash) in hashes.iter().enumerate() {
        let piece = Pcm {
            rate: pcm.rate,
            samples: pcm.samples[bounds[i]..bounds[i + 1]].to_vec(),
        };
        let audio_path = format!("{hash}.wav");
        let abs = lib.audio_path(&audio_path);
        let written = if abs.exists() {
            // Only an identical paragraph generated at the same moment gets
            // here; its file stays as it is.
            log::warn!("{audio_path} already exists; keeping it");
            false
        } else {
            audio::write_wav(&abs, &piece)?;
            true
        };
        let trim_ms = (written && trim_settings.enabled)
            .then(|| dialogue::edge_trim(&piece, trim_settings, i == 0, i + 1 == n))
            .flatten();
        out.push(Piece {
            audio_path,
            trim_ms,
        });
    }
    Ok(out)
}

/// Reuse an identical paragraph's takes: the segment's own take where it has
/// one, a new free row pointing at the same file otherwise.
async fn store_cache_hit(
    ctx: &Ctx,
    project: &Project,
    segs: &[Segment],
    group: &str,
    seed: u32,
    takes: Vec<Take>,
) -> Result<()> {
    let n = takes.len();
    // New rows get their automatic trim, as new takes do.
    let trim_settings = project.settings.trim.clone();
    let lib = ctx.lib.clone();
    let needs: Vec<(usize, Take)> = takes
        .iter()
        .enumerate()
        .filter(|(i, t)| t.segment_id != segs[*i].id && trim_settings.enabled)
        .map(|(i, t)| {
            (
                i,
                Take {
                    trim_ms: None,
                    ..t.clone()
                },
            )
        })
        .collect();
    let trims: Vec<Option<(u32, u32)>> = tokio::task::spawn_blocking(move || {
        let mut out = vec![None; n];
        for (i, t) in needs {
            match trim::load_take(&lib, &t, &trim_settings) {
                Ok(pcm) => out[i] = dialogue::edge_trim(&pcm, &trim_settings, i == 0, i + 1 == n),
                Err(e) => log::warn!("auto-trim of a cached dialogue take: {e:#}"),
            }
        }
        out
    })
    .await?;
    let parts: Vec<DialoguePart> = segs
        .iter()
        .zip(&takes)
        .zip(&trims)
        .map(|((s, t), &trim_ms)| DialoguePart {
            segment_id: s.id,
            audio_path: &t.audio_path,
            hash: &t.hash,
            credits_used: 0,
            existing: (t.segment_id == s.id).then_some(t.id),
            trim_ms,
        })
        .collect();
    let stored = ctx.db(|db| {
        db.store_dialogue_takes(
            &DialogueGroup {
                group,
                seed,
                request_id: takes[0].request_id.as_deref(),
                credits_exact: true,
            },
            &parts,
        )
    })?;
    for (take_id, _) in stored {
        takecheck::new_take(ctx, take_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
