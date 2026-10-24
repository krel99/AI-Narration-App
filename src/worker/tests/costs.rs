//! End-to-end tests of the Claude Code backend (with a fake `claude`) and of
//! the cost bookkeeping, through the real worker passes.

use super::*;
use crate::claude_code::tests::{FakeClaude, success};
use crate::config::ClaudeBackend;
use serde_json::json;

fn use_claude_code(ctx: &Ctx, bin: PathBuf) {
    let mut keys = ctx.keys.write().unwrap();
    keys.claude_backend = ClaudeBackend::ClaudeCode;
    keys.claude_code_path = Some(bin);
    // Claude Code needs no key; an empty one must not matter.
    keys.anthropic.clear();
}

fn usage_rows(ctx: &Ctx) -> Vec<crate::db::UsageRow> {
    ctx.db(|db| db.all_llm_usage()).unwrap()
}

#[test]
fn claude_code_backend_attributes_and_tags_end_to_end() {
    let fake = FakeClaude::new();
    fake.set(
        "out.0.json",
        &success(
            json!({
                "segments": [
                    {"paragraph": 2, "kind": "dialogue", "speaker": "guard", "text": "\"Who goes there?\""},
                    {"paragraph": 2, "kind": "narration", "speaker": "narrator", "text": "asked the guard."},
                    {"paragraph": 3, "kind": "dialogue", "speaker": "the old man", "text": "\"Only me,\""},
                    {"paragraph": 3, "kind": "narration", "speaker": "narrator", "text": "said the old man."}
                ],
                "characters": [{"key": "guard", "display_name": "Guard", "aliases": []}]
            }),
            0.05,
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    // Neither HTTP endpoint may be used.
    let (ctx, events) = test_ctx(&lib, "http://unused", "http://unused");
    use_claude_code(&ctx, fake.bin());
    let (pid, cid) = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            db.upsert_character(pid, "vortram", "Vortram", &["the old man".into()])?;
            Ok((pid, db.import_chapter(pid, "One", STORY)?))
        })
        .unwrap();

    rt().block_on(segment_chapter(&ctx, cid)).unwrap();
    assert_eq!(fake.calls(), 1);
    let prompt = fake.read("stdin.0");
    assert!(prompt.contains("[P2 TARGET]") && prompt.contains("- vortram: Vortram; the old man"));
    let argv = fake.argv(0);
    let arg = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(arg("--model"), "claude-opus-5-5");
    assert!(arg("--system-prompt").contains("attribute speech"));
    assert!(arg("--json-schema").contains("\"characters\""));
    assert!(
        std::path::Path::new(fake.read("cwd.0").trim()).ends_with("claude-cwd"),
        "runs in the app-owned empty directory"
    );

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
    assert!(
        ctx.db(|db| db.characters(pid))
            .unwrap()
            .iter()
            .any(|c| c.key == "guard")
    );

    // Audio tags, answering with the ids the prompt offered.
    let id = |text: &str| segs.iter().find(|s| s.text == text).unwrap().id;
    fake.set(
        "out.1.json",
        &success(
            json!({"segments": [
                {"id": id("\"Who goes there?\""), "performance": "\"[shouting] Who goes there?\""},
                // Changes a word: rejected by validation as with the API.
                {"id": id("asked the guard."), "performance": "[tense] asked a guard."}
            ]}),
            0.02,
        ),
    );
    rt().block_on(direct_chapter(&ctx, cid, false)).unwrap();
    assert_eq!(fake.calls(), 2);
    assert!(
        fake.read("stdin.1")
            .contains(&format!("[S{} |", id("\"Only me,\"")))
    );
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    let tagged: Vec<_> = segs.iter().filter_map(|s| s.performance.clone()).collect();
    assert_eq!(tagged, vec!["\"[shouting] Who goes there?\"".to_string()]);

    // Both calls are on record, priced by what Claude Code reported.
    let rows = usage_rows(&ctx);
    assert_eq!(
        rows,
        vec![
            (
                "attribution".into(),
                "claude_code".into(),
                "claude-opus-5-5".into(),
                183,
                Some(0.05)
            ),
            (
                "tags".into(),
                "claude_code".into(),
                "claude-opus-5-5".into(),
                183,
                Some(0.02)
            ),
        ]
    );
    let finished = events.try_iter().find_map(|e| match e {
        Event::SegmentationFinished { input_tokens, .. } => Some(input_tokens),
        _ => None,
    });
    assert_eq!(finished, Some(1155), "cached prompt tokens count as input");
}

#[test]
fn usage_limit_stops_the_pass_without_falling_back() {
    let fake = FakeClaude::new();
    fake.set(
        "out.json",
        &json!({
            "type": "result", "subtype": "success", "is_error": true,
            "terminal_reason": "api_error", "api_error_status": 429,
            "api_error": "usage_limit_reached", "modelUsage": {},
            "result": "You've hit your session limit · resets 3:45pm"
        })
        .to_string(),
    );
    fake.set("exit", "1");
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", "http://unused");
    use_claude_code(&ctx, fake.bin());
    let cid = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            let mut p = db.project(pid)?;
            // One paragraph per request: two dialogue windows.
            p.settings.segmentation.chunk_paragraphs = 1;
            db.update_project(&p)?;
            db.import_chapter(pid, "One", STORY)
        })
        .unwrap();

    // Through the command, so the automatic audio-tag pass would follow.
    let err = rt()
        .block_on(run(&ctx, Command::SegmentChapter { chapter_id: cid }))
        .unwrap_err()
        .to_string();
    assert!(err.contains("resets 3:45pm"), "{err}");
    assert!(err.contains("Re-segment drafts"), "{err}");
    assert_eq!(fake.calls(), 1, "no second window, no audio-tag pass");
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    assert_eq!(
        segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        vec!["The night was cold."],
        "dialogue paragraphs are left for a later run, not given to the parser"
    );
    assert!(segs.iter().all(|s| s.speaker != UNKNOWN_SPEAKER));
    assert!(
        events
            .try_iter()
            .any(|e| matches!(e, Event::SegmentationFinished { .. }))
    );
}

#[test]
fn missing_claude_code_falls_back_to_the_parser() {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", "http://unused");
    use_claude_code(&ctx, dir.path().join("no-such-claude"));
    let cid = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            db.import_chapter(pid, "One", STORY)
        })
        .unwrap();
    rt().block_on(segment_chapter(&ctx, cid)).unwrap();
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    assert_eq!(
        segs.iter().filter(|s| s.speaker == UNKNOWN_SPEAKER).count(),
        2
    );
    let errors: Vec<String> = events
        .try_iter()
        .filter_map(|e| match e {
            Event::Error(m) => Some(m),
            _ => None,
        })
        .collect();
    assert!(
        errors.iter().any(|m| m.contains("Claude Code not found")),
        "{errors:?}"
    );

    // The tag pass refuses up front instead of failing chunk by chunk.
    let err = rt().block_on(direct_chapter(&ctx, cid, false)).unwrap_err();
    assert!(err.to_string().contains("No audio tags were added"));

    // Detection reports it for Settings.
    rt().block_on(run(
        &ctx,
        Command::DetectClaudeCode {
            path: Some(dir.path().join("no-such-claude")),
        },
    ))
    .unwrap();
    let d = events.try_iter().find_map(|e| match e {
        Event::ClaudeCode(d) => Some(d),
        _ => None,
    });
    assert!(d.is_some_and(|d| !d.ready()));
}

#[test]
fn too_old_claude_code_is_refused() {
    let fake = FakeClaude::new();
    fake.set("version", "2.1.100 (Claude Code)\n");
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, _events) = test_ctx(&lib, "http://unused", "http://unused");
    use_claude_code(&ctx, fake.bin());
    let err = match rt().block_on(claude::claude_client(&ctx)) {
        Ok(_) => panic!("an old CLI must be refused"),
        Err(e) => e,
    };
    assert!(
        err.contains("2.1.100 is too old") && err.contains("claude update"),
        "{err}"
    );
    assert_eq!(fake.calls(), 0, "no model call was attempted");
}

#[test]
fn api_backend_usage_is_recorded_at_list_price() {
    let (url, _requests) = serve(|_, _, _| {
        claude_reply(json!({
            "segments": [
                {"paragraph": 2, "kind": "dialogue", "speaker": "guard", "text": "\"Who goes there?\""},
                {"paragraph": 2, "kind": "narration", "speaker": "narrator", "text": "asked the guard."},
                {"paragraph": 3, "kind": "dialogue", "speaker": "vortram", "text": "\"Only me,\""},
                {"paragraph": 3, "kind": "narration", "speaker": "narrator", "text": "said the old man."}
            ],
            "characters": []
        }))
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, _events) = test_ctx(&lib, &url, "http://unused");
    let cid = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            db.import_chapter(pid, "One", STORY)
        })
        .unwrap();
    rt().block_on(segment_chapter(&ctx, cid)).unwrap();
    let rows = usage_rows(&ctx);
    assert_eq!(rows.len(), 1);
    let (pass, backend, model, output, cost) = rows[0].clone();
    assert_eq!(
        (pass.as_str(), backend.as_str(), model.as_str(), output),
        ("attribution", "api", "claude-opus-5-5", 50)
    );
    // 100 input × $4 + 50 output × $20 per MTok.
    assert!((cost.unwrap() - 0.0014).abs() < 1e-12);
    let summary = ctx.db(|db| db.chapter_llm_usage(cid)).unwrap();
    assert_eq!(summary[0].total_tokens(), 150);
}

#[test]
fn takes_teach_the_rate_and_missing_headers_use_it() {
    let (eleven, _reqs) = serve(|n, _, _| {
        // No character-cost header.
        (
            200,
            vec![("request-id".into(), format!("r{n}"))],
            vec![0u8; 480],
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, _events) = test_ctx(&lib, "http://unused", &eleven);
    let (cid, ids) = ctx
        .db(|db| {
            let pid = db.create_project("Book", "en")?;
            let mut p = db.project(pid)?;
            p.voice_map.insert(NARRATOR.into(), "vn".into());
            p.voice_map.insert("vortram".into(), "vv".into());
            p.settings.output_format = "pcm_24000".into();
            p.settings.audio_tags.enabled = false;
            db.update_project(&p)?;
            let cid = import(db, pid);
            // Earlier exact takes of this model billed 0.44 credits per character.
            let seg = db.chapter_segments(cid)?[0].id;
            for n in 0..5 {
                let t = db.insert_take(&NewTake {
                    segment_id: seg,
                    seed: 100 + n,
                    request_id: None,
                    audio_path: "old.wav",
                    hash: &format!("old{n}"),
                    credits_used: 44,
                    credits_exact: true,
                })?;
                db.record_take_cost(t, "eleven_v4", 100)?;
            }
            let ids: Vec<Id> = db.chapter_segments(cid)?.iter().map(|s| s.id).collect();
            Ok((cid, ids))
        })
        .unwrap();
    rt().block_on(generate(&ctx, ids.clone(), false)).unwrap();
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    let tag = segs.iter().find(|s| s.text == "started Vortram,").unwrap();
    let take = ctx.db(|db| db.take(tag.current_take_id.unwrap())).unwrap();
    assert!(!take.credits_exact);
    let chars = "started Vortram,".chars().count();
    assert_eq!(take.credits_used, (chars as f64 * 0.44).round() as i64);
    let (model, stored) = ctx.db(|db| db.take_cost(take.id)).unwrap().unwrap();
    assert_eq!((model.as_str(), stored), ("eleven_v4", chars as i64));
    // Estimated takes never feed the learned rate.
    let sample = ctx.db(|db| db.learned_rate("eleven_v4")).unwrap().unwrap();
    assert_eq!(sample.takes, 5);
}

#[test]
fn account_balance_and_model_rates() {
    let (url, requests) = serve(|_, path, _| {
        let body = if path.ends_with("/user/subscription") {
            json!({"tier": "creator", "character_count": 18_700, "character_limit": 431_000,
                   "next_character_count_reset_unix": 1_793_664_000, "voice_slots_used": 4, "voice_limit": 30})
        } else {
            json!([{"model_id": "eleven_v4", "model_rates": {"character_cost_multiplier": 1.0}}])
        };
        (200, vec![], body.to_string().into_bytes())
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(run(&ctx, Command::FetchAccount)).unwrap();
    let info = events
        .try_iter()
        .find_map(|e| match e {
            Event::Account(a) => Some(a),
            _ => None,
        })
        .unwrap();
    assert_eq!(info.subscription.unwrap().remaining(), Some(412_300));
    assert_eq!(info.model_rates, 1);
    assert_eq!(ctx.db(|db| db.model_rate("eleven_v4")).unwrap(), Some(1.0));
    let paths: Vec<String> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert!(
        paths.contains(&"/user/subscription".to_string()) && paths.contains(&"/models".to_string())
    );

    // A key without user_read: "balance unavailable", not an invalid key.
    let (url, _) = serve(|_, _, _| {
        (
            401,
            vec![],
            br#"{"detail":{"status":"missing_permissions","message":"missing user_read"}}"#
                .to_vec(),
        )
    });
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(run(&ctx, Command::FetchAccount)).unwrap();
    let info = events
        .try_iter()
        .find_map(|e| match e {
            Event::Account(a) => Some(a),
            _ => None,
        })
        .unwrap();
    let err = info.subscription.unwrap_err();
    assert!(
        err.contains("balance unavailable") && err.contains("user_read"),
        "{err}"
    );
}
