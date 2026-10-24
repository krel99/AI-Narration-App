//! The voice picker's jobs against a local fake ElevenLabs: paging through
//! the account's voices, lookups by id, and preview caching.

use super::*;
use crate::worker::voices as jobs;
use serde_json::json;

type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

/// A GET-only fake server that also records each request's headers.
fn serve_get(handler: impl Fn(&str) -> (u16, Vec<u8>) + Send + 'static) -> (String, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut headers = Vec::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.push((k.trim().to_lowercase(), v.trim().to_string()));
                }
            }
            seen2.lock().unwrap().push((path.clone(), headers));
            let (status, body) = handler(&path);
            // Preview files are served as text/plain; that must not matter.
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (url, seen)
}

fn paths(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect()
}

fn header(seen: &Seen, n: usize, name: &str) -> Option<String> {
    seen.lock().unwrap()[n]
        .1
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn voice_json(id: &str, name: &str) -> serde_json::Value {
    json!({
        "voice_id": id, "name": name, "category": "cloned",
        "labels": {"gender": "female"}, "description": null,
        "preview_url": null, "samples": null, "settings": null, "sharing": null
    })
}

fn wav(samples: &[i16]) -> Vec<u8> {
    let mut cur = std::io::Cursor::new(Vec::new());
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 22050,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
    for &s in samples {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
    cur.into_inner()
}

/// An MP3 preview when ffmpeg can make one, otherwise WAV (both decode).
fn preview_file() -> Vec<u8> {
    let tone: Vec<i16> = (0..11025)
        .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
        .collect();
    let wav = wav(&tone);
    if !crate::export::ffmpeg_available() {
        return wav;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, dst) = (dir.path().join("in.wav"), dir.path().join("out.mp3"));
    std::fs::write(&src, &wav).unwrap();
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&src)
        .args(["-codec:a", "libmp3lame", "-b:a", "64k"])
        .arg(&dst)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        let mp3 = std::fs::read(&dst).unwrap();
        assert!(!mp3.starts_with(b"RIFF"));
        mp3
    } else {
        wav
    }
}

fn voices_event(events: &Receiver<Event>) -> std::result::Result<Vec<VoiceInfo>, String> {
    events
        .try_iter()
        .find_map(|e| match e {
            Event::Voices(v) => Some(v),
            _ => None,
        })
        .expect("a Voices event")
}

fn preview_event(events: &Receiver<Event>) -> (String, std::result::Result<audio::Pcm, String>) {
    events
        .try_iter()
        .find_map(|e| match e {
            Event::VoicePreview { voice_id, result } => Some((voice_id, result)),
            _ => None,
        })
        .expect("a VoicePreview event")
}

#[test]
fn voice_list_pages_through_my_voices() {
    let (url, seen) = serve_get(|path| {
        let body = if path.contains("next_page_token") {
            json!({
                "voices": [voice_json("v3", "Mira"), voice_json("v1", "alpha")],
                "has_more": false, "next_page_token": null
            })
        } else {
            json!({
                "voices": [voice_json("v2", "Zed"), voice_json("v1", "alpha"), {"name": "no id"}],
                "has_more": true, "next_page_token": "page 2/="
            })
        };
        (200, body.to_string().into_bytes())
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(jobs::fetch_voices(&ctx)).unwrap();

    let p = paths(&seen);
    assert_eq!(p.len(), 2, "two pages, then has_more is false");
    for q in &p {
        assert!(q.starts_with("/v2/voices?"), "{q}");
        assert!(q.contains("voice_type=non-default"), "{q}");
        assert!(q.contains("page_size=100"), "{q}");
        assert!(q.contains("include_total_count=false"), "{q}");
    }
    assert!(!p[0].contains("next_page_token"));
    assert!(p[1].contains("next_page_token=page+2%2F%3D"), "{}", p[1]);
    assert_eq!(header(&seen, 0, "xi-api-key").as_deref(), Some("test"));

    let list = voices_event(&events).unwrap();
    let names: Vec<&str> = list.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(
        names,
        ["alpha", "Mira", "Zed"],
        "deduplicated, sorted by name"
    );
    assert_eq!(
        list[0].labels.get("gender").map(String::as_str),
        Some("female")
    );
}

#[test]
fn voice_list_errors_are_reported_not_thrown() {
    let (url, seen) = serve_get(|_| {
        (
            401,
            br#"{"detail":{"status":"missing_permissions","message":"The API key you used is missing the permission voices_read"}}"#.to_vec(),
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(jobs::fetch_voices(&ctx)).unwrap();
    let err = voices_event(&events).unwrap_err();
    assert!(err.contains("lacks the voices_read permission"), "{err}");

    // A rejected key is said plainly.
    let (url, _) = serve_get(|_| {
        (
            401,
            br#"{"detail":{"status":"invalid_api_key","message":"Invalid API key"}}"#.to_vec(),
        )
    });
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(jobs::fetch_voices(&ctx)).unwrap();
    assert!(
        voices_event(&events)
            .unwrap_err()
            .contains("rejected the API key")
    );

    // No key: no request at all.
    let (ctx, events) = test_ctx(&lib, "http://unused", "http://127.0.0.1:9");
    ctx.keys.write().unwrap().elevenlabs.clear();
    rt().block_on(jobs::fetch_voices(&ctx)).unwrap();
    assert!(voices_event(&events).unwrap_err().contains("API key"));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn assigned_voices_outside_the_list_are_looked_up() {
    let (url, seen) = serve_get(|path| match path {
        "/voices/prem1" => (
            200,
            json!({"voice_id": "prem1", "name": "Rachel", "category": "premade"})
                .to_string()
                .into_bytes(),
        ),
        "/voices/nope1" => (404, br#"{"detail":{"status":"voice_not_found"}}"#.to_vec()),
        _ => (500, b"oops".to_vec()),
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    let ids = ["prem1", "nope1", "flaky1"].map(String::from).to_vec();
    rt().block_on(jobs::lookup_voices(&ctx, ids)).unwrap();
    assert_eq!(
        paths(&seen),
        ["/voices/prem1", "/voices/nope1", "/voices/flaky1"]
    );
    let (found, missing) = events
        .try_iter()
        .find_map(|e| match e {
            Event::VoicesLookedUp { found, missing } => Some((found, missing)),
            _ => None,
        })
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Rachel");
    // A server error is not "unknown voice": it is in neither list.
    assert_eq!(missing, ["nope1"]);
}

#[test]
fn previews_are_downloaded_once_and_keyed_by_url() {
    let file = preview_file();
    let served = file.clone();
    let (url, seen) = serve_get(move |path| {
        if path.starts_with("/previews/") {
            (200, served.clone())
        } else {
            (404, vec![])
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    let mut voice = VoiceInfo {
        voice_id: "vale1".into(),
        name: "Vale".into(),
        preview_url: Some(format!("{url}/previews/vale.mp3")),
        ..Default::default()
    };

    rt().block_on(jobs::preview_voice(&ctx, voice.clone(), "en".into()))
        .unwrap();
    let (id, pcm) = preview_event(&events);
    assert_eq!(id, "vale1");
    let pcm = pcm.unwrap();
    assert!(pcm.rate > 0 && pcm.samples.len() > 1000);
    assert_eq!(paths(&seen), ["/previews/vale.mp3"]);
    assert_eq!(
        header(&seen, 0, "xi-api-key"),
        None,
        "the API key never goes to the preview host"
    );
    let cache = crate::voices::previews_dir(&lib.root);
    let files: Vec<String> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(files.len(), 1);
    assert!(
        files[0].starts_with("vale1-") && files[0].ends_with(".mp3"),
        "{files:?}"
    );

    // Second play: from the cache, no request.
    rt().block_on(jobs::preview_voice(&ctx, voice.clone(), "en".into()))
        .unwrap();
    assert_eq!(preview_event(&events).1.unwrap(), pcm);
    assert_eq!(paths(&seen).len(), 1, "second play doesn't refetch");

    // A new preview URL (the voice was edited) is fetched again and
    // replaces the old file.
    voice.preview_url = Some(format!("{url}/previews/vale-v2.mp3"));
    rt().block_on(jobs::preview_voice(&ctx, voice, "en".into()))
        .unwrap();
    assert!(preview_event(&events).1.is_ok());
    assert_eq!(paths(&seen).len(), 2);
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
}

#[test]
fn previews_fall_back_to_the_uploaded_sample() {
    let sample = wav(&[0, 4000, -4000, 2000, 0, 100, -100, 50]);
    let served = sample.clone();
    let (url, seen) = serve_get(move |path| {
        if path == "/voices/clone1/samples/s1/audio" {
            (200, served.clone())
        } else {
            // The preview URL is gone, and garbage must not be cached.
            (200, b"<html>gone</html>".to_vec())
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    let voice = VoiceInfo {
        voice_id: "clone1".into(),
        name: "My clone".into(),
        preview_url: Some(format!("{url}/previews/broken.mp3")),
        sample: Some(crate::tts::voices::VoiceSample {
            sample_id: "s1".into(),
            mime_type: Some("audio/wav".into()),
        }),
        ..Default::default()
    };
    rt().block_on(jobs::preview_voice(&ctx, voice.clone(), "en".into()))
        .unwrap();
    let pcm = preview_event(&events).1.unwrap();
    assert_eq!(pcm.samples.len(), 8);
    assert_eq!(
        paths(&seen),
        ["/previews/broken.mp3", "/voices/clone1/samples/s1/audio"]
    );
    assert_eq!(header(&seen, 0, "xi-api-key"), None);
    assert_eq!(header(&seen, 1, "xi-api-key").as_deref(), Some("test"));
    let cached: Vec<String> = std::fs::read_dir(crate::voices::previews_dir(&lib.root))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        cached.len(),
        1,
        "only the sample, which decoded: {cached:?}"
    );
    assert!(cached[0].ends_with(".wav"));
    // Played again from the cache, without asking the dead URL first.
    rt().block_on(jobs::preview_voice(&ctx, voice, "en".into()))
        .unwrap();
    assert_eq!(preview_event(&events).1.unwrap(), pcm);
    assert_eq!(paths(&seen).len(), 2);

    // Nothing to play at all.
    let bare = VoiceInfo {
        voice_id: "bare1".into(),
        ..Default::default()
    };
    rt().block_on(jobs::preview_voice(&ctx, bare, "en".into()))
        .unwrap();
    assert_eq!(
        preview_event(&events).1.unwrap_err(),
        "This voice has no preview."
    );
}

#[test]
fn voice_paging_stops_on_a_repeated_or_missing_token() {
    // A server that always says has_more with the same token.
    let (url, seen) = serve_get(|_| {
        let body = json!({"voices": [voice_json("v1", "Vale")], "has_more": true, "next_page_token": "same"});
        (200, body.to_string().into_bytes())
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(jobs::fetch_voices(&ctx)).unwrap();
    assert_eq!(paths(&seen).len(), 2, "the repeated token is not followed");
    assert_eq!(voices_event(&events).unwrap().len(), 1);

    // has_more without a token.
    let (url, seen) = serve_get(|_| {
        let body = json!({"voices": [voice_json("v1", "Vale")], "has_more": true, "next_page_token": null});
        (200, body.to_string().into_bytes())
    });
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(jobs::fetch_voices(&ctx)).unwrap();
    assert_eq!(paths(&seen).len(), 1);
    assert_eq!(voices_event(&events).unwrap().len(), 1);
}

#[test]
fn a_403_insufficient_permissions_is_a_missing_permission() {
    let (url, _) = serve_get(|_| {
        (
            403,
            br#"{"detail":{"type":"authorization_error","code":"insufficient_permissions","message":"The API key is missing the permission voices_read"}}"#.to_vec(),
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    rt().block_on(jobs::fetch_voices(&ctx)).unwrap();
    let err = voices_event(&events).unwrap_err();
    assert!(err.contains("lacks the voices_read permission"), "{err}");
}

#[test]
fn lookups_treat_a_400_voice_not_found_as_unknown() {
    let (url, _) = serve_get(|path| {
        match path {
        // Recorded for unknown ids, besides the 404.
        "/voices/gone400" => (
            400,
            br#"{"detail":{"status":"voice_not_found","message":"A voice with voice_id 'gone400' was not found."}}"#.to_vec(),
        ),
        // Any other 400 says nothing about the voice.
        "/voices/odd400" => (400, br#"{"detail":{"status":"invalid_request"}}"#.to_vec()),
        _ => (404, vec![]),
    }
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    let ids = ["gone400", "odd400", "gone404"].map(String::from).to_vec();
    rt().block_on(jobs::lookup_voices(&ctx, ids)).unwrap();
    let (found, missing) = events
        .try_iter()
        .find_map(|e| match e {
            Event::VoicesLookedUp { found, missing } => Some((found, missing)),
            _ => None,
        })
        .unwrap();
    assert!(found.is_empty());
    assert_eq!(missing, ["gone400", "gone404"]);
}

#[test]
fn public_previews_play_without_an_api_key() {
    let file = preview_file();
    let served = file.clone();
    let (url, seen) = serve_get(move |path| {
        if path.starts_with("/previews/") {
            (200, served.clone())
        } else {
            (500, vec![])
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::new(dir.path().to_path_buf());
    let (ctx, events) = test_ctx(&lib, "http://unused", &url);
    ctx.keys.write().unwrap().elevenlabs.clear();
    let voice = VoiceInfo {
        voice_id: "lib1".into(),
        preview_url: Some(format!("{url}/previews/lib1.mp3")),
        ..Default::default()
    };
    rt().block_on(jobs::preview_voice(&ctx, voice, "en".into()))
        .unwrap();
    assert!(preview_event(&events).1.is_ok());

    // An uploaded sample needs the key: no request is made without one.
    let clone = VoiceInfo {
        voice_id: "clone2".into(),
        sample: Some(crate::tts::voices::VoiceSample {
            sample_id: "s1".into(),
            mime_type: None,
        }),
        ..Default::default()
    };
    rt().block_on(jobs::preview_voice(&ctx, clone, "en".into()))
        .unwrap();
    let err = preview_event(&events).1.unwrap_err();
    assert!(err.contains("no ElevenLabs API key"), "{err}");
    assert_eq!(paths(&seen), ["/previews/lib1.mp3"]);
}
