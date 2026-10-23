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

### To verify against a real account

- ElevenLabs response header names for the request id and the character cost.
- Whether request stitching applies to `eleven_v3` (the setting can be turned
  off per project).
- Which PCM rates the plan allows.
