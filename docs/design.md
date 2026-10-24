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
- Credits: read from the `character-cost` response header (`x-character-count`
  as a fallback), parsed as a number and rounded. Without either header the
  take stores an estimate at the learned rate, flagged inexact (see below);
  never the text length, which overstates paid plans about 2x.

### Export details

- Chapter export: MP3 192 kbps CBR 44.1 kHz mono plus a FLAC master, both from
  one ffmpeg call (`asplit`). The FLAC master is the mastered version. The
  first build normalised with single-pass `loudnorm=I=-19:TP=-3.5:LRA=11`;
  that is replaced by RMS-targeted gain and a limiter, measured afterwards
  (see "Retail export" below).
- Book export: every fully approved chapter in one M4B (AAC 128 kbps) with
  chapter markers, book tags and the cover from an FFMETADATA file.
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

### Claude Code backend and costs (added after the first build)

Requested: use the user's Claude subscription through Claude Code instead of
paying for the API as well, and show estimated costs.

- **Backends.** `AppConfig.claude_backend` is `api_key` (default) or
  `claude_code`, plus an optional `claude_code_path`. `llm::ClaudeClient` has a
  transport: `Api { url, key }` (unchanged Messages API requests) or
  `ClaudeCode(Invocation)`. Prompts, schemas and validation are shared; both
  return the structured JSON to the same parsers.
- **Default.** The API key stays the default and Claude Code is opt-in in
  Settings: Anthropic's terms describe subscription logins as meant for
  ordinary, individual use of Claude Code, so the app should not route its
  users through a subscription by default (and must not if it is ever
  distributed).
- **Invocation** (`claude_code.rs`): `claude -p --output-format json
  --json-schema <schema> --model <full id> --effort <level> --system-prompt
  <prompt> --tools "" --strict-mcp-config --disable-slash-commands --safe-mode
  --setting-sources= --no-session-persistence --max-turns 8`, prompt on stdin,
  cwd `<library>/claude-cwd/`, 20-minute timeout with kill-on-drop, stdout and
  stderr read while stdin is written. Never `--bare` (it ignores the
  subscription login) and no `--max-budget-usd` (a tripped cap discards the
  finished answer). `--setting-sources=` is required: `--safe-mode` alone still
  applies a settings file's `env` block, which can swap in an API key.
- **Environment.** Removed: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, the
  Bedrock/Vertex/Foundry switches, `ANTHROPIC_PROFILE` and federation
  variables, `CLAUDE_CODE_SIMPLE`, `CLAUDECODE`, `CLAUDE_CODE_EFFORT_LEVEL`
  (it overrides `--effort`), `ANTHROPIC_MODEL`, `ANTHROPIC_BASE_URL` and the
  `ANTHROPIC_DEFAULT_*_MODEL` aliases. Kept: `CLAUDE_CONFIG_DIR`,
  `CLAUDE_CODE_OAUTH_TOKEN`, `HOME`, `PATH`, proxies. Set:
  `CLAUDE_CODE_MAX_OUTPUT_TOKENS=32000` (as the API's `max_tokens`), file
  mentions, attachments and auto memory off, `MAX_STRUCTURED_OUTPUT_RETRIES=5`.
- **Detection** (Settings): the binary (configured path, `PATH`,
  `~/.local/bin/claude`), `claude --version` (2.1.169 is the floor, as
  `--safe-mode` arrived then; 2.1.290+ recommended), and
  `claude --setting-sources= auth status` with the same environment (the flag
  must precede the subcommand). `claude.ai` and `oauth_token` are accepted;
  `api_key`, `api_key_helper` and `third_party` get "billed to the API, not
  your plan". When the isolated check says signed out but a plain one says
  signed in, the token probably sits in a settings `env` block. The real
  run's result stays authoritative.
- **Results.** stdout is one object, or an array whose last `type: result`
  element counts (a Stop hook causes that). Success needs `is_error: false`,
  `subtype: success` and `structured_output`. Errors are classified in order:
  usage limit (`api_error: usage_limit_reached`, "You've hit your …" with
  either apostrophe, or "usage limit reached"; not the throttle's "(not your
  usage limit)"), output cap (`api_error: max_output_tokens`, not
  `stop_reason`) → split the chunk, plan without the model ("Claude Opus is
  not available with the Claude Pro plan … run /logout and /login": checked
  before auth because of that `/login`, and answered with "choose Sonnet"),
  auth (not logged in, expired, 401; a leaked "Invalid API key" gets its own
  message), API credit, prompt too long, refusal, error subtypes, temporary
  throttling. No result object: stderr if any (an unknown option means the
  CLI is too old), else "exited without a result (possibly a usage limit)".
  Usage limit, auth and "unavailable" errors stop the whole pass instead of
  falling back chunk by chunk: unattributed paragraphs stay unsegmented, as
  after Cancel, and the automatic tag pass is skipped. Both passes report the
  error after their "finished" event, so it is the message left in the
  status bar. Runs always use `<library>/claude-cwd/` (created when missing;
  if it cannot be, the run fails rather than using the app's own working
  directory).
- **Usage tracking.** Every call's usage goes to `llm_usage` (pass, backend,
  model, input/output/cache tokens, cost), failed calls included, since a
  failed call can still spend tokens. Claude Code usage comes from
  `modelUsage` (never `usage`, which is zero on some errors) and its `costUSD`,
  a list-price estimate shown as "API-equivalent". API usage is priced from
  the table in `costs.rs` (Opus 5.5 $4/$20 per MTok, cache reads $0.20;
  Sonnet 5.5 $2/$10; Haiku 5.5 $0.10/$0.50; cache writes 1.25x input).
  Rows keep their project when a chapter is deleted.
- **Credit rate.** Billed credits per character are account- and
  date-specific (measured values range from 0.12 to 0.59 on paid plans while
  `/v1/models` says 1.0), so `costs::credit_rate` uses, in order: the rate
  learned from the last 50 exact paid takes of that model in the library
  (`take_cost` stores each paid take's model and spoken characters; at least
  3 takes and 200 characters), the `/v1/models` `character_cost_multiplier` ×
  `cost_discount_multiplier` (stored in `model_rate`), then the fallback
  table. The basis is always shown with the number.
- **Balance.** `GET /v1/user/subscription`: remaining = max(0,
  `character_limit` − `character_count`), reset from
  `next_character_count_reset_unix`. 401 `missing_permissions` or 403
  `insufficient_permissions` means the key lacks `user_read`, shown as
  "balance unavailable", not as a bad key. Fetched at start, after each
  generation batch (at most every 20 s; later requests wait) and on refresh.
- **Claude estimate.** `costs::estimate_claude` mirrors the worker's chunking
  (attribution windows of dialogue paragraphs with their overlap, then tag
  windows over all paragraphs) and assumes ~4 characters per token for
  English, 3 otherwise, plus Claude Code's own framing. It is labelled rough;
  retries are not included.

### Voice picker (added after the first build)

Requested: assign voices from the user's own account only ("I will browse
the library on the web"), so no public-library browsing and no automatic
casting.

- **List.** `GET /v2/voices?voice_type=non-default&page_size=100&include_total_count=false`,
  following `next_page_token` while `has_more` (at most 100 pages; a repeated
  or missing token stops the loop; duplicates across pages are dropped).
  `non-default` is personal + workspace + community voices, the last being
  Voice Library voices added to My Voices; ElevenLabs' default voices are
  left out. Voices are parsed field by field from JSON because real
  responses carry explicit nulls (`samples`, `settings`, `sharing`,
  `description`, `preview_url`); a voice without `voice_id` is skipped, not
  the page. `category` stays a string. Labels are lowercased by key.
- **Loading.** When the Voices dialog opens with a key set, once per key (and
  again after an error); *Refresh* reloads. A list belongs to the key it was
  requested with; if the key changes while it loads, it is loaded again. The list lives in memory only.
  Errors are shown inline: 401 `missing_permissions` / 403
  `insufficient_permissions` as "the API key lacks the voices_read
  permission", other 401s as a rejected key, network errors without the URL.
- **Assigned ids outside the list** (a default voice picked earlier, a pasted
  id) are looked up with `GET /v1/voices/{id}` once no text field has focus:
  found ones show their name, a 404 (or a 400 whose detail is
  `voice_not_found`, also recorded) shows "⚠ unknown id", and other failures
  leave "not in your voices" (retried on Refresh). The id is always kept as
  typed; generation decides.
- **Picker** (`app/voice_picker.rs`, logic in `voices.rs`). Search splits the
  query into words, and every word must begin a word of the name,
  category, description, label values or language names (so "male" does not
  match "female"); a run-together word also offers its parts ("DeepCalm" →
  "calm", "Read2024" → "2024"), and the voice id counts as one whole word.
  Facets (category, gender, age, accent, language) list
  only values present in the loaded voices; languages come from the
  `language` label and `verified_languages`, normalised to codes. The list is
  virtualised (`show_rows`): every row takes exactly its fixed height (its
  contents go into a child `Ui` that does not move the list's cursor), which
  is what `show_rows` assumes, and the name line, two description lines and
  "Used by" fit inside it (a layout test checks both). *Assign* only edits the open Voices dialog;
  *Save* there writes `voice_map` as before. "Used by" counts the dialog's
  current, unsaved rows.
- **Previews.** Sources in order: `preview_url`, verified-language previews
  (the project's language first), then the first uploaded sample via
  `GET /v1/voices/{id}/samples/{sample_id}/audio`. Preview URLs are public
  storage objects on another host, so the API key is never sent with them;
  the content type (served as `text/plain`) is ignored and the bytes are
  sniffed (WAV by its header, otherwise MP3 via `audio::decode_mp3`). Files
  are cached as `<library>/voice-previews/<voice_id>-<10 hex of the
  source>.mp3|wav`, keyed by URL so an edited voice's new preview is fetched
  again (older files of that voice are removed). Only audio that decodes is
  cached; a cached file that no longer decodes is deleted. All cached sources
  are tried before any download. Playback goes through the app's player
  with the sentinel id `-1`, so no segment row lights up; a second click
  stops it, and closing the Voices dialog stops a preview.
- **Free plan.** When the balance says `tier: free`, the picker notes that
  ElevenLabs may refuse Voice Library voices for API use (reported as 402
  `payment_required`; not verified here).

### Retail export (added after the first build)

Requested: "M4B gets cover art, author and narrator metadata; the app
produces an ACX compliance report (loudness, peak and noise floor per
chapter) and a short retail sample clip."

- **Why the loudness pipeline changed.** Single-pass `loudnorm` runs in
  dynamic mode and targets integrated loudness, not RMS: on test speech the
  whole-file RMS landed anywhere from -21.3 to -18.2 dB depending on how many
  pauses a chapter has, 0.2 dB from ACX's ceiling, and the output was never
  measured. Now (`export/mastering.rs`): measure the assembly WAV's RMS in
  Rust, then one ffmpeg call with `volume=<-20 - RMS>dB,alimiter=limit=0.6310
  (-4 dBFS):level=false:latency=true:attack=5:release=50,aresample=44100`
  (`level=false` matters: alimiter's default auto-level would push the RMS to
  about -16 dB). The encoded file is measured; outside -22.5…-18.5 dB RMS or
  above -3.5 dB peak it is encoded once more with a corrected gain (and a
  lower ceiling if the peak overshot). LAME 192k lowers RMS by about 0.26 dB,
  which the measurement on the encoded file absorbs. The gain is clamped to
  ±30 dB; a silent file gets none. alimiter's `latency` option only exists
  from FFmpeg 5.1 and older builds reject unknown options (every export
  would fail on Ubuntu 22.04's 4.4), so it is passed only when `ffmpeg -h
  filter=alimiter` lists it; without it the audio is 5 ms (the attack)
  later. `amix normalize` needs FFmpeg 4.4, the documented minimum.
- **Room tone.** Assembly inserts digital zeros, whose floor measures -inf
  and which ACX Check calls "dead silence". `Publication.room_tone` (on by
  default) mixes `anoisesrc=color=pink:amplitude=0.0012:seed=7`
  (about -72.7 dB RMS, a -73.4 dB floor) in after the gain stage (`amix
  duration=first normalize=0`, both set explicitly), so the floor does not
  move with the gain, and after the sample's fades, so faded ends do not
  measure as silence. Whether ACX rejects digital silence is not verified.
- **Meter** (`acx.rs`). ffmpeg decodes to mono 44.1 kHz f32 on stdout
  (stderr is discarded, so a chatty decode cannot block). Per 50 ms block it
  keeps n, Σx and Σx²: whole-file RMS with the DC offset removed (variance),
  sample peak, noise floor = the quietest 0.5 s window at a 0.1 s hop over
  complete blocks (whole file when shorter), head and tail = runs of blocks
  below -50 dBFS mean square, the last block counted by its real length.
  This is the ACX Check plugin's method (McCown: RMS of the quietest 500 ms;
  Daulton: 0.4 s, 0.1 s hop). ffmpeg's astats `Noise floor dB` is a minimum
  of windowed peak maxima and reads -inf on real room tone, so it is not
  used. The peak is taken before DC removal (Daulton does not remove DC).
- **Checks.** RMS: pass -23…-18, warning -25…-23 and -18…-16, fail beyond.
  Peak: fail above -3, pass -6…-3, warning -9…-6, fail below -9. Floor:
  fail above -60, warning below -90 or -inf. Head/tail room tone: fail above
  5 s (ACX's Dec 2021 rule), warning below 0.5 s (head) or 1 s (tail); the
  legacy "0.5–1 s head" rule is not enforced because the operative rule is
  the 5 s limit and the measured head is about 1.05 s with the 750 ms
  lead-in (takes carry their own leading silence). ffprobe: codec mp3,
  44.1 kHz, ≥192 kbps (the audio stream's bit rate; the format's includes the
  cover). CBR: the first frame's `Info` tag (ffmpeg rewrites `Xing` to
  `Info` when no frame's rate differed); `Xing`/`VBRI` fail, no tag warns.
  Length: fail above 120 min, warning above 119; size: fail above 170 MB.
  Samples: no head/tail check, fail above 5 min, warning under 1 min. A
  report "passes" only when every line passes.
- **Storage.** `export_report` (one row per chapter, `ON DELETE CASCADE`)
  and `retail_sample` (one per project, the source chapter `ON DELETE SET
  NULL`), created with `CREATE TABLE IF NOT EXISTS` from `db/retail.rs`
  (plus an idempotent `ADD COLUMN takes_key`). -inf levels are stored as
  NULL. Each row keeps its checks as JSON. A final export replaces the row;
  a draft export replaces only a draft (an upsert with `WHERE excluded.draft
  = 0 OR export_report.draft = 1`), so `ACX report.md` keeps describing the
  delivery file and the status line says so. Each row also keeps
  `takes_key`, a hash of the chapter's segments in order with their current
  take and approval at export time (`retail::takes_key`); the report is
  "outdated" when that no longer matches (a new take, an older take picked
  again, an approval changed, segments re-split). The open chapter's chip
  compares against the segments on screen, so it follows an approval at
  once. Rows without a key fall back to "a take was created after
  `checked_at`".
- **Tags** (`retail.rs`). ID3v2.3 (`-id3v2_version 3 -write_id3v1 1`):
  title, album, artist, album_artist, composer (narrator), genre, date
  (TYER), track n/N (position among the project's chapters), publisher
  (TPUB), copyright (TCOP), TIT3 (subtitle); description is left out because
  ffmpeg cannot write COMM. Cover: `-c:v copy -disposition:v:0 attached_pic`
  with stream comment "Cover (front)" (APIC type 3); never re-encoded. FLAC:
  the same as Vorbis comments plus the picture. M4B: FFMETADATA with title,
  album (title: subtitle, as Audiobookshelf writes it), artist,
  album_artist, composer, genre, date, comment/description/synopsis,
  copyright, `media_type=2` (stik Audiobook), chapters; `-brand "M4B "`.
  ffmpeg drops `publisher` for MP4, so the copyright line falls back to
  "© year publisher". Narrator display in Apple Books is not verified
  (ffmpeg cannot write ©nrt).
- **Cover check.** `imagesize` (jpeg, png features) for type and size; the
  colour model from the first JPEG SOFn component count (3 RGB/YCbCr, 4
  CMYK, 1 grey) or the PNG IHDR colour type. Warnings only. The chosen file
  is copied to `<library>/covers/project-<id>-<10 hex of its content>.<ext>`
  on Save; a cover that went missing is reported and the export continues
  without art.
- **Retail sample.** Planned from the chapter's segments and their current
  takes' WAV headers (pure `retail::plan_sample`, shown live in the dialog):
  from the selected segment or the chapter start, the same pauses as the
  chapter, 0.5 s room tone before and 1 s after, up to the chosen length
  (1–5 min), ending at the last paragraph end that fits, else the last
  segment, else cut inside a single long segment. It stops before the first
  segment that is not approved (or whose take cannot be read), and before a
  paragraph that has no segments (a placeholder source, so its text is never
  skipped over). Fades 0.5 s in and 3 s out on the voice. The worker plans
  again from the database.
- **Report.** `ACX report.md` (one table, the book-level checks, the
  sample, every problem in words, the rules and the policy note) is
  rewritten after every chapter and sample export. The profile is called
  "ACX/Findaway technical spec", and the AI-narration policy note is
  information only.

### To verify against a real account

- ElevenLabs response header names for the request id and the character cost.
- Whether request stitching applies to `eleven_v3` (the setting can be turned
  off per project).
- Which PCM rates the plan allows.
- Whether `/v2/voices` sends `total_count` when it is not asked for (it is
  read as optional and not used).
