//! Text-to-Dialogue generation against a fake ElevenLabs server.

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
                store_segments(db, &p, &segs)?;
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
        let infos: Vec<String> = s
            .events
            .try_iter()
            .filter_map(|e| match e {
                Event::Info(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(
            infos.iter().any(|m| m.contains("for ¶ 1 (")
                && m.contains("one by one")
                && m.contains("billed")),
            "{name}: {infos:?}"
        );
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
