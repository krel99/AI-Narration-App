# Narration Studio — Design Decisions

Oct 23, 2026 · Karel Šlaj

## Purpose and scope

A native Linux desktop app that turns long prose into multi-voice narration through the ElevenLabs API, with per-segment review: every segment is approved or re-rolled individually, then assembled into a final audiobook file.

- Target platform: Linux only. Native desktop app, not a web app.
- Single user, local only. No server, no accounts.
- One language per project, chosen by the user. Not multilingual.
- The user assigns an ElevenLabs voice ID to every speaker, including the narrator.
- Speaker attribution is done by an LLM (Claude API).
- API calls draw from the user's ElevenLabs subscription credits first, then pay-as-you-go balance. Every re-roll costs the segment's full character count again.
- Status: planning complete, implementation starts in a new repo.

## Segmentation rules

The segment is the unit of approval. Every quoted speech span is its own segment, and every stretch of non-speech text between them is a narrator segment.

Example paragraph:

```
SOME STORY...
"It is," started Vortram, "truly magnificent!"
SOME STORY...
```

yields 5 segments:

| # | Kind      | Speaker  | Text                 |
|---|-----------|----------|----------------------|
| 1 | narration | narrator | SOME STORY...        |
| 2 | dialogue  | Vortram  | "It is,"             |
| 3 | narration | narrator | started Vortram,     |
| 4 | dialogue  | Vortram  | "truly magnificent!" |
| 5 | narration | narrator | SOME STORY...        |

- Dialogue tags (segment 3) are voiced by the narrator by default.
- Attribution: an LLM pass per chapter (or per chunk with overlap) returns strict JSON `[{paragraph, kind, speaker, text}]`.
- The prompt includes the project's known speaker list, so speakers map to existing keys. Aliases ("the old man" = Vortram) resolve through a character registry kept across chunks.
- Deterministic validation after the LLM: the concatenated segment texts must equal the source paragraph exactly. On mismatch, reject and retry; the LLM must never drop or rewrite words.
- Quote styles handled by the parser and validator depend on the project language (e.g. "…", „…“, «…»).
- An unknown speaker is flagged "unassigned voice" and blocks generation for that segment until mapped.
- Segments are editable before any audio is generated: change speaker, merge, split. Fixing attribution here is free; fixing it after generation costs credits.

## Data model

SQLite for state, plus a folder of audio files named by take hash.

| Entity    | Fields | Notes |
|-----------|--------|-------|
| Project   | name, language, tts_model, voice_map (speaker → voice_id), default voice settings | One language per project; language decides the model |
| Chapter   | project_id, order, title, source_text, status | Unit of export |
| Paragraph | chapter_id, order | Grouping and playback context only |
| Segment   | paragraph_id, order, kind (narration/dialogue), speaker, text, settings override, status, current_take_id | Unit of approval |
| Take      | segment_id, seed, request_id, audio_path, hash, credits_used, created_at | Every take kept for comparison |

- Segment status: draft → generated → approved | rejected. Rejected segments are re-rolled with a new seed; approved ones are locked.
- Take hash = hash(text + voice_id + settings + model + seed). Identical inputs reuse the cached take.
- credits_used per take enables a per-chapter cost and re-roll-rate report.
- The user can pick any earlier take as current, not only the latest.

## Pipeline

Four stages, each resumable from SQLite state.

1. **Import and segment.** Load chapter text, split into paragraphs, run the LLM attribution pass, validate, store segments as draft. User reviews and fixes speakers before generating.
2. **Generate.** One ElevenLabs TTS call per segment.
   - Pass previous_text / next_text (neighboring segments' text) and previous request ids for prosody continuity. Critical for short fragments like dialogue tags.
   - Worker pool with a semaphore sized to the plan's concurrency limit. Retry with backoff on 429.
   - Skip the call when a take with the same hash exists.
3. **Review.** UI lists paragraphs with their segments. Per segment: play, approve, re-roll, speaker badge, take picker. Per paragraph: play all segments in sequence, gapless, for context. Batch action: re-roll all rejected. Chapter progress counter.
   - Keyboard-first: space = play, A = approve, R = reject/re-roll, arrows = navigate.
4. **Assemble and export.** Allowed when every segment in the chapter is approved (a draft export may flag gaps).
   - Concatenate PCM with configurable silence: ~0 ms within a paragraph, longer between paragraphs and scenes.
   - One ffmpeg call: loudnorm, then encode delivery formats.

Threading architecture (same for any stack):

```
UI thread: segment list, review controls, editor, audio player (queue)
    | commands (channel)          ^ events (channel)
Workers: segmentation (Claude API), TTS pool (ElevenLabs, semaphore), exporter
    |
SQLite + audio/<take_hash>.wav
```

## Audio format

Work lossless internally and encode exactly once, at export.

- Request PCM from ElevenLabs (`output_format = pcm_44100`). 44.1 kHz PCM via API requires Pro or higher; verify which PCM rates the user's plan allows.
- Store each take as WAV: raw PCM plus a 44-byte header, so takes open in Audacity.
- Fallback if the plan lacks PCM: request the highest MP3 bitrate (`mp3_44100_192`), decode to PCM once on arrival, work in PCM from then on. Never decode-edit-re-encode MP3 repeatedly.
- Playback and assembly then need no decoder: append samples, insert silence.
- Export targets: MP3 192 kbps CBR 44.1 kHz (ACX/Audible spec), M4B with chapter markers (Apple Books, Audiobookshelf), and a FLAC or WAV master for archiving.
- loudnorm in the ffmpeg export pass evens out level differences between voices.

## Tech stack

Recommended: Rust + egui. Final choice between egui and Go + Fyne is still open.

Concurrency is not a deciding factor: the workload is I/O-bound and capped by ElevenLabs' per-plan concurrency limit, which any language saturates easily.

| Role | Rust + egui (recommended) | Go + Fyne (alternative) |
|------|---------------------------|-------------------------|
| UI | eframe/egui, `ScrollArea::show_rows` for the long list | Fyne, virtualized `widget.List` |
| Playback | rodio (cpal); Sink queue for gapless paragraph playback | oto, or gopxl/beep for sequencing |
| Decode (MP3 fallback only) | symphonia | go-mp3 |
| WAV I/O | hound | go-audio/wav |
| HTTP / async | tokio + reqwest | goroutines + net/http |
| Storage | rusqlite | modernc.org/sqlite (pure Go) |
| Export | ffmpeg subprocess | ffmpeg subprocess |

Rationale for egui: the app is a state-driven list of many small interactive rows, which suits immediate mode; keyboard shortcuts are trivial; rodio is the more mature and widely used playback stack; Rust is the user's primary stack.

Rejected: Walk (Windows-only), web stacks such as Leptos/Axum (user wants native). GTK4 via gtk4-rs + Relm4 is viable only if a native GNOME look matters; it costs the most boilerplate. A Bubble Tea TUI is a viable minimal option since review is keyboard-driven.

## Risks and open items

Risks:

- **Short fragments sound worst.** Two- or three-word dialogue tags are where most re-rolls will happen, even with stitching. Possible mitigation: a per-project option to merge tags into adjacent narration.
- **Re-rolls can break flow with neighbors.** Stitching uses neighboring text, not the chosen audio, so a re-rolled segment may clash with an approved neighbor. Paragraph playback in review is the safeguard.
- **Cost multiplier.** Every rejection costs full credits again. Expect a 10–30% re-roll rate with expressive models; track actual spend per chapter.
- **LLM attribution errors.** Mitigated by exact-text validation and the pre-generation edit step.

Open decisions:

- [ ] Final stack: Rust + egui or Go + Fyne
- [ ] Project language and therefore TTS model (e.g. v3 vs Multilingual v2)
- [ ] Confirm the ElevenLabs plan supports pcm_44100; otherwise use the MP3 fallback
- [ ] Default silence durations within paragraphs, between paragraphs and at scene breaks
- [ ] Whether dialogue tags stay narrator-voiced or merge into narration
- [ ] Chunk size and overlap for the LLM segmentation pass

---

## Implementation notes (first build)

How the first implementation resolves or defers each open item, and where it
goes beyond the plan. Everything below is a default that can be changed.

### Open decisions

| Decision | Status in code |
|----------|----------------|
| Final stack | Built with **Rust + egui** (eframe 0.37, rodio 0.23, rusqlite, tokio + reqwest, hound). Go + Fyne was not pursued. |
| Language / TTS model | Per project. New projects start on `eleven_v4` (performs audio tags); `eleven_v4_turbo`, `eleven_v3`, `eleven_multilingual_v2`, `eleven_turbo_v2_5` and `eleven_flash_v2_5` are selectable in Project settings. `language_code` is sent only to models that accept it (v2.5, v3, v4). |
| pcm_44100 | Default. Project settings offer `pcm_24000`, `pcm_22050` and the `mp3_44100_192` fallback (decoded to PCM once on arrival). Still needs confirming against the actual plan. |
| Silence defaults | 0 ms within a paragraph, 700 ms between paragraphs, 2000 ms at scene breaks, 750 ms chapter lead-in, 2000 ms tail (inside ACX's room-tone ranges). All editable per project. |
| Dialogue tags | Stay narrator-voiced. No automatic merge option yet; the editor's *Merge with next/previous* covers individual cases. Still open. |
| Chunk size / overlap | 30 paragraphs per request, 4 preceding paragraphs as read-only context, 2 retries per paragraph. All editable per project. |

### Segmentation details

- Paragraphs: blank lines separate paragraphs when the text has any; otherwise
  each line is a paragraph. Lines made only of `* # ~ - _ •` and similar mark a
  scene break (stored as `scene_break_before` on the next paragraph).
- Paragraphs without any opening quote character never go to the LLM: they
  become one narrator segment directly. Narration-only paragraphs in a chunk
  are still sent as context.
- Paragraphs that already have audio (any take, or any non-draft segment) are
  never re-segmented, so re-running segmentation cannot discard paid takes.
- Validation: segment texts must appear in order in the paragraph with only
  whitespace between them and cover it completely. Narration segments are
  always assigned to `narrator`. Segments with nothing speakable (a lone dash)
  are folded into a neighbour, and punctuation right after a closing quote
  (French `« Bonjour », dit-il.`) is attached to the quote.
- The LLM response is wrapped as `{segments: [...], characters: [...]}` so it
  can also report new characters and aliases for the registry. It is
  constrained with structured outputs (`output_config.format`), and server-side
  refusal fallback is enabled (`fallbacks: "default"`).
- A paragraph that still fails after the retries, or a chunk that hits a fatal
  API error, falls back to the deterministic quote parser; its quotes get the
  speaker `unknown`, which has no voice and therefore blocks generation until
  fixed.

### Generation details

- First take of a segment uses a fixed seed (1), so identical lines with the
  same voice and settings (a repeated "he said,") share one cached take.
  Re-rolls use a fresh random seed.
- The take hash also includes the output format, so a PCM take never collides
  with an MP3-fallback take.
- A cache hit on another segment creates a new take row with `credits_used = 0`
  pointing at the same file.
- Segments of one paragraph are generated in order (so each can stitch onto
  the previous request id); paragraphs run in parallel under the concurrency
  semaphore. `previous_request_ids` holds up to three ids from earlier
  segments of the same paragraph that use the same voice. If the API rejects
  the request with ids attached, it is retried once without them.
- 429 responses honour `retry-after`, otherwise exponential backoff with jitter.
- Credits: read from the `character-cost` / `x-character-count` response
  header, falling back to the text's character count.

### Export details

- Chapter export: MP3 192 kbps CBR 44.1 kHz mono plus a FLAC master, both from
  one ffmpeg call (`loudnorm=I=-19:TP=-3.5:LRA=11`, then `asplit`). The FLAC
  master is the loudness-normalised version.
- Book export: every fully approved chapter in one M4B (AAC 128 kbps) with
  chapter markers from an FFMETADATA file.
- Assembly streams to a temporary WAV, so book length is not limited by memory.
- Draft export uses current takes regardless of approval and fills missing
  audio with 1 s of silence, reporting the number of gaps.

### Beyond the plan

- Paragraphs store their source text (needed for validation, merge and split).
- A character table holds the registry (key, display name, aliases).
- One library database holds all projects (default
  `~/.local/share/narration-studio/`), with audio shared across projects so the
  hash cache works library-wide.

### Audio tags (added after the first build)

Requested on top of the original plan: ElevenLabs audio tags such as
`[whispers]`, `[thoughtful]`, `[short pause]` and `[long pause]`, in narration
as well as dialogue. For example:

```
said Lieutenant Vale. [thoughtful] Commander Voss looked at the countdown above the forward display. Three minutes. … [long pause] It would not. [short pause] The first enemy vessel slid out of the planet’s shadow.
```

- ElevenLabs performs tags on Eleven v4, v4 Turbo and v3, but has no API that
  inserts them, so Claude does it in a separate "direction" pass. It runs
  automatically after segmentation (project setting, on by default) and on
  demand with *✦ Audio tags*.
- The segment keeps its exact source `text`; the tagged version is stored
  separately as `performance`. Validation strips the tags, collapses
  whitespace (dropping the gap a tag leaves next to a quote mark), and requires
  the result to equal the source exactly. Bracketed text already in the source
  (`[sic]`) must survive, and a segment may not end in a tag, because a tag
  shapes the text after it. Invalid suggestions are discarded and counted.
- The direction pass only touches draft segments without audio. The automatic
  run skips segments that already have tags; the manual run replaces them.
- What is spoken: `performance` for tag-capable models, plain `text`
  otherwise. The take hash uses the spoken text, so changing tags never reuses
  an old take. Neighbouring `previous_text`/`next_text` stay plain.
- Settings: density (light, moderate, rich) and free-form director's notes per
  project. Tags can also be edited per segment in the review detail panel.
- New projects default to `eleven_v4`.

### UI (revised)

- One chapter at a time: the sidebar is gone. The top bar holds the project
  and chapter pickers, the chapter menu (new chapter, delete, export book) and
  two tabs. **Text** shows the chapter as plain prose, editable until it has
  audio. **Review** is the segment list.
- A light "paper" theme is the default (`src/theme.rs`); a dark theme is
  available in Settings.

### Silence trimming (added after the first build)

Requested on top of the plan: trim the silence (and breaths) off every take,
but disabled by default. ElevenLabs takes start and end with uneven silence,
so "0 ms within a paragraph" did not give even gaps.

- **Non-destructive.** The take row stores the kept range as `trim_start_ms`
  and `trim_end_ms` (NULL means untrimmed). An idempotent migration adds the
  columns to older libraries. The WAV is never rewritten and the take hash
  does not change, so the cache still reuses the file. The trim belongs to the
  take row, so a cache hit for another segment has its own trim.
- **Detection** (`trim::detect`): the RMS of 10 ms frames is compared with a
  threshold (default -50 dBFS). The kept range runs from the first to the last
  frame above it, widened by 40 ms before and 80 ms after, because decays fade
  slowly. It is clamped to the take. An all-silent take is left untrimmed, and
  the range is never empty or inverted.
- **Applying** is a cut plus a linear fade-in and fade-out (10 ms, at most half
  the kept length), done when the audio is loaded. `trim::load_take` is the
  only loader of a take's audio: single-take playback, paragraph playback and
  every export go through it. A stale or invalid trim plays the whole take.
- **Automatic trimming** of new takes is off by default
  (`ProjectSettings.trim.enabled`). When it is on, the worker trims each newly
  stored take on the blocking pool. This includes cache-hit takes created for
  another segment. A take that its own segment reuses keeps its trim. A
  failure is only logged and the paid take stays untrimmed.
- **Auto-trim chapter** is a toolbar button shown only while automatic
  trimming is on. It re-detects the current take of every unapproved segment
  and replaces earlier trims, manual ones included. Approved segments are
  skipped, including ones approved while it runs. The detail panel's
  **Auto-trim** works on one take even when automatic trimming is off. Neither
  spends credits.
- **Waveform**: a background thread reads each audio file once and computes
  min/max peaks (2048 buckets) plus the detector's 10 ms frame energies
  (`trim::Frames`); the UI thread never reads a WAV to draw it and shows a
  placeholder until the thread is done. The detail panel's Auto-trim runs
  `trim::detect_frames` on those cached frames, so it gives the same range
  as the worker without reading the file again. Peaks are drawn one column
  per pixel and the cut-off parts are shaded. A press on or beyond a handle
  grabs that handle (between them, the nearer one); the trim is saved on
  release. A drag that ends without its release reaching the waveform (the
  segment was approved or the take changed meanwhile) is dropped unsaved. At
  least 20 ms is kept, and dragging back to the full range stores
  "untrimmed". While the take plays, a playhead follows rodio's per-source
  position. Approved segments show the waveform read-only.
- **Locking**: automatic trimming re-checks, just before storing, that the
  segment was not approved while its take was analysed; Auto-trim chapter
  does the same and also checks the take is still current. The fade length is
  a project setting applied at load time, so changing it also changes the
  cut points of approved trimmed takes (by at most the fade length, inside the
  kept margins).
- **Breaths** inside a take are never touched. At either end they are cut only
  when they are quieter than the threshold. Raising it (e.g. to -40 dBFS) cuts
  more, at the risk of clipping soft word onsets.

### Take checking (added after the first build)

Requested on top of the plan: "This would be done locally. It should flag
it, not reject it." Every take is transcribed on the user's machine with
Whisper (whisper.cpp through whisper-rs 0.16) and compared with its text.
The design follows the verified research
(`plan/research/local-whisper.verify.md`).

- **Flag only.** A check stores a result per take (`take_check`: status
  ok / flagged / dismissed / error / skipped, the transcript, WER, findings
  as JSON, the model, a timestamp). Nothing else changes: segment statuses,
  approval, takes and credits are untouched, and approving a flagged take is
  always possible. Deleting a take deletes its check.
- **What is compared.** The expected words are the segment's `text`; audio
  tags (as `tags::find` defines them) and `<break …/>` are not spoken.
  Both sides are normalised: case, diacritics, apostrophes, quotes, dashes
  and other punctuation; thousands separators; English number words are
  parsed ("forty thousand" = "$40,000"), and digits against unparsed number
  words (Czech, other languages, "nineteen ninety-nine") are not counted.
  Whisper's own annotations (`[laughs]`, `(sighs)`, `*coughs*`, `♪ … ♪`) are
  dropped, but only when they close within 40 characters, so a stray `<` or
  `(` no longer swallows the rest of a line. The words are aligned with a
  token-level edit distance that accepts British/American spelling variants
  by explicit rules (not an edit distance, which accepted dead/deed and
  than/then), merges ("every day" / "everyday"), English contractions
  ("don't" / "do not", "we'll" / "we will", "they'd" / "they had") and
  fillers. Fillers count on neither side: an extra "um" is not an error,
  and neither is an "um" or "oh" of the text that Whisper leaves out
  (it tends to drop disfluencies).
- **Verdict** (heuristic thresholds from the research, in one
  `Thresholds` struct, to be calibrated on real takes): none of the text's
  words heard flags (a line that is only an interjection, "Hmm." or
  "Ha ha!", never flags on its words: a silent take is the length check's
  job); an audio tag heard as extra words flags; lines of at most three words
  (dialogue tags) flag only when nothing matches or two words differ; three
  missing/extra/wrong words in a row flag; otherwise a WER above 20 % (with
  at least three errors, under 15 words) or 12 % (15 words or more) flags,
  and these limits are 1.6 times higher outside English. Anything less is
  "minor differences" and does not flag.
- **Length check.** Free and always on: the sounding part of the take (the
  silence detector with no margins) against the characters at 15 per second
  times the voice's speed. Under half the expected length flags (for lines
  of at least 1.5 s), as does more than twice the expected length plus
  1.5 s plus 1 s per audio tag, or a silent take. Without a model (or in a
  build without Whisper) this is the whole check ("skipped": words not
  checked).
- **Hallucinations.** whisper.cpp already drops a window it judges silent.
  On top of that only a segment that is both in a near-silent window
  (no-speech probability ≥ 0.6) and a known invented phrase ("Thank you.",
  "Thanks for watching!", and two commonly reported Czech ones) is ignored,
  and never one the text itself contains. Filtering on the no-speech
  probability alone would erase real speech.
- **Whisper settings.** The chapter's language (never auto-detect), greedy
  decoding, no context, a single segment without timestamps for takes up
  to 28 s in English and 20 s otherwise (whisper.cpp stops a window after
  220 tokens and, without timestamps, cannot resume; a fast Czech take of
  25 s needs about that many), suppressed
  blanks and non-speech tokens, no initial prompt (the expected text would
  bias Whisper toward it). Input is resampled to 16 kHz with a windowed-sinc
  polyphase filter (`audio::resample_linear` has no anti-alias filter).
  Takes under 1.5 s get a 200 ms lead-in and are padded to 1.5 s. Decoding
  is capped at 16 + 15 tokens per second of audio so a hallucination loop
  cannot run to the window limit; that cap is an addition to the research.
  Whisper's tokenizer needs about 0.2 tokens per character for English but
  about 0.48 for Czech and Slovak (measured with its `multilingual.tiktoken`
  vocabulary), 7–9 tokens a second at narration pace, so a tighter cap
  would cut Czech transcripts short and flag them.
- **One checker, two threads.** A "take-check" thread takes one take at a
  time from a de-duplicated queue: it reads the take with its own database
  connection, runs the length check and the comparison and stores the
  result. It mostly waits, at normal priority. The transcription itself
  (model loading and hashing, resampling, Whisper) runs on a
  "take-check-asr" thread at nice 19 and SCHED_IDLE (both per thread on
  Linux and inherited by ggml's worker threads). Database work is kept off
  the idle-priority thread on purpose: SCHED_IDLE gets almost no CPU on a
  busy machine, and preempted while holding SQLite's write lock it could
  make generation's own write of a paid take time out (busy timeout 5 s).
  Neither thread holds up generation or the UI. The model is loaded on
  first use and dropped after five idle minutes. Cancelling uses whisper.cpp's
  raw abort callback (whisper-rs's `set_abort_callback_safe` is broken in
  0.13–0.16) and is checked before each transcription; whisper.cpp polls it
  only between encoder and decoder passes. Threads default to half the cores
  (1–8).
- **Triggers.** Every take `generate_one` stores, cache hits included, is
  queued while automatic checking is on (`AppConfig.check.enabled`, default
  on). A cache hit reuses the check of the take it shares a file and text
  with instead of transcribing again (a dismissal included). "Check chapter"
  queues the current takes without a result for the current model; failed
  checks are retried, dismissed flags kept, and a length-only result is
  upgraded once a model is installed. "Re-check" goes to the front of the
  queue and replaces the result. The check reads the take's WAV untrimmed:
  it judges the generated audio, which cache hits share, not the editable
  trim.
- **Models.** `<library>/models/ggml-<name>.bin`, downloaded from
  huggingface.co/ggerganov/whisper.cpp to a `.part` file, verified against
  the SHA-1 published in whisper.cpp's model list (or, for files without
  one, the SHA-256 from Hugging Face's file listing), then renamed. A model
  placed by hand is hashed once before its first load. Offered: base, small
  (default), small-q8_0, large-v3-turbo-q5_0 (recommended for Czech) and
  large-v3-turbo; all multilingual. An English-only model is refused for
  other languages.
- **Build.** Cargo feature `whisper` (default). `--no-default-features`
  builds without it; the take check then only checks lengths and Settings
  says so. Debug builds compile whisper.cpp with debug logging, so the log
  filter defaults to `whisper_rs=warn`.

Not verified here (no network access to Hugging Face, no real weights): the
recognition quality of the offered models on ElevenLabs takes in English and
Czech, the size and hash of `small-q8_0`, how Whisper renders performed tags
such as `[laughs]`, and therefore the thresholds. The pipeline was exercised
end to end with random-weight models of the real shapes.

### Multi-speaker dialogue (added after the first build)

Requested on top of the plan: "For paragraphs that go back and forth
between speakers, ElevenLabs' multi-speaker dialogue endpoint produces more
natural turn-taking than separate calls. The result would be cut back into
segments so per-segment review still works." The design follows the
verified research (`plan/research/eleven-dialogue.verify.md`).

- **Endpoint.** `POST /v1/text-to-dialogue/with-timestamps?output_format=…`
  (non-streaming): one JSON answer with `audio_base64`, the character
  `alignment` and `voice_segments` (start/end time, character range and
  `dialogue_input_index` per stretch of audio). Body: `inputs` (one per
  segment, in order: the spoken text with tags, and the voice), `model_id`,
  `language_code`, `settings`, `seed`, `previous_text` / `future_text` and
  `previous_request_ids`. The request id and the `character-cost` (or
  `x-character-count`) header are read as for text-to-speech. 240 s
  timeout, the SDKs' default.
- **Which paragraphs** (`dialogue::qualifies`, used by the worker and by the
  Generate confirmation alike): the setting is on (default), the model is
  `eleven_v4` or `eleven_v3` (v4 Turbo is documented only for the dialogue
  WebSocket), it is a first generation (never a re-roll), the request covers
  every segment of the paragraph and all are drafts without a current take,
  every speaker has a voice, there are 2–10 distinct voices (10 is the API's
  limit), and the spoken characters, tags included, fit the budget (1,800 by
  default, at most the advised 2,000). Also kept per segment: a segment with
  its own voice settings or a project speed or style other than the default
  (dialogue has one `settings` object with only stability and similarity),
  and an `[interrupting]` tag, which is reported to collapse several inputs
  onto one timestamp.
- **Settings.** Stability from the project's default voice settings,
  snapped to 0 / 0.5 / 1 for Eleven v3 (other values reportedly fail);
  `similarity` from `similarity_boost`, sent to v4 only (only stability is
  documented for v3 dialogue). Seed 1, as for first takes.
- **Context.** `previous_text` / `future_text` are the end and the start of
  the neighbouring paragraphs' plain text: at most 100 characters, counted
  in UTF-16 units so the limit holds however the server counts, cut at
  words. `previous_request_ids` holds the request ids of the directly
  preceding paragraphs that were themselves generated as dialogue (at most
  3, closest last; the API describes them as ids of dialogue generations).
  A 400/422 with any of these fields is retried once without them.
  Conversely, per-segment re-rolls no longer stitch onto a dialogue take's
  request id.
- **Validation** (`dialogue::cut`), before anything is stored: every
  `dialogue_input_index` is in range and each input has a segment; per input
  the span is min(start)–max(end) over its segments and must have a length
  and at least 0.04 s per spoken character (no tags, no spaces); spans run
  in order (20 ms of overlap tolerated); the last ends inside the
  audio (+0.15 s); every input has at least three 10 ms frames of sound.
  Character ranges are keyed on the input index, never the voice id (a
  voice-id mismatch is only logged): they must run contiguously from 0 over
  all of `alignment.characters`, and each input's slice must be its text.
  The research could check the index arithmetic only on ASCII, so the slice
  is compared exactly first and then after Unicode NFKC normalisation and
  whitespace folding, in the server's own index space: a server that sends
  "č" decomposed or "…" as three dots does not force a paid fallback, but
  different words still fail. Without an alignment the ranges must match
  the inputs' code-point counts.
- **Cutting.** Real voice segments are contiguous (the pause belongs to one
  of the two lines), so the cut is searched for: around the boundary (±0.3 s,
  never past the middle of either line, widened to the character times'
  end and start of speech when those agree with the voice segments) the
  10 ms frames quieter than the speech level minus 30 dB (−65…−40 dBFS; one
  10 dB relaxation) form pauses of at least 20 ms, measured over the whole
  stretch between the two lines' middles. A pause of at least 80 ms
  touching the boundary is the turn (the longest of those). Otherwise the
  timings are off by more than the pause, and the nearest quiet stretch may
  be a stop consonant inside a word, so each pause is weighed by its length
  minus its distance from the boundary: the turn's longer pause a little
  further away wins over a 40 ms gap right next to it (tested with the
  boundary 50–200 ms early or late and no character times). The chosen
  pause is cut in the middle, moved to the quietest sample within 2 ms. No
  pause means validation fails. An `alignment` without characters counts as
  no alignment (the counts are checked) instead of failing the paragraph.
  The first take starts at 0 and the last ends with the audio, so the takes
  back to back are exactly the dialogue; with 0 ms within a paragraph,
  export and paragraph playback keep its timing.
- **Fallback.** A failed validation, an unreadable answer or a refused
  request (4xx except 401) discards the dialogue and returns the paragraph's
  jobs to the per-segment loop, with a status message naming the paragraph
  and whether (and how much) the dialogue was billed; a billed, discarded
  dialogue counts in the run's "credits used". After two billed dialogues
  are discarded in one Generate run, the run sends no more dialogue
  requests (checked when a request would go out, so at most the requests
  already being validated follow): a systematic problem, such as a change
  in the API's answers, must not bill every paragraph twice. A 401, or a
  429/5xx still failing after the text-to-speech retry policy (6 rate-limit
  and 3 temporary retries), fails the segments instead: per-segment calls
  would meet the same problem and spend more.
- **Takes.** One WAV and one take row per segment, stored in one
  transaction. Hash: a group hash over all inputs' text and voice, the
  settings, model, seed and output format (`take.dialogue_group`, a new
  nullable column with an idempotent migration), and per take a hash of the
  group and the input index, which never equals a text-to-speech hash.
  Every take carries the dialogue's request id. The credits (header, or the
  characters as an inexact estimate) are split over the takes in proportion
  to their characters, largest remainder first, so they sum exactly.
- **Cache.** If every take of an identical paragraph exists with its file,
  they are reused without a request (the segment's own row, or a new free
  row). If only part of it is left, the paragraph is generated under a
  fresh random seed, so no existing file is overwritten and one hash keeps
  meaning one audio file (the take check shares results by hash).
- **Trimming and checking.** New dialogue takes are queued for the take
  check like any other. With automatic trimming on, only the paragraph's
  outer edges are trimmed (the first take's start, the last take's end);
  the pauses between speakers are left as the dialogue made them. **Auto-trim
  chapter** does the same for takes cut from a dialogue (a piece whose
  neighbour in the paragraph is from the same dialogue keeps that edge, and
  a middle piece is stored untrimmed); the per-take **✂ Auto-trim** button
  still trims the whole take, as asked.

Not verified here (api.elevenlabs.io was unreachable): real responses, how
precise v4's voice-segment timings are, how the server counts non-ASCII
characters, whether `character-cost` is sent on dialogue, and the cut
thresholds, which were tuned on synthetic audio.

### To verify against a real account

- ElevenLabs response header names for the request id and the character cost.
- Whether request stitching applies to `eleven_v3` (the setting can be turned
  off per project).
- Which PCM rates the plan allows.
- Text-to-Dialogue: that cuts land in the pauses on real v4 and v3 output
  (and on the MP3 fallback, whose decoder delay shifts the timings
  slightly), how often the fallback triggers, the `character-cost` header,
  and how non-ASCII text is counted in `voice_segments`; how far the
  voice-segment boundaries sit from the real pauses and how long real turn
  pauses are (the 80 ms turn threshold and the length-minus-distance choice
  were tuned on synthetic audio).
