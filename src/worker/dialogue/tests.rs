//! Text-to-Dialogue generation against a fake ElevenLabs server.

use std::collections::HashSet;
use std::sync::mpsc::Receiver;

use base64::Engine;
use serde_json::{Value, json};

use super::super::tests::{rt, serve, test_ctx};
use super::super::{Ctx, Event, generate, store_segments};
use crate::audio;
use crate::db::Library;
use crate::dialogue::testing::{Synth, synth};
use crate::lang;
use crate::model::*;
use crate::segment;

const RATE: u32 = 16_000;
const FORMAT: &str = "pcm_16000";
const DIALOGUE_PATH: &str = "/text-to-dialogue/with-timestamps?output_format=pcm_16000";

/// Narration, a paragraph with three speakers, narration.
const CHAPTER: &str =
    "The night was cold.\n\n“Who goes there?” asked the guard. “Only me.”\n\nNobody answered.";
/// Only the multi-speaker paragraph.
const PARAGRAPH: &str = "“Who goes there?” asked the guard. “Only me.”";

type Reply = (u16, Vec<(String, String)>, Vec<u8>);
type Requests = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

fn texts_and_voices(body: &Value) -> (Vec<String>, Vec<String>) {
    body["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["text"].as_str().unwrap().to_string(),
                i["voice_id"].as_str().unwrap().to_string(),
            )
        })
        .unzip()
}

/// The synthetic dialogue for a request body.
fn synth_for(body: &Value) -> Synth {
    let (texts, voices) = texts_and_voices(body);
    let t: Vec<&str> = texts.iter().map(String::as_str).collect();
    let v: Vec<&str> = voices.iter().map(String::as_str).collect();
    synth(&t, &v, RATE)
}

/// A dialogue answer built from the request, after `tweak`.
fn dialogue_reply(n: usize, body: &Value, tweak: &dyn Fn(&mut Synth)) -> Reply {
    let mut s = synth_for(body);
    tweak(&mut s);
    let bytes: Vec<u8> = s.pcm.samples.iter().flat_map(|x| x.to_le_bytes()).collect();
    let (texts, _) = texts_and_voices(body);
    let chars: usize = texts.iter().map(|t| t.chars().count()).sum();
    let answer = json!({
        "audio_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
        "alignment": s.alignment,
        "normalized_alignment": s.alignment,
        "voice_segments": s.segments,
    });
    (
        200,
        vec![
            ("request-id".into(), format!("dlg-{n}")),
            ("character-cost".into(), chars.to_string()),
        ],
        answer.to_string().into_bytes(),
    )
}

/// ElevenLabs: text-to-speech answers 240 samples; dialogue requests go to
/// `on_dialogue`.
fn fake_eleven(
    on_dialogue: impl Fn(usize, &Value) -> Reply + Send + 'static,
) -> (String, Requests) {
    serve(move |n, path, body| {
        let v: Value = serde_json::from_str(body).unwrap();
        if path.starts_with("/text-to-dialogue/") {
            return on_dialogue(n, &v);
        }
        let chars = v["text"].as_str().unwrap().chars().count();
        let pcm: Vec<u8> = (0..240i16).flat_map(|s| (s * 50).to_le_bytes()).collect();
        (
            200,
            vec![
                ("request-id".into(), format!("tts-{n}")),
                ("character-cost".into(), chars.to_string()),
            ],
            pcm,
        )
    })
}

struct Setup {
    ctx: Ctx,
    events: Receiver<Event>,
    lib: Library,
    _dir: tempfile::TempDir,
    project_id: Id,
}

fn setup(url: &str, language: &str, configure: impl FnOnce(&mut Project)) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", url);
    let project_id = ctx
        .db(|db| {
            let pid = db.create_project("Book", language)?;
            let mut p = db.project(pid)?;
            p.voice_map.insert(NARRATOR.into(), "vn".into());
            p.voice_map.insert("guard".into(), "vg".into());
            p.voice_map.insert("vortram".into(), "vv".into());
            p.settings.output_format = FORMAT.into();
            configure(&mut p);
            db.update_project(&p)?;
            Ok(pid)
        })
        .unwrap();
    Setup {
        ctx,
        events,
        lib,
        _dir: dir,
        project_id,
    }
}

/// Import `text`; dialogue segments are spoken by `speakers` in turn.
fn import(s: &Setup, text: &str, speakers: &[&str]) -> (Id, Vec<Segment>) {
    s.ctx
        .db(|db| {
            let lang_code = db.project(s.project_id)?.language;
            let cid = db.import_chapter(s.project_id, "One", text)?;
            for p in db.paragraphs(cid)? {
                let segs = segment::parse_paragraph(&p.text, lang::by_code(&lang_code));
                store_segments(db, &HashSet::new(), &p, &segs)?;
            }
            let mut k = 0;
            for seg in db.chapter_segments(cid)? {
                if seg.kind == SegmentKind::Dialogue {
                    db.set_segment_speaker(seg.id, speakers[k % speakers.len()])?;
                    k += 1;
                }
            }
            Ok((cid, db.chapter_segments(cid)?))
        })
        .unwrap()
}

fn ids(segs: &[Segment]) -> Vec<Id> {
    segs.iter().map(|s| s.id).collect()
}

fn current(ctx: &Ctx, segment_id: Id) -> Take {
    ctx.db(|db| db.take(db.segment(segment_id)?.current_take_id.unwrap()))
        .unwrap()
}

fn dialogue_bodies(requests: &Requests) -> Vec<(String, Value)> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.starts_with("/text-to-dialogue/"))
        .map(|(p, b)| (p.clone(), serde_json::from_str(b).unwrap()))
        .collect()
}

fn tts_count(requests: &Requests) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.starts_with("/text-to-speech/"))
        .count()
}

fn finished(events: &Receiver<Event>) -> (usize, usize, usize, i64) {
    events
        .try_iter()
        .find_map(|e| match e {
            Event::GenerationFinished {
                generated,
                cached,
                failed,
                credits,
            } => Some((generated, cached, failed, credits)),
            _ => None,
        })
        .expect("generation finished")
}

#[test]
fn a_multi_speaker_paragraph_is_one_dialogue_cut_into_takes() {
    let (url, requests) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s = setup(&url, "en", |_| {});
    let (_, segs) = import(&s, CHAPTER, &["guard", "vortram"]);
    assert_eq!(segs.len(), 5);
    let para = &segs[1..4];
    // Audio tags are passed through to the dialogue.
    s.ctx
        .db(|db| db.set_segment_performance(para[0].id, Some("“[whispers] Who goes there?”")))
        .unwrap();

    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();

    // Narration paragraphs per segment, the dialogue paragraph in one request.
    assert_eq!(tts_count(&requests), 2);
    let dialogue = dialogue_bodies(&requests);
    assert_eq!(dialogue.len(), 1);
    let (path, body) = &dialogue[0];
    assert_eq!(path, DIALOGUE_PATH);
    assert_eq!(
        body["inputs"],
        json!([
            {"text": "“[whispers] Who goes there?”", "voice_id": "vg"},
            {"text": "asked the guard.", "voice_id": "vn"},
            {"text": "“Only me.”", "voice_id": "vv"},
        ])
    );
    assert_eq!(body["model_id"], "eleven_v4");
    assert_eq!(body["language_code"], "en");
    assert_eq!(
        body["settings"],
        json!({"stability": 0.5, "similarity": 0.75})
    );
    assert_eq!(body["seed"], 1);
    assert_eq!(body["previous_text"], "The night was cold.");
    assert_eq!(body["future_text"], "Nobody answered.");
    assert!(body.get("previous_request_ids").is_none());

    // One take per segment: the cuts of one dialogue, in its pauses.
    let synth = synth_for(body);
    let takes: Vec<Take> = para.iter().map(|seg| current(&s.ctx, seg.id)).collect();
    let mut joined: Vec<i16> = Vec::new();
    let mut bounds = vec![0usize];
    for t in &takes {
        let pcm = audio::read_wav(&s.lib.audio_path(&t.audio_path)).unwrap();
        assert_eq!(pcm.rate, RATE);
        joined.extend(&pcm.samples);
        bounds.push(joined.len());
    }
    assert_eq!(
        joined, synth.pcm.samples,
        "the takes back to back are the dialogue"
    );
    for k in 0..2 {
        let (end, start) = (synth.speech[k].1, synth.speech[k + 1].0);
        assert!(
            bounds[k + 1] > end && bounds[k + 1] < start,
            "cut {k} is in the pause"
        );
    }
    // Shared request, group and seed; the credits split exactly.
    let group = takes[0].dialogue_group.clone().unwrap();
    let chars = [28i64, 16, 10];
    for (t, &c) in takes.iter().zip(&chars) {
        assert_eq!(t.dialogue_group.as_deref(), Some(group.as_str()));
        assert!(t.request_id.as_deref().unwrap().starts_with("dlg-"));
        assert_eq!(t.seed, 1);
        assert!(t.credits_exact);
        assert_eq!(t.credits_used, c, "54 credits split by characters");
        assert_eq!(t.trim_ms, None, "automatic trimming is off by default");
    }
    assert_eq!(
        takes
            .iter()
            .map(|t| &t.request_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        takes
            .iter()
            .map(|t| &t.hash)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    for seg in para {
        assert_eq!(
            s.ctx.db(|db| db.segment(seg.id)).unwrap().status,
            SegmentStatus::Generated
        );
    }
    // The narration takes are ordinary single-voice takes.
    assert_eq!(current(&s.ctx, segs[0].id).dialogue_group, None);
    assert_eq!(current(&s.ctx, segs[4].id).dialogue_group, None);
    let n = |t: &str| t.chars().count() as i64;
    assert_eq!(
        finished(&s.events),
        (
            5,
            0,
            0,
            54 + n("The night was cold.") + n("Nobody answered.")
        )
    );
}

/// Without a `character-cost` header, dialogue takes are estimated at the
/// learned rate like per-segment takes, the split still sums exactly, the
/// takes are recorded for rate learning (estimates never teach the rate),
/// and every new take is queued for the take check once.
#[test]
fn dialogue_takes_use_the_credit_bookkeeping_and_the_take_check() {
    let no_cost = |r: Reply| -> Reply {
        let (status, headers, body) = r;
        let headers = headers
            .into_iter()
            .filter(|(k, _)| k != "character-cost")
            .collect();
        (status, headers, body)
    };
    let (url, _requests) = fake_eleven(move |n, body| no_cost(dialogue_reply(n, body, &|_| {})));
    let s = setup(&url, "en", |_| {});
    let (cid, segs) = import(&s, CHAPTER, &["guard", "vortram"]);
    // Earlier exact takes of this model billed 0.44 credits per character.
    s.ctx
        .db(|db| {
            for n in 0..5 {
                let t = db.insert_take(&crate::db::NewTake {
                    segment_id: segs[0].id,
                    seed: 100 + n,
                    request_id: None,
                    audio_path: "old.wav",
                    hash: &format!("old{n}"),
                    credits_used: 44,
                    credits_exact: true,
                })?;
                db.record_take_cost(t, "eleven_v4", 100)?;
            }
            Ok(())
        })
        .unwrap();
    // Only the dialogue paragraph: the narration's exact takes would teach
    // the rate while the dialogue is in flight.
    let para = &segs[1..4];
    rt().block_on(generate(&s.ctx, ids(para), false)).unwrap();

    let takes: Vec<Take> = para.iter().map(|seg| current(&s.ctx, seg.id)).collect();
    assert!(takes.iter().all(|t| t.dialogue_group.is_some()));
    // 43 characters at 0.44 = 19 credits, split 17:16:10 by characters.
    let total = (43.0f64 * 0.44).round() as i64;
    assert_eq!(takes.iter().map(|t| t.credits_used).sum::<i64>(), total);
    assert_eq!(
        takes.iter().map(|t| t.credits_used).collect::<Vec<_>>(),
        crate::dialogue::split_credits(total, &[17, 16, 10])
    );
    for (t, chars) in takes.iter().zip([17i64, 16, 10]) {
        assert!(!t.credits_exact, "no header: estimated");
        let (model, stored) = s.ctx.db(|db| db.take_cost(t.id)).unwrap().unwrap();
        assert_eq!((model.as_str(), stored), ("eleven_v4", chars));
    }
    // Estimated takes never teach the rate.
    let sample = s
        .ctx
        .db(|db| db.learned_rate("eleven_v4"))
        .unwrap()
        .unwrap();
    assert_eq!(sample.takes, 5);
    let stats = s
        .ctx
        .db(|db| db.chapter_stats(cid, &db.project(s.project_id)?))
        .unwrap();
    assert!(stats.credits_estimated);
    assert_eq!(stats.credits_used, 5 * 44 + total);

    // Every new take is queued for the take check exactly once.
    let (queued, total_queued) = super::super::takecheck::queued(&s.ctx);
    let mut got = queued.clone();
    got.sort();
    assert_eq!(got, takes.iter().map(|t| t.id).collect::<Vec<_>>());
    assert_eq!(total_queued, 3);

    // With the header the takes are exact and teach the rate; the rest of
    // the chapter goes per segment and is queued for the check as well.
    let (url, _requests) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s2 = setup(&url, "en", |_| {});
    let (_, segs2) = import(&s2, CHAPTER, &["guard", "vortram"]);
    rt().block_on(generate(&s2.ctx, ids(&segs2), false))
        .unwrap();
    for (seg, chars) in segs2[1..4].iter().zip([17i64, 16, 10]) {
        let t = current(&s2.ctx, seg.id);
        assert!(t.credits_exact && t.dialogue_group.is_some());
        assert_eq!(t.credits_used, chars);
        let (_, stored) = s2.ctx.db(|db| db.take_cost(t.id)).unwrap().unwrap();
        assert_eq!(stored, chars);
    }
    let sample = s2
        .ctx
        .db(|db| db.learned_rate("eleven_v4"))
        .unwrap()
        .unwrap();
    assert_eq!(sample.takes, 5, "3 dialogue takes and 2 narration takes");
    let (queued, total_queued) = super::super::takecheck::queued(&s2.ctx);
    let mut got = queued;
    got.sort();
    let mut expect: Vec<Id> = segs2
        .iter()
        .map(|seg| current(&s2.ctx, seg.id).id)
        .collect();
    expect.sort();
    assert_eq!(got, expect);
    assert_eq!(total_queued, 5);
}

/// A discarded dialogue without a `character-cost` header is reported at
/// the learned rate, marked as an estimate.
#[test]
fn a_discarded_dialogue_without_a_cost_header_is_estimated_at_the_learned_rate() {
    let (url, _requests) = fake_eleven(|n, body| {
        let (status, headers, body) = dialogue_reply(n, body, &|s| s.pcm.samples.clear());
        let headers = headers
            .into_iter()
            .filter(|(k, _)| k != "character-cost")
            .collect();
        (status, headers, body)
    });
    let s = setup(&url, "en", |_| {});
    let (_, segs) = import(&s, PARAGRAPH, &["guard", "vortram"]);
    s.ctx
        .db(|db| {
            for n in 0..5 {
                let t = db.insert_take(&crate::db::NewTake {
                    segment_id: segs[0].id,
                    seed: 100 + n,
                    request_id: None,
                    audio_path: "old.wav",
                    hash: &format!("old{n}"),
                    credits_used: 50,
                    credits_exact: true,
                })?;
                db.record_take_cost(t, "eleven_v4", 100)?;
            }
            Ok(())
        })
        .unwrap();
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    let events: Vec<Event> = s.events.try_iter().collect();
    let about = (43.0f64 * 0.5).round() as i64;
    assert!(
        events.iter().any(|e| matches!(e, Event::Info(m)
            if m.contains(&format!("billed (about {about} credits)")))),
        "{events:?}"
    );
    let credits = events.iter().find_map(|e| match e {
        Event::GenerationFinished { credits, .. } => Some(*credits),
        _ => None,
    });
    // The fallback's per-segment takes have the header (43 credits).
    assert_eq!(credits, Some(about + 43));
}

#[test]
fn bad_voice_segments_fall_back_to_per_segment_requests() {
    type Tweak = fn(&mut Synth);
    let cases: [(&str, Tweak); 4] = [
        ("zero-length input", |s| {
            let t = s.segments[1].start_time_seconds;
            s.segments[1].end_time_seconds = t;
            s.segments[2].start_time_seconds = t;
        }),
        ("missing input index", |s| {
            s.segments.remove(1);
        }),
        ("character count mismatch", |s| {
            s.alignment.characters.pop();
            s.alignment.character_start_times_seconds.pop();
            s.alignment.character_end_times_seconds.pop();
        }),
        ("unreadable audio", |s| s.pcm.samples.clear()),
    ];
    for (name, tweak) in cases {
        let (url, requests) = fake_eleven(move |n, body| dialogue_reply(n, body, &tweak));
        let s = setup(&url, "en", |_| {});
        let (_, segs) = import(&s, PARAGRAPH, &["guard", "vortram"]);
        rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
        assert_eq!(dialogue_bodies(&requests).len(), 1, "{name}");
        assert_eq!(tts_count(&requests), 3, "{name}: one request per segment");
        for seg in &segs {
            let t = current(&s.ctx, seg.id);
            assert_eq!(t.dialogue_group, None, "{name}");
            assert!(t.request_id.unwrap().starts_with("tts-"), "{name}");
        }
        let events: Vec<Event> = s.events.try_iter().collect();
        let infos: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                Event::Info(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(
            infos.iter().any(|m| m.contains("for ¶ 1 (")
                && m.contains("one by one")
                && m.contains("billed (43 credits)")),
            "{name}: {infos:?}"
        );
        // The discarded dialogue counts in the credits used: 43 for it and
        // 43 again for the three segments.
        let credits = events.iter().find_map(|e| match e {
            Event::GenerationFinished { credits, .. } => Some(*credits),
            _ => None,
        });
        assert_eq!(credits, Some(86), "{name}");
        // Nothing is left behind from the discarded dialogue.
        let files = std::fs::read_dir(s.lib.audio_dir()).unwrap().count();
        assert_eq!(files, 3, "{name}");
    }

    // A refused request (not billed) also falls back.
    let (url, requests) = fake_eleven(|_, _| {
        (
            400,
            vec![],
            br#"{"detail":{"message":"model not supported"}}"#.to_vec(),
        )
    });
    let s = setup(&url, "en", |_| {});
    let (_, segs) = import(&s, PARAGRAPH, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    assert_eq!(
        (dialogue_bodies(&requests).len(), tts_count(&requests)),
        (1, 3)
    );
    let info = s
        .events
        .try_iter()
        .find_map(|e| match e {
            Event::Info(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert!(
        info.contains("model not supported") && !info.contains("billed"),
        "{info}"
    );
    // Refused, so not billed: only the segments count.
    assert_eq!(finished(&s.events).3, 43);
}

#[test]
fn repeated_billed_fallbacks_stop_dialogue_for_the_run() {
    // Every dialogue answer fails validation (as after a change in the API):
    // after two discarded (billed) dialogues the run stops sending them, so
    // not every paragraph is billed twice.
    let (url, requests) = fake_eleven(|n, body| {
        dialogue_reply(n, body, &|s| {
            let t = s.segments[1].start_time_seconds;
            s.segments[1].end_time_seconds = t;
            s.segments[2].start_time_seconds = t;
        })
    });
    let s = setup(&url, "en", |p| p.settings.concurrency = 1);
    let text: Vec<String> = (1..=6)
        .map(|i| format!("“Who goes there, {i}?” asked guard {i}. “Only me, {i}.”"))
        .collect();
    let (_, segs) = import(&s, &text.join("\n\n"), &["guard", "vortram"]);
    assert_eq!(segs.len(), 18);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    let sent = dialogue_bodies(&requests).len();
    // Exactly the two that may be discarded: nothing else was sent while
    // they were being validated.
    assert_eq!(
        sent,
        crate::dialogue::MAX_DISCARDED_PER_RUN,
        "{sent} dialogue requests"
    );
    assert_eq!(tts_count(&requests), 18, "every segment per segment");
    for seg in &segs {
        assert_eq!(current(&s.ctx, seg.id).dialogue_group, None);
    }
    let events: Vec<Event> = s.events.try_iter().collect();
    let stops = events
        .iter()
        .filter(|e| matches!(e, Event::Info(m) if m.contains("rest of this generation goes per segment")))
        .count();
    assert_eq!(stops, 1);
    let credits = events.iter().find_map(|e| match e {
        Event::GenerationFinished { credits, .. } => Some(*credits),
        _ => None,
    });
    // Every segment once, plus each discarded dialogue.
    let all: i64 = segs.iter().map(|s| s.text.chars().count() as i64).sum();
    assert_eq!(credits, Some(all + all / 6 * sent as i64));
}

#[test]
fn only_whole_first_generations_with_several_voices_use_dialogue() {
    type Configure = fn(&mut Project);
    let cases: [(&str, Configure, bool, usize); 7] = [
        (
            "single voice",
            |p| {
                p.voice_map.insert("guard".into(), "vn".into());
                p.voice_map.insert("vortram".into(), "vn".into());
            },
            false,
            3,
        ),
        (
            "over the budget",
            |p| p.settings.dialogue.max_chars = 40,
            false,
            3,
        ),
        ("re-roll", |_| {}, true, 3),
        ("partial paragraph", |_| {}, false, 2),
        (
            "model without dialogue",
            |p| p.tts_model = "eleven_multilingual_v2".into(),
            false,
            3,
        ),
        (
            "v4 turbo",
            |p| p.tts_model = "eleven_v4_turbo".into(),
            false,
            3,
        ),
        (
            "turned off",
            |p| p.settings.dialogue.enabled = false,
            false,
            3,
        ),
    ];
    for (name, configure, reroll, count) in cases {
        let (url, requests) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
        let s = setup(&url, "en", configure);
        let (_, segs) = import(&s, PARAGRAPH, &["guard", "vortram"]);
        rt().block_on(generate(&s.ctx, ids(&segs)[..count].to_vec(), reroll))
            .unwrap();
        assert!(dialogue_bodies(&requests).is_empty(), "{name}");
        assert_eq!(tts_count(&requests), count, "{name}");
    }
    // Within the budget (44 characters), the same paragraph does use dialogue.
    let (url, requests) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s = setup(&url, "en", |p| p.settings.dialogue.max_chars = 44);
    let (_, segs) = import(&s, PARAGRAPH, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    assert_eq!(
        (dialogue_bodies(&requests).len(), tts_count(&requests)),
        (1, 0)
    );
}

#[test]
fn identical_paragraphs_reuse_their_takes() {
    let (url, requests) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s = setup(&url, "en", |_| {});
    let (_, first) = import(&s, CHAPTER, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&first), false)).unwrap();
    let sent = requests.lock().unwrap().len();
    assert_eq!(sent, 3);
    let _ = finished(&s.events);

    let (_, second) = import(&s, CHAPTER, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&second), false))
        .unwrap();
    assert_eq!(requests.lock().unwrap().len(), sent, "no new requests");
    for (a, b) in first.iter().zip(&second) {
        let (ta, tb) = (current(&s.ctx, a.id), current(&s.ctx, b.id));
        assert_eq!(ta.audio_path, tb.audio_path);
        assert_eq!(ta.hash, tb.hash);
        assert_eq!(ta.dialogue_group, tb.dialogue_group);
        assert_ne!(ta.id, tb.id, "a row of its own");
        assert_eq!(tb.credits_used, 0);
    }
    assert_eq!(finished(&s.events), (5, 5, 0, 0));

    // If part of the cached paragraph is gone, it is generated anew under a
    // fresh seed: the remaining files are never overwritten.
    let (_, third) = import(&s, CHAPTER, &["guard", "vortram"]);
    let kept = current(&s.ctx, first[1].id);
    let gone = current(&s.ctx, first[2].id);
    let before = std::fs::read(s.lib.audio_path(&kept.audio_path)).unwrap();
    std::fs::remove_file(s.lib.audio_path(&gone.audio_path)).unwrap();
    rt().block_on(generate(&s.ctx, ids(&third), false)).unwrap();
    let dialogue = dialogue_bodies(&requests);
    assert_eq!(dialogue.len(), 2);
    assert_ne!(dialogue[1].1["seed"], 1);
    assert_eq!(
        std::fs::read(s.lib.audio_path(&kept.audio_path)).unwrap(),
        before
    );
    let fresh = current(&s.ctx, third[1].id);
    assert_ne!(fresh.audio_path, kept.audio_path);
    assert_ne!(fresh.seed, 1);
}

#[test]
fn stitching_retries_and_rerolls() {
    // Two dialogue paragraphs; the guard speaks twice in the first.
    const TWO: &str = "“Who goes there?” asked the guard. “Answer me!”\n\n“Only me,” said Vortram. “Put that down.”";
    // Continuity fields are refused once (as a model without support would).
    let refused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let r = refused.clone();
    let (url, requests) = fake_eleven(move |n, body| {
        if body.get("previous_request_ids").is_some()
            && !r.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return (
                422,
                vec![],
                br#"{"detail":"previous_request_ids not supported"}"#.to_vec(),
            );
        }
        dialogue_reply(n, body, &|_| {})
    });
    let s = setup(&url, "en", |_| {});
    let (cid, segs) = import(&s, TWO, &["guard", "guard", "vortram", "vortram"]);
    let paras = crate::dialogue::paragraphs(&segs);
    assert_eq!(paras.len(), 2);
    let p1: Vec<Id> = paras[0].iter().map(|s| s.id).collect();
    let p2: Vec<Id> = paras[1].iter().map(|s| s.id).collect();

    rt().block_on(generate(&s.ctx, p1.clone(), false)).unwrap();
    let first_id = current(&s.ctx, p1[0]).request_id.unwrap();
    rt().block_on(generate(&s.ctx, p2.clone(), false)).unwrap();
    let d = dialogue_bodies(&requests);
    assert_eq!(
        d.len(),
        3,
        "one refused attempt, retried without continuity"
    );
    assert_eq!(d[1].1["previous_request_ids"], json!([first_id]));
    assert_eq!(
        d[1].1["previous_text"],
        "“Who goes there?” asked the guard. “Answer me!”"
    );
    assert!(d[2].1.get("previous_request_ids").is_none() && d[2].1.get("previous_text").is_none());
    assert!(current(&s.ctx, p2[0]).dialogue_group.is_some());

    // Re-rolling one segment later is a text-to-speech request, and a
    // dialogue request id is not stitched onto it.
    rt().block_on(generate(&s.ctx, vec![p1[2]], true)).unwrap();
    let tts: Vec<Value> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.starts_with("/text-to-speech/"))
        .map(|(_, b)| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(tts.len(), 1);
    assert_eq!(tts[0]["text"], "“Answer me!”");
    assert!(tts[0].get("previous_request_ids").is_none(), "{}", tts[0]);
    let takes = s.ctx.db(|db| db.takes(p1[2])).unwrap();
    assert_eq!(takes.len(), 2, "the dialogue take is kept");
    assert_eq!(current(&s.ctx, p1[2]).dialogue_group, None);
    let _ = cid;
}

#[test]
fn automatic_trimming_keeps_the_pauses_between_speakers() {
    let (url, _) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s = setup(&url, "en", |p| p.settings.trim.enabled = true);
    let (_, segs) = import(&s, PARAGRAPH, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    let takes: Vec<Take> = segs.iter().map(|seg| current(&s.ctx, seg.id)).collect();
    let dur = |t: &Take| {
        crate::trim::duration_ms(&audio::read_wav(&s.lib.audio_path(&t.audio_path)).unwrap())
    };
    // 120 ms lead-in: cut to the default 40 ms before the speech.
    assert_eq!(takes[0].trim_ms, Some((80, dur(&takes[0]))));
    assert_eq!(takes[1].trim_ms, None);
    let (start, end) = takes[2].trim_ms.unwrap();
    assert_eq!(start, 0);
    // The default 80 ms kept after the speech, of the 200 ms tail (within
    // one 10 ms detector frame).
    assert!((dur(&takes[2]) - end).abs_diff(120) <= 10, "{end}");
}

#[test]
fn auto_trim_chapter_keeps_the_pauses_between_speakers() {
    // Generated with automatic trimming off, then "Auto-trim chapter": the
    // dialogue's outer edges are trimmed, the cuts between the speakers stay
    // (a manual trim on the middle take is replaced by "whole", as Auto-trim
    // chapter replaces manual trims).
    let (url, _) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s = setup(&url, "en", |_| {});
    let (cid, segs) = import(&s, CHAPTER, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    let para = &segs[1..4];
    let middle = current(&s.ctx, para[1].id);
    s.ctx
        .db(|db| db.set_take_trim(middle.id, Some((10, 200))))
        .unwrap();
    let _: Vec<Event> = s.events.try_iter().collect();

    rt().block_on(super::super::autotrim::auto_trim_chapter(&s.ctx, cid))
        .unwrap();
    let takes: Vec<Take> = para.iter().map(|seg| current(&s.ctx, seg.id)).collect();
    let dur = |t: &Take| {
        crate::trim::duration_ms(&audio::read_wav(&s.lib.audio_path(&t.audio_path)).unwrap())
    };
    assert_eq!(takes[0].trim_ms, Some((80, dur(&takes[0]))));
    assert_eq!(takes[1].trim_ms, None);
    let (start, end) = takes[2].trim_ms.unwrap();
    assert_eq!(start, 0);
    assert!(end < dur(&takes[2]));
    let info = s
        .events
        .try_iter()
        .find_map(|e| match e {
            Event::Info(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert!(info.contains("1 dialogue takes kept whole"), "{info}");
}

#[test]
fn non_ascii_paragraphs_validate_per_character() {
    let (url, requests) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s = setup(&url, "cs", |_| {});
    let text = "„Kdo je tam?“ zeptal se strážný — tiše. „Jenom já…“";
    let (_, segs) = import(&s, text, &["guard", "vortram"]);
    assert_eq!(segs.len(), 3);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    let d = dialogue_bodies(&requests);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].1["language_code"], "cs");
    assert_eq!(d[0].1["inputs"][1]["text"], "zeptal se strážný — tiše.");
    assert_eq!(tts_count(&requests), 0);
    for seg in &segs {
        assert!(current(&s.ctx, seg.id).dialogue_group.is_some());
    }
}

/// Like `serve`, but every connection is answered on its own thread, dialogue
/// requests after 400 ms, as a real server handles concurrent requests.
fn serve_concurrent(
    handler: impl Fn(usize, &str, &str) -> Reply + Send + Sync + 'static,
) -> (String, Requests) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log: Requests = Default::default();
    let log2 = log.clone();
    let handler = std::sync::Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let log2 = log2.clone();
            let handler = handler.clone();
            std::thread::spawn(move || {
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
                if path.starts_with("/text-to-dialogue/") {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
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
            });
        }
    });
    (url, log)
}

#[test]
fn the_discard_limit_holds_with_concurrent_requests() {
    // Every dialogue fails validation, with the default concurrency (3) and
    // a server that works on several requests at once: still only
    // MAX_DISCARDED_PER_RUN dialogues are billed and thrown away, as the
    // Generate dialog promises.
    let (url, requests) = serve_concurrent(|n, path, body| {
        let v: Value = serde_json::from_str(body).unwrap();
        if path.starts_with("/text-to-dialogue/") {
            return dialogue_reply(n, &v, &|s| {
                let t = s.segments[1].start_time_seconds;
                s.segments[1].end_time_seconds = t;
                s.segments[2].start_time_seconds = t;
            });
        }
        let chars = v["text"].as_str().unwrap().chars().count();
        let pcm: Vec<u8> = (0..240i16).flat_map(|s| (s * 50).to_le_bytes()).collect();
        (
            200,
            vec![
                ("request-id".into(), format!("tts-{n}")),
                ("character-cost".into(), chars.to_string()),
            ],
            pcm,
        )
    });
    let s = setup(&url, "en", |_| {});
    assert_eq!(
        s.ctx
            .db(|db| db.project(s.project_id))
            .unwrap()
            .settings
            .concurrency,
        3
    );
    let text: Vec<String> = (1..=10)
        .map(|i| format!("“Who goes there, {i}?” asked guard {i}. “Only me, {i}.”"))
        .collect();
    let (_, segs) = import(&s, &text.join("\n\n"), &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    let max = crate::dialogue::MAX_DISCARDED_PER_RUN;
    assert_eq!(dialogue_bodies(&requests).len(), max);
    let events: Vec<Event> = s.events.try_iter().collect();
    let billed_twice = events
        .iter()
        .filter(|e| matches!(e, Event::Info(m) if m.contains("billed (")))
        .count();
    assert_eq!(billed_twice, max);
    assert_eq!(tts_count(&requests), segs.len(), "the rest per segment");
    let reported = events
        .iter()
        .filter(|e| matches!(e, Event::SegmentGenerated { .. }))
        .count();
    assert_eq!(reported, segs.len());
}

#[test]
fn cancel_stops_dialogues_waiting_for_a_request_slot() {
    // Cancel is pressed while the first dialogue is on its way: the
    // paragraphs waiting for a request slot send nothing, neither as a
    // dialogue nor per segment.
    let epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let e2 = epoch.clone();
    let (url, requests) = fake_eleven(move |n, body| {
        e2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        dialogue_reply(n, body, &|_| {})
    });
    let mut s = setup(&url, "en", |p| p.settings.concurrency = 1);
    s.ctx.epoch = epoch;
    let text: Vec<String> = (1..=8)
        .map(|i| format!("“Who goes there, {i}?” asked guard {i}. “Only me, {i}.”"))
        .collect();
    let (_, segs) = import(&s, &text.join("\n\n"), &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&segs), false)).unwrap();
    assert_eq!(
        dialogue_bodies(&requests).len(),
        1,
        "only the one already sent"
    );
    assert_eq!(tts_count(&requests), 0);
    let events: Vec<Event> = s.events.try_iter().collect();
    let cancelled = events
        .iter()
        .filter(|e| matches!(e, Event::GenerationFailed { error, .. } if error == "cancelled"))
        .count();
    assert_eq!(cancelled, segs.len() - 3);
    let done = events.iter().find_map(|e| match e {
        Event::GenerationFinished {
            generated, failed, ..
        } => Some((*generated, *failed)),
        _ => None,
    });
    assert_eq!(
        done,
        Some((3, 0)),
        "the sent dialogue is kept; cancelled is no failure"
    );
}

#[test]
fn lines_with_per_segment_takes_are_not_billed_again_as_a_dialogue() {
    // A chapter generated one segment at a time (dialogue off, or a library
    // from before dialogue), then the same text again with dialogue on (a
    // chapter imported again to fix a typo): every line is reused for
    // nothing instead of paying for a dialogue.
    let (url, requests) = fake_eleven(|n, body| dialogue_reply(n, body, &|_| {}));
    let s = setup(&url, "en", |p| p.settings.dialogue.enabled = false);
    let (_, first) = import(&s, CHAPTER, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&first), false)).unwrap();
    assert_eq!(tts_count(&requests), 5);
    s.ctx
        .db(|db| {
            let mut p = db.project(s.project_id)?;
            p.settings.dialogue.enabled = true;
            db.update_project(&p)
        })
        .unwrap();
    let _ = s.events.try_iter().count();
    let (_, again) = import(&s, CHAPTER, &["guard", "vortram"]);
    rt().block_on(generate(&s.ctx, ids(&again), false)).unwrap();
    assert_eq!(requests.lock().unwrap().len(), 5, "no new request");
    assert_eq!(finished(&s.events), (5, 5, 0, 0));
    for seg in &again {
        assert_eq!(current(&s.ctx, seg.id).credits_used, 0);
    }
}
