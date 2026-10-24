//! Silence-trim jobs: automatic trimming of new takes (when the project turns
//! it on) and "Auto-trim chapter". Trims are metadata on the take row; the WAV
//! is never touched and no credits are spent. Detection reads the WAV, so it
//! runs on the blocking pool.

use std::sync::atomic::Ordering;

use anyhow::Result;

use super::{Ctx, Event};
use crate::model::*;
use crate::trim::{self, TrimSettings};

/// Read a take's WAV and detect its trim with `settings`.
async fn detect(ctx: &Ctx, settings: &TrimSettings, take_id: Id) -> Result<Option<(u32, u32)>> {
    let take = ctx.db(|db| db.take(take_id))?;
    let (lib, settings) = (ctx.lib.clone(), settings.clone());
    tokio::task::spawn_blocking(move || trim::detect_take(&lib, &take, &settings)).await?
}

/// After a new take row is stored: trim it when automatic trimming is on.
/// Failures are only logged; the (paid) take is kept untrimmed either way.
pub(super) async fn trim_new_take(ctx: &Ctx, project: &Project, take_id: Id) {
    if !project.settings.trim.enabled {
        return;
    }
    match detect(ctx, &project.settings.trim, take_id).await {
        Ok(Some(range)) => {
            // The take is already current, so its segment may have been
            // approved (locked) while the audio was analysed: then it stays
            // exactly as it was approved.
            let stored = ctx.db(|db| {
                let segment_id = db.take(take_id)?.segment_id;
                if db.segment(segment_id)?.status == SegmentStatus::Approved {
                    return Ok(false);
                }
                db.set_take_trim(take_id, Some(range))?;
                Ok(true)
            });
            match stored {
                Ok(true) => {}
                Ok(false) => log::info!("take {take_id}: approved before its trim was stored"),
                Err(e) => log::warn!("storing the trim of take {take_id}: {e:#}"),
            }
        }
        Ok(None) => log::info!("take {take_id}: nothing above the trim threshold; left untrimmed"),
        Err(e) => log::warn!("auto-trim of take {take_id}: {e:#}"),
    }
}

/// Detect and store the trim of the current take of every unapproved segment
/// in the chapter, with the project's settings (whether or not automatic
/// trimming is on). Replaces earlier trims, manual ones included.
pub(super) async fn auto_trim_chapter(ctx: &Ctx, chapter_id: Id) -> Result<()> {
    let started = ctx.epoch.load(Ordering::SeqCst);
    let (project, segments) = ctx.db(|db| {
        let chapter = db.chapter(chapter_id)?;
        Ok((
            db.project(chapter.project_id)?,
            db.chapter_segments(chapter_id)?,
        ))
    })?;
    let (mut trimmed, mut silent, mut failed) = (0usize, 0usize, 0usize);
    for seg in &segments {
        if ctx.cancelled(started) {
            break;
        }
        let Some(take_id) = seg.current_take_id else {
            continue;
        };
        if seg.status == SegmentStatus::Approved {
            continue;
        }
        let range = match detect(ctx, &project.settings.trim, take_id).await {
            Ok(Some(range)) => range,
            Ok(None) => {
                silent += 1;
                continue;
            }
            Err(e) => {
                log::warn!("auto-trim of segment {}: {e:#}", seg.id);
                failed += 1;
                continue;
            }
        };
        // Re-check: the segment may have been approved (locked) or switched
        // to another take while the audio was analysed.
        let stored = ctx.db(|db| {
            let now = db.segment(seg.id)?;
            if now.status == SegmentStatus::Approved || now.current_take_id != Some(take_id) {
                return Ok(false);
            }
            db.set_take_trim(take_id, Some(range))?;
            Ok(true)
        })?;
        trimmed += stored as usize;
    }
    ctx.out.emit(Event::SegmentsChanged { chapter_id });
    let mut msg = format!("Auto-trim: {trimmed} takes trimmed");
    if silent > 0 {
        msg.push_str(&format!(
            ", {silent} left as they are (nothing above {} dB)",
            project.settings.trim.threshold_db
        ));
    }
    if failed > 0 {
        msg.push_str(&format!(", {failed} could not be read (see log)"));
    }
    msg.push_str(". Approved segments are not changed.");
    ctx.out.emit(if failed > 0 {
        Event::Error(msg)
    } else {
        Event::Info(msg)
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{import, rt, serve, test_ctx};
    use super::super::{FIRST_TAKE_SEED, generate};
    use super::*;
    use crate::db::Library;
    use crate::trim::tests::{RATE, silence, tone};

    /// The fake ElevenLabs answer: 200 ms silence, 300 ms tone, 300 ms silence.
    fn take_audio() -> Vec<u8> {
        [silence(200), tone(300), silence(300)]
            .concat()
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect()
    }

    /// Default margins: 40 ms before the tone, 80 ms after it.
    const DETECTED: (u32, u32) = (160, 580);

    #[test]
    fn new_takes_are_trimmed_only_when_enabled() {
        let (url, requests) = serve(|n, _, _| {
            (
                200,
                vec![
                    ("request-id".into(), format!("r{n}")),
                    ("character-cost".into(), "5".into()),
                ],
                take_audio(),
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, _events) = test_ctx(&lib, "http://unused", &url);
        let (pid, cid) = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                assert!(!p.settings.trim.enabled, "off by default");
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = format!("pcm_{RATE}");
                db.update_project(&p)?;
                Ok((pid, import(db, pid)))
            })
            .unwrap();
        let ids: Vec<Id> = ctx
            .db(|db| db.chapter_segments(cid))
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        let current = |id: Id| -> Take {
            ctx.db(|db| db.take(db.segment(id)?.current_take_id.unwrap()))
                .unwrap()
        };

        // Disabled (the default): takes stay untrimmed.
        rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 4);
        for &id in &ids {
            assert_eq!(current(id).trim_ms, None);
        }

        // Enabled: a re-roll's new take is trimmed; its hash and file are not touched.
        ctx.db(|db| {
            let mut p = db.project(pid)?;
            p.settings.trim.enabled = true;
            db.update_project(&p)
        })
        .unwrap();
        rt().block_on(generate(&ctx, vec![ids[0]], true)).unwrap();
        let rerolled = current(ids[0]);
        assert_ne!(rerolled.seed, FIRST_TAKE_SEED);
        assert_eq!(rerolled.trim_ms, Some(DETECTED));
        let wav = crate::audio::read_wav(&lib.audio_path(&rerolled.audio_path)).unwrap();
        assert_eq!(trim::duration_ms(&wav), 800, "the WAV keeps its silence");
        // Playback/export load the trimmed audio.
        let loaded = trim::load_take(&lib, &rerolled, &TrimSettings::default()).unwrap();
        assert_eq!(loaded.samples.len() as u32, (580 - 160) * RATE / 1000);
        // The other segments' existing takes are left as they were.
        assert_eq!(current(ids[1]).trim_ms, None);

        // Cache hits create new take rows for another chapter's segments: trimmed too,
        // while the takes they reuse keep their own (absent) trim.
        let cid2 = ctx.db(|db| Ok(import(db, pid))).unwrap();
        let ids2: Vec<Id> = ctx
            .db(|db| db.chapter_segments(cid2))
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        rt().block_on(generate(&ctx, ids2.clone(), false)).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 5, "all cache hits");
        for &id in &ids2 {
            let t = current(id);
            assert_eq!((t.credits_used, t.trim_ms), (0, Some(DETECTED)));
        }
        assert_eq!(current(ids[2]).trim_ms, None);
    }

    #[test]
    fn auto_trim_chapter_skips_approved_segments() {
        let (url, _requests) = serve(|n, _, _| {
            (
                200,
                vec![("request-id".into(), format!("r{n}"))],
                take_audio(),
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, events) = test_ctx(&lib, "http://unused", &url);
        let cid = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = format!("pcm_{RATE}");
                db.update_project(&p)?;
                Ok(import(db, pid))
            })
            .unwrap();
        let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
        let ids: Vec<Id> = segs.iter().map(|s| s.id).collect();
        rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
        let takes: Vec<Id> = ctx
            .db(|db| {
                ids.iter()
                    .map(|&id| Ok(db.segment(id)?.current_take_id.unwrap()))
                    .collect()
            })
            .unwrap();
        // A manual trim on segment 1, then segment 0 is approved (locked).
        ctx.db(|db| {
            db.set_take_trim(takes[1], Some((10, 790)))?;
            db.set_segment_status(ids[0], SegmentStatus::Approved)
        })
        .unwrap();

        // Automatic trimming is off, but the explicit command still runs.
        rt().block_on(auto_trim_chapter(&ctx, cid)).unwrap();
        let trims: Vec<Option<(u32, u32)>> = ctx
            .db(|db| takes.iter().map(|&t| Ok(db.take(t)?.trim_ms)).collect())
            .unwrap();
        assert_eq!(
            trims,
            vec![None, Some(DETECTED), Some(DETECTED), Some(DETECTED)]
        );
        // Segment statuses are untouched.
        let statuses: Vec<SegmentStatus> = ctx
            .db(|db| db.chapter_segments(cid))
            .unwrap()
            .iter()
            .map(|s| s.status)
            .collect();
        assert_eq!(
            statuses,
            vec![
                SegmentStatus::Approved,
                SegmentStatus::Generated,
                SegmentStatus::Generated,
                SegmentStatus::Generated
            ]
        );
        let info: Vec<String> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::Info(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(
            info.iter()
                .any(|m| m.starts_with("Auto-trim: 3 takes trimmed")),
            "{info:?}"
        );
    }

    #[test]
    fn all_silent_takes_are_left_untrimmed() {
        let (url, _requests) = serve(|_, _, _| (200, vec![], vec![0u8; 9600]));
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, _events) = test_ctx(&lib, "http://unused", &url);
        let cid = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = format!("pcm_{RATE}");
                p.settings.trim.enabled = true;
                db.update_project(&p)?;
                Ok(import(db, pid))
            })
            .unwrap();
        let ids: Vec<Id> = ctx
            .db(|db| db.chapter_segments(cid))
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
        for id in ids {
            let t = ctx
                .db(|db| db.take(db.segment(id)?.current_take_id.unwrap()))
                .unwrap();
            assert_eq!(t.trim_ms, None);
        }
    }

    #[test]
    fn a_take_approved_during_detection_is_not_trimmed() {
        let (url, _requests) = serve(|n, _, _| {
            (
                200,
                vec![("request-id".into(), format!("r{n}"))],
                take_audio(),
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let (ctx, _events) = test_ctx(&lib, "http://unused", &url);
        let (project, ids) = ctx
            .db(|db| {
                let pid = db.create_project("Book", "en")?;
                let mut p = db.project(pid)?;
                p.voice_map.insert(NARRATOR.into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vv".into());
                p.settings.output_format = format!("pcm_{RATE}");
                db.update_project(&p)?;
                let cid = import(db, pid);
                let ids: Vec<Id> = db.chapter_segments(cid)?.iter().map(|s| s.id).collect();
                Ok((p, ids))
            })
            .unwrap();
        rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
        let take_of = |id: Id| {
            ctx.db(|db| {
                db.segment(id)?
                    .current_take_id
                    .ok_or_else(|| anyhow::anyhow!("no take"))
            })
            .unwrap()
        };
        // The user approves segment 0 between the take being stored and
        // trim_new_take storing its trim.
        ctx.db(|db| db.set_segment_status(ids[0], SegmentStatus::Approved))
            .unwrap();
        let mut enabled = project;
        enabled.settings.trim.enabled = true;
        rt().block_on(trim_new_take(&ctx, &enabled, take_of(ids[0])));
        rt().block_on(trim_new_take(&ctx, &enabled, take_of(ids[1])));
        let trim = |id: Id| {
            let take_id = take_of(id);
            ctx.db(|db| db.take(take_id)).unwrap().trim_ms
        };
        assert_eq!(trim(ids[0]), None, "approved: left as it was approved");
        assert_eq!(trim(ids[1]), Some(DETECTED));
    }
}
