//! Paid takes against everything else that runs at the same time: the
//! segmentation and audio-tag passes, approvals and edits in the UI, Cancel.
//! Nothing may delete, detach or replace a take the user paid for or
//! approved.

use std::time::Duration as StdDuration;

use super::*;

/// TTS answers (24 kHz silence) with a credit count; requests from number
/// `delay_from` on take `delay`.
fn fake_tts(delay_from: usize, delay: StdDuration) -> (String, RequestLog) {
    serve(move |n, _path, body| {
        if n >= delay_from {
            std::thread::sleep(delay);
        }
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let chars = v["text"].as_str().unwrap_or("").chars().count();
        (
            200,
            vec![
                ("request-id".into(), format!("req-{n}")),
                ("character-cost".into(), chars.to_string()),
            ],
            vec![0u8; 4800],
        )
    })
}

/// A project with voices for the narrator, Vortram and the guard, and
/// `import`'s chapter. Returns (project, chapter, segment ids).
fn book(ctx: &Ctx) -> (Id, Id, Vec<Id>) {
    ctx.db(|db| {
        let pid = db.create_project("Book", "en")?;
        let mut p = db.project(pid)?;
        p.voice_map.insert(NARRATOR.into(), "vn".into());
        p.voice_map.insert("vortram".into(), "vv".into());
        p.voice_map.insert("guard".into(), "vg".into());
        p.settings.output_format = "pcm_24000".into();
        db.update_project(&p)?;
        let cid = import(db, pid);
        let ids = db.chapter_segments(cid)?.iter().map(|s| s.id).collect();
        Ok((pid, cid, ids))
    })
    .unwrap()
}

fn credits_used(ctx: &Ctx, pid: Id, cid: Id) -> i64 {
    ctx.db(|db| Ok(db.chapter_stats(cid, &db.project(pid)?)?.credits_used))
        .unwrap()
}

fn infos(events: &Receiver<Event>) -> Vec<String> {
    events
        .try_iter()
        .filter_map(|e| match e {
            Event::Info(m) => Some(m),
            _ => None,
        })
        .collect()
}

#[test]
fn resegment_keeps_drafts_that_have_paid_takes() {
    // A Claude server that is never needed: no paragraph may be re-sent.
    let (claude, claude_reqs) = serve(|_, _, _| {
        (
            400,
            vec![],
            b"{\"error\":{\"message\":\"unexpected\"}}".to_vec(),
        )
    });
    let (url, _) = fake_tts(usize::MAX, StdDuration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, _events) = test_ctx(&lib, &claude, &url);
    let (pid, cid, ids) = book(&ctx);
    rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
    let paid = credits_used(&ctx, pid, cid);
    assert!(paid > 0);
    // Ordinary edits that leave segments drafts without a current take but
    // with their paid takes: new tags on the narration line, a new speaker
    // on every segment of the dialogue paragraph.
    ctx.db(|db| {
        db.set_segment_performance(ids[3], Some("[sighs] The end."))?;
        for &id in &ids[..3] {
            db.set_segment_speaker(id, "guard")?;
        }
        Ok(())
    })
    .unwrap();
    let before = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    assert!(
        before
            .iter()
            .all(|s| s.status == SegmentStatus::Draft && s.current_take_id.is_none())
    );

    rt().block_on(segment_chapter(&ctx, cid, false)).unwrap();
    let after = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    assert_eq!(
        after, before,
        "both paragraphs are left exactly as they were"
    );
    for &id in &ids {
        assert_eq!(ctx.db(|db| db.takes(id)).unwrap().len(), 1, "take kept");
    }
    assert_eq!(credits_used(&ctx, pid, cid), paid);
    assert_eq!(
        claude_reqs.lock().unwrap().len(),
        0,
        "nothing sent to Claude"
    );
}

#[test]
fn a_take_that_finishes_after_approval_does_not_replace_the_approved_one() {
    // The re-roll (request 4 on) takes a while; the user approves the take
    // they heard meanwhile (R, then A).
    let (url, _) = fake_tts(4, StdDuration::from_millis(700));
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    let (_, _, ids) = book(&ctx);
    rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
    let id0 = ids[0];
    let approved = ctx
        .db(|db| Ok(db.segment(id0)?.current_take_id.unwrap()))
        .unwrap();
    let ctx2 = ctx.clone();
    let reroll = std::thread::spawn(move || rt().block_on(generate(&ctx2, vec![id0], true)));
    std::thread::sleep(StdDuration::from_millis(250));
    // The UI's own connection.
    let ui = lib.open().unwrap();
    ui.set_segment_status(id0, SegmentStatus::Rejected).unwrap();
    ui.set_segment_status(id0, SegmentStatus::Approved).unwrap();
    reroll.join().unwrap().unwrap();

    let seg = ctx.db(|db| db.segment(id0)).unwrap();
    assert_eq!(
        (seg.status, seg.current_take_id),
        (SegmentStatus::Approved, Some(approved))
    );
    assert_eq!(
        ctx.db(|db| db.takes(id0)).unwrap().len(),
        2,
        "the paid re-roll is kept"
    );
    assert!(
        infos(&events)
            .iter()
            .any(|m| m.contains("after the segment was approved"))
    );
}

#[test]
fn a_take_in_the_old_voice_does_not_become_current() {
    // The speaker changes while the first take is on its way.
    let (url, requests) = fake_tts(0, StdDuration::from_millis(700));
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    let (_, _, ids) = book(&ctx);
    let id0 = ids[0];
    let ctx2 = ctx.clone();
    let job = std::thread::spawn(move || rt().block_on(generate(&ctx2, vec![id0], false)));
    std::thread::sleep(StdDuration::from_millis(250));
    lib.open()
        .unwrap()
        .set_segment_speaker(id0, "guard")
        .unwrap();
    job.join().unwrap().unwrap();

    assert_eq!(
        requests.lock().unwrap()[0].0,
        "/text-to-speech/vv?output_format=pcm_24000",
        "requested in Vortram's voice"
    );
    let seg = ctx.db(|db| db.segment(id0)).unwrap();
    assert_eq!(seg.speaker, "guard");
    assert_eq!(
        (seg.status, seg.current_take_id),
        (SegmentStatus::Draft, None),
        "still waiting for a take in the guard's voice"
    );
    assert_eq!(ctx.db(|db| db.takes(id0)).unwrap().len(), 1, "kept");
    assert!(infos(&events).iter().any(|m| m.contains("is not used")));
}

/// Fake Messages API for the audio-tag pass: every offered segment gets
/// "[softly]", after `delay`.
fn slow_tagger(delay: StdDuration) -> (String, RequestLog) {
    serve(move |_, _, body| {
        std::thread::sleep(delay);
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let prompt = v["messages"][0]["content"].as_str().unwrap();
        let segs: Vec<serde_json::Value> = prompt
            .lines()
            .filter(|l| l.starts_with("[S"))
            .map(|l| {
                let id: i64 = l[2..l.find(' ').unwrap()].parse().unwrap();
                let text = &l[l.find("] ").unwrap() + 2..];
                serde_json::json!({"id": id, "performance": format!("[softly] {text}")})
            })
            .collect();
        claude_reply(serde_json::json!({ "segments": segs }))
    })
}

#[test]
fn audio_tags_arriving_late_do_not_detach_takes_generated_meanwhile() {
    let (claude, claude_reqs) = slow_tagger(StdDuration::from_millis(800));
    let (eleven, eleven_reqs) = fake_tts(usize::MAX, StdDuration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, &claude, &eleven);
    let (_, cid, ids) = book(&ctx);
    rt().block_on(async {
        let c2 = ctx.clone();
        // As run() does after segmentation: untagged drafts only.
        let tagger = tokio::spawn(async move { direct_chapter(&c2, cid, true).await });
        tokio::time::sleep(Duration::from_millis(250)).await;
        generate(&ctx, ids.clone(), false).await.unwrap();
        tagger.await.unwrap().unwrap();
    });
    assert_eq!(claude_reqs.lock().unwrap().len(), 1);
    assert_eq!(eleven_reqs.lock().unwrap().len(), 4);
    for s in ctx.db(|db| db.chapter_segments(cid)).unwrap() {
        assert_eq!(s.status, SegmentStatus::Generated, "segment {}", s.id);
        assert!(s.current_take_id.is_some());
        assert_eq!(s.performance, None, "the take was made without tags");
    }
    assert!(
        infos(&events)
            .iter()
            .any(|m| m.contains("left as they are"))
    );
}

#[test]
fn resegment_keeps_paragraphs_generated_while_it_runs() {
    let (claude, _) = serve(|_, _, _| {
        std::thread::sleep(Duration::from_millis(800));
        claude_reply(serde_json::json!({
            "segments": [
                {"paragraph": 2, "kind": "dialogue", "speaker": "guard", "text": "\"Who goes there?\""},
                {"paragraph": 2, "kind": "narration", "speaker": "narrator", "text": "asked the guard."},
                {"paragraph": 3, "kind": "dialogue", "speaker": "guard", "text": "\"Only me,\""},
                {"paragraph": 3, "kind": "narration", "speaker": "narrator", "text": "said the old man."}
            ],
            "characters": []
        }))
    });
    let (eleven, eleven_reqs) = fake_tts(usize::MAX, StdDuration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, &claude, &eleven);
    let cid = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            let mut p = db.project(pid)?;
            p.voice_map.insert(NARRATOR.into(), "vn".into());
            p.voice_map.insert("guard".into(), "vg".into());
            p.settings.dialogue.enabled = false;
            p.settings.audio_tags.enabled = false;
            db.update_project(&p)?;
            let cid = db.import_chapter(pid, "One", STORY)?;
            for par in db.paragraphs(cid)? {
                let segs = segment::parse_paragraph(&par.text, lang::by_code("en"));
                store_segments(db, &HashSet::new(), &par, &segs)?;
            }
            for s in db.chapter_segments(cid)? {
                if s.kind == SegmentKind::Dialogue {
                    db.set_segment_speaker(s.id, "guard")?;
                }
            }
            Ok(cid)
        })
        .unwrap();
    rt().block_on(async {
        let c2 = ctx.clone();
        let seg = tokio::spawn(async move { segment_chapter(&c2, cid, false).await });
        tokio::time::sleep(Duration::from_millis(250)).await;
        let ids: Vec<Id> = ctx
            .db(|db| db.chapter_segments(cid))
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        generate(&ctx, ids, false).await.unwrap();
        seg.await.unwrap().unwrap();
    });
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    assert_eq!(segs.len(), 5);
    assert!(segs.iter().all(|s| s.current_take_id.is_some()));
    let takes: usize = segs
        .iter()
        .map(|s| ctx.db(|db| db.takes(s.id)).unwrap().len())
        .sum();
    assert_eq!(
        takes,
        eleven_reqs.lock().unwrap().len(),
        "every paid take kept"
    );
    assert!(infos(&events).iter().any(|m| m.contains("got audio while")));
    assert!(ctx.db(|db| db.attribution_pending(cid)).unwrap().is_empty());
}

#[test]
fn cancel_during_segmentation_skips_the_tag_pass() {
    let (claude, reqs) = serve(|_, _, body| {
        std::thread::sleep(Duration::from_millis(500));
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        if v["system"].as_str().unwrap().contains("audio tags") {
            return claude_reply(serde_json::json!({"segments": []}));
        }
        claude_reply(serde_json::json!({
            "segments": [
                {"paragraph": 2, "kind": "dialogue", "speaker": "guard", "text": "\"Who goes there?\""},
                {"paragraph": 2, "kind": "narration", "speaker": "narrator", "text": "asked the guard."}
            ],
            "characters": []
        }))
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, &claude, "http://unused");
    let cid = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            let mut p = db.project(pid)?;
            p.settings.segmentation.chunk_paragraphs = 1;
            db.update_project(&p)?;
            db.import_chapter(pid, "One", STORY)
        })
        .unwrap();
    rt().block_on(async {
        let c2 = ctx.clone();
        let job = tokio::spawn(async move {
            run(
                &c2,
                Command::SegmentChapter {
                    chapter_id: cid,
                    resume: false,
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // What Command::Cancel does.
        ctx.epoch.fetch_add(1, Ordering::SeqCst);
        job.await.unwrap().unwrap();
    });
    let systems: Vec<bool> = reqs
        .lock()
        .unwrap()
        .iter()
        .map(|(_, b)| {
            let v: serde_json::Value = serde_json::from_str(b).unwrap();
            v["system"].as_str().unwrap().contains("audio tags")
        })
        .collect();
    assert_eq!(systems, vec![false], "one attribution window, no tag pass");
    // Said, and resumable: the paragraph left is still flagged.
    assert!(
        infos(&events)
            .iter()
            .any(|m| m.contains("stopped by Cancel"))
    );
    assert_eq!(ctx.db(|db| db.attribution_pending(cid)).unwrap().len(), 1);
}

#[test]
fn an_odd_retry_after_is_ignored_and_every_segment_is_reported() {
    let (eleven, _) = serve(|n, _, _| {
        if n == 0 {
            (
                429,
                vec![("retry-after".into(), "-1".into())],
                b"{}".to_vec(),
            )
        } else {
            (
                200,
                vec![("request-id".into(), format!("r{n}"))],
                vec![0u8; 480],
            )
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &eleven);
    let (_, cid, ids) = book(&ctx);
    rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
    let reported = events
        .try_iter()
        .filter(|e| matches!(e, Event::SegmentGenerated { .. }))
        .count();
    assert_eq!(reported, ids.len());
    let with_audio = ctx
        .db(|db| db.chapter_segments(cid))
        .unwrap()
        .iter()
        .filter(|s| s.current_take_id.is_some())
        .count();
    assert_eq!(with_audio, ids.len());
}

#[test]
fn cancel_stops_requests_waiting_for_a_slot() {
    // One request at a time; Cancel is pressed while the first is on its
    // way. Nothing else is sent, and the rest is reported cancelled.
    let epoch = Arc::new(AtomicU64::new(0));
    let e2 = epoch.clone();
    let (eleven, requests) = serve(move |n, _, _| {
        e2.fetch_add(1, Ordering::SeqCst);
        (
            200,
            vec![
                ("request-id".into(), format!("r{n}")),
                ("character-cost".into(), "5".into()),
            ],
            vec![0u8; 480],
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (mut ctx, events) = test_ctx(&lib, "http://unused", &eleven);
    ctx.epoch = epoch;
    let ids: Vec<Id> = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            let mut p = db.project(pid)?;
            p.voice_map.insert(NARRATOR.into(), "vn".into());
            p.settings.concurrency = 1;
            p.settings.dialogue.enabled = false;
            db.update_project(&p)?;
            let cid = db.import_chapter(pid, "One", "One.\n\nTwo.\n\nThree.\n\nFour.\n\nFive.")?;
            for par in db.paragraphs(cid)? {
                let segs = segment::parse_paragraph(&par.text, lang::by_code("en"));
                store_segments(db, &HashSet::new(), &par, &segs)?;
            }
            Ok(db.chapter_segments(cid)?.iter().map(|s| s.id).collect())
        })
        .unwrap();
    rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    let events: Vec<Event> = events.try_iter().collect();
    let cancelled = events
        .iter()
        .filter(|e| matches!(e, Event::GenerationFailed { error, .. } if error == "cancelled"))
        .count();
    assert_eq!(cancelled, ids.len() - 1);
    let done = events.iter().find_map(|e| match e {
        Event::GenerationFinished {
            generated,
            failed,
            credits,
            ..
        } => Some((*generated, *failed, *credits)),
        _ => None,
    });
    assert_eq!(done, Some((1, 0, 5)));
}
