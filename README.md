# Narration Studio

A native Linux desktop app that turns long prose into multi-voice audiobook
narration through the ElevenLabs API. Claude attributes speakers and directs
the performance with ElevenLabs audio tags (`[whispers]`, `[thoughtful]`,
`[long pause]`), in narration as well as dialogue. Every segment is reviewed
individually: approve it, or re-roll it with a new seed. Approved chapters are
then assembled and exported as MP3 (ACX spec), FLAC and M4B with chapter
markers.

You work on one chapter at a time, in two tabs:

| 1 Text: paste the chapter as plain prose | 2 Review: speakers, audio tags, takes |
|---|---|
| ![Text tab](docs/screenshots/text.png) | ![Review tab](docs/screenshots/review.png) |

The design and the reasoning behind it are in [docs/design.md](docs/design.md),
including how this first build settles the open decisions.

## Build

Requirements:

- Rust 1.95 or newer (eframe 0.37 requires it)
- Build dependencies: `pkg-config`, `libasound2-dev` (ALSA headers for audio playback)
- Runtime: `ffmpeg` 4.4 or newer with `libmp3lame`, and `ffprobe`, on `PATH`
  (export and its ACX checks only), and a desktop session (X11 or Wayland)
- For the local take check (the default `whisper` feature, which compiles
  whisper.cpp): `cmake`, a C and C++ compiler, and libclang (any
  `libclang1-NN`, which `clang` pulls in, or `libclang-dev`). The first build
  takes about a minute longer. `cargo build --no-default-features` builds
  without Whisper; the take check then only checks lengths.

```sh
sudo apt install pkg-config libasound2-dev ffmpeg build-essential cmake clang   # Debian/Ubuntu
cargo build --release
./target/release/narration-studio
```

whisper.cpp is compiled for the building machine's CPU (`-march=native`). To
build a binary for other machines, set `GGML_NATIVE=OFF` and run
`cargo clean -p whisper-rs-sys` first (cargo does not notice the variable).

Run the tests with `cargo test`. The export tests also encode with ffmpeg and
inspect the files with ffprobe when they are installed. The pipeline tests drive the workers against local fake
Claude and ElevenLabs servers, so they need no API keys.

## Setup

1. **Settings**: choose how Claude runs (see [Claude](#claude) below) and
   enter your ElevenLabs API key. Keys are stored in
   `~/.config/narration-studio/config.json` (mode 0600). The environment
   variables `ANTHROPIC_API_KEY` and `ELEVENLABS_API_KEY` are used when the
   fields are empty. Appearance is light by default; a dark theme is available.
2. The project menu's **＋ New project…** creates a project. Choose its
   language, which sets the quote styles.
3. **Project**: pick the TTS model (default `eleven_v4`; v4, v4 Turbo and v3
   perform audio tags) and the output format (`pcm_44100` needs the Pro plan or
   higher; `mp3_44100_192` is the fallback). Set the concurrency to your plan's
   limit. Under *Audio tags*, choose the density (light, moderate, rich) and
   add director's notes for the whole book
   ([screenshot](docs/screenshots/project-settings.png)).
4. **Voices**: give every speaker a voice, including the narrator.
   *Choose…* picks one of the voices in your ElevenLabs account; a pasted
   voice id works too (see [Voices](#voices) below).

## Voices

The app offers the voices in your own ElevenLabs account: the ones you
created, cloned or designed, and the Voice Library voices you added to
*My Voices* on elevenlabs.io. It does not browse the public library; add
voices there on the web, then press *⟳ Refresh*
([screenshot](docs/screenshots/voice-picker.png)).

- **Voices dialog.** The list loads when the dialog opens (and stays loaded
  until you refresh it or change the API key). Next to each voice id it shows
  the voice's name, a ▶ preview and *Choose…*. An id that is not in your
  account (an ElevenLabs default voice, say) is looked up by id, and one
  ElevenLabs does not know is marked *⚠ unknown id*; either way the id is
  kept as you typed it.
- **Choose a voice for …** lists your voices with their category, labels and
  description. Search matches the start of any word in the name, labels,
  description or category (`brit narr`; `calm` also finds *DeepCalm*, and
  a pasted voice id finds its voice); the Category, Gender, Age, Accent
  and Language filters offer only the values your voices have. *Used by:
  vale, narrator* marks voices other speakers already have. *Assign* puts the
  voice into the dialog; *Save* there keeps it.
- **Previews** cost no credits. ▶ downloads a voice's preview once (a
  spinner shows meanwhile) and keeps it in `voice-previews/`; a second click
  stops it. Voices without a preview file fall back to a preview in a
  verified language (the project's first), then to the sample you uploaded.
- The API key needs the *Voices: Read* permission (`voices_read`); without
  it the dialog says so, and pasted ids still work. On the Free plan,
  ElevenLabs may refuse Voice Library voices for API use.

## Claude

Claude attributes speakers and adds audio tags. Settings → *Claude* offers two
ways to run it:

- **Claude Code — your Pro/Max subscription (the default).** The app runs your own
  [Claude Code](https://code.claude.com) CLI (`claude -p`), so the calls use
  your plan instead of API billing. Setup:
  1. Install Claude Code (2.1.290 or newer; `claude update` upgrades it).
  2. In a terminal, run `claude auth login` once and choose your Claude
     subscription. Narration Studio never asks for or stores a Claude token.
  3. In Settings, pick *Claude Code*. The section shows the version, whether
     you are signed in and how; *Check again* re-runs the check. If `claude`
     is not on the app's `PATH` or in `~/.local/bin`, set its path there.

  The calls count against your plan's usage limits, which are shared with
  the Claude apps (claude.ai, desktop, mobile). When a limit is reached, the
  run stops with the reset time and leaves unattributed paragraphs as they
  are; *Re-segment drafts* continues later. If your plan does not include
  the project's model (Claude Code says e.g. "Claude Opus is not available
  with the Claude Pro plan"), set Project → Claude → Model to
  `claude-sonnet-5-5`. This backend is meant for your own use on your own
  computer.

  Every run is isolated from your Claude Code setup: no tools, MCP servers,
  hooks, skills or settings files, no session history, and an empty working
  directory. `ANTHROPIC_API_KEY` (and `ANTHROPIC_AUTH_TOKEN`,
  `ANTHROPIC_BASE_URL`) are removed from its environment, because in this
  mode Claude Code would otherwise bill that key instead of your plan;
  Settings says so when one of them is set. If Settings reports
  *billed to the API, not your plan*, Claude Code is signed in with an API
  key: run `claude auth login` and choose your subscription.
- **Anthropic API key — pay per use.** Requests go to the Messages API and are
  billed per token. Pick it in Settings if you prefer API billing or have no
  Claude subscription.

## Costs

- **ElevenLabs balance.** The top bar shows the credits left and the reset
  date (hover for plan, usage and voice slots; ⟳ refreshes). It is fetched at
  start, after each generation batch, and on refresh. A key without the
  `user_read` permission shows *balance unavailable*; everything else works.
- **Before generating.** The Generate dialog estimates the credits, says where
  the rate comes from, and the share of your remaining balance. It warns when
  the batch exceeds the balance but never stops you. The Review toolbar shows
  *Drafts ≈ N credits*.
- **Where the rate comes from.** Credits per character differ by plan and
  change over time, so the best rate is learned from your own takes: the
  `character-cost` ElevenLabs reports for your last 50 takes of that model.
  Before there are enough takes, the model's list rate from ElevenLabs is
  used, then a built-in table (v4/v3/Multilingual 1.0, Flash/Turbo 0.5 per
  character). A take that comes back without a `character-cost` is recorded
  with this estimate and marked `≈`.
- **Money.** Optionally set your price per 1,000 credits (and a currency) in
  Settings to see money next to credit estimates.
- **Claude.** The Text tab gives a rough token estimate for *Segment & add
  audio tags* before you run it. The Review toolbar shows what the chapter's
  Claude calls used, per pass on hover: tokens and the API list price
  (Opus 5.5: $4 / $20 per million input/output tokens). With Claude Code the
  figure is labelled *API-equivalent (subscription)*: Claude Code reports
  it, but your plan's usage limits apply instead of a bill.

## Workflow

1. **Text tab**: paste the chapter as plain prose, or *Open text file…*.
   Paragraphs are separated by blank lines; a line like `***` or `#` marks a
   scene break. The text stays editable until the chapter has audio. The
   chapter menu at the top switches chapters or starts a new one.
2. **Segment & add audio tags** saves the text, splits paragraphs into
   narration and dialogue, and attributes speakers. Every result is checked
   against the source text, and a paragraph that keeps failing falls back to a
   quote parser with speaker `unknown`. Claude then inserts audio tags into
   narration and dialogue. Tags may only be added: if stripping them doesn't
   give back the exact source words, the suggestion is rejected.
3. **Review tab**: fix attribution and tags while it is free. Change speakers,
   merge or split segments (*Split…* takes `|` markers), and edit a segment's
   tags in the detail panel. **✦ Audio tags** re-runs the tagging for all
   draft segments. Segments whose speaker has no voice show ⚠ and are
   skipped.
4. **Generate N drafts**. A confirmation shows the character count, the
   estimated credits and how many paragraphs go through multi-speaker
   dialogue first. Identical inputs reuse cached takes at no cost.
5. Review with the keyboard and approve or re-roll. Earlier takes stay
   available in the take picker.
6. **Export chapter** once every segment is approved, or use **Draft export**
   at any time. **Export book (M4B)** combines all fully approved chapters.
   Every export is mastered and checked against the ACX spec (see
   [Retail export](#retail-export)).

## Retail export

Exports are ready for retail: loudness to the ACX spec, the book's metadata
and cover art in every file, an ACX report and a retail sample
([screenshot](docs/screenshots/retail.png)).

- **Publication** (Project settings): title (the project name when empty),
  subtitle, author, narrator, publisher, year, genre (`Audiobook`),
  copyright and description, plus the cover. *Choose…* picks a JPG or PNG;
  on *Save* it is copied into the library (`covers/`). The dialog checks the
  picture without opening it: ACX wants it square, at least 2400×2400 px,
  RGB (not CMYK, greyscale or palette), at most 8 MB. Anything else gets an
  amber note; the cover is used anyway.
- **What goes into the files.** Chapter MP3s get ID3v2.3 tags: chapter
  title, book title as album, author as artist and album artist, narrator as
  composer, track *n/N*, genre, year, publisher, copyright, subtitle, and the
  cover (*Embed the cover in the chapter MP3s* can turn that off, in case a
  retailer's uploader objects). The FLAC masters get the same tags and the
  cover. The M4B gets the book's tags (album carries the subtitle,
  `media_type` marks it as an audiobook, the publisher goes into the
  copyright line because MP4 has no publisher field), the cover and the
  chapter markers.
- **Loudness.** Each file is brought to -20 dB RMS under a -4 dB limiter,
  encoded, and measured again; one correction pass follows if it landed
  outside -22.5…-18.5 dB RMS or above -3.5 dB peak. *Room tone* (on by
  default) lays a very quiet pink-noise bed (about -73 dB) under the audio,
  so pauses are room tone instead of digital silence, which ACX Check flags
  as "dead silence".
- **ACX report.** After every chapter export the MP3 is measured and checked:
  RMS -23…-18 dB, peak at most -3 dB, noise floor (the quietest half second)
  at most -60 dB, 44.1 kHz, 192 kbps constant bit rate, at most 120 minutes
  and 170 MB, and 0.5–5 s of room tone at the start and 1–5 s at the end.
  Each check is *pass*, *warning* (close, or worth a look) or *fail*, with
  the bands of the ACX Check plugin. The Review toolbar shows the open
  chapter's result (*ACX ✓*, *ACX ⚠ …*, *ACX ✗ RMS -24.1*; *draft* when
  it came from a draft export, *outdated* when segments, takes or approvals
  changed since);
  hover for every check, click for the **Retail & ACX** window (also in the
  ⋯ menu), which lists all chapters, the book-level checks (cover, mono or
  stereo throughout, retail sample, opening and closing credits) and opens
  the export folder. `ACX report.md` in the export folder has the same
  table and is rewritten after every export. A draft export is checked
  too, but it only stands in until the chapter's first real export; after
  that the report keeps describing the delivery file.
- **Retail sample.** *Export retail sample…* in that window builds
  `<Book title> - Retail Sample.mp3` from approved takes only, starting at
  the segment selected in Review (or the chapter start), 1 to 5 minutes
  (3 by default), ending at a paragraph end where it can, faded in over
  0.5 s and out over 3 s, mastered and tagged like the chapters. If the
  range reaches a segment that is not approved, or a paragraph that has no
  segments yet, the sample stops before it and says so. ACX wants up to 5
  minutes from the book itself (not the credits) and recommends the
  opening.
- **What this is not.** The checks cover the technical audio spec
  ("ACX/Findaway technical spec"), not acceptance. ACX does not accept
  unauthorized AI or text-to-speech narration; Findaway Voices by Spotify
  accepts ElevenLabs narration and labels it as digitally narrated. Nothing
  here blocks an export.

### Keyboard

| Key | Action |
|-----|--------|
| Space | Play / stop the selected segment |
| P | Play the whole paragraph, gapless |
| A | Approve and move to the next segment |
| R | Reject and re-roll with a new seed (generate, if there is no audio yet) |
| ↑ / ↓ | Previous / next segment |
| ← / → | Previous / next paragraph |

### Silence trimming

ElevenLabs takes start and end with uneven silence, so even "0 ms within a
paragraph" leaves uneven gaps. A trim cuts a take's leading and trailing
silence, never the middle, with a short fade at each cut
([screenshot](docs/screenshots/trim-waveform.png)).

- The Review detail panel shows the current take's waveform. The cut-off
  parts are shaded. Drag the start or end handle to trim by hand.
  **Auto-trim** detects the silence with the project's settings, and **Reset
  trim** restores the whole take. Approved segments are locked: their
  waveform is shown, but the trim can only change after unapproving.
- Automatic trimming of new takes is **off by default**. Turn it on in
  **Project → Silence trimming**, which also sets the threshold (-50 dBFS),
  the silence kept before (40 ms) and after (80 ms) the speech, and the fade
  (10 ms). While it is on, the Review toolbar has **✂ Auto-trim chapter**. It
  re-detects the trim of every unapproved segment's current take and replaces
  earlier trims. The fade applies whenever a trimmed take is played or
  exported, so changing it also affects approved trimmed takes.
- Trimming is free and non-destructive. The trim is stored on the take in
  the database. The WAV file and the take hash stay as they are. Playback,
  paragraph playback and every export use the trimmed audio.

### Take checking

Every new take is transcribed on this computer with Whisper and compared
with the segment's text. A take whose words differ, or whose length is
unusual for its text, is **flagged** for a listen. A flag never rejects a
take, never blocks approving it and never costs credits
([screenshot](docs/screenshots/take-check.png)).

- **Settings → Take checking**: pick a model and **Download** it (from
  Hugging Face, into the library's `models/` folder; the checksum is verified
  before the file is kept; a download that receives nothing for a minute
  gives up). *small* (466 MiB, about 780 MB of memory, about
  5 s per take with 4 threads) is the default. *base* is faster and less
  accurate. *Large v3 Turbo, 5-bit* is the better choice for Czech and other
  non-English projects, at about five times the cost of *small*. The check
  uses half the CPU cores by default and runs one take at a time at the
  lowest priority. Set `HF_TOKEN` to send a Hugging Face token.
- Without a model (or in a build without Whisper) only the free **length
  check** runs: a take much shorter or longer than its text needs at the
  voice's speed is flagged.
- **Review**: a flagged segment shows an amber ⚠ next to its status; hover it
  for the reason. The toolbar's **⚠ N flagged** chip selects the next flagged
  segment. **Check chapter** queues the current takes that have no result yet
  (for example after installing a model) and shows the progress; **Stop**
  empties the queue.
- The detail panel's **Take check** shows the text with the differences
  marked: words that were not heard are struck through in red, words that
  were heard but are not in the text are highlighted. Below it are what
  Whisper heard, the reason for a flag and the length. **Re-check** runs the
  check again, **Dismiss flag** marks the take as fine, and **↻ Re-roll**
  generates a new take (costs credits, like R).
- Differences in punctuation, case, quotes, dashes, British/American
  spelling, numbers written as digits or words, contractions and fillers
  ("um", laughter) are ignored. An audio tag that was read aloud
  (`[whispers]` heard as "whispers") is flagged. Short dialogue tags are
  judged leniently, and non-English text more leniently than English.
- Checking new takes automatically can be turned off in Settings; **Check
  chapter** and **Re-check** work either way.

### Multi-speaker dialogue

A paragraph that goes back and forth between speakers is generated as one
ElevenLabs Text-to-Dialogue request instead of one request per segment,
which gives more natural turn-taking. The result is cut back into one take
per segment in the pauses between the speakers, so every segment is still
reviewed, approved and re-rolled on its own
([screenshot](docs/screenshots/dialogue.png)).

- On by default, for Eleven v4 and v3 (**Project → Multi-speaker
  dialogue**). Eleven v4 Turbo and the older models generate per segment.
- A paragraph qualifies on its **first generation** when the request
  covers all of its segments, all are drafts without audio, they use 2 to
  10 different voices and their spoken text (tags included) is at most
  1,800 characters (adjustable up to ElevenLabs' advised 2,000). Re-rolls,
  single segments, a segment with its own voice settings, a project speed
  or style other than the default (dialogue takes only stability and
  similarity) and an `[interrupting]` tag (it is known to break the
  dialogue's timings) keep per-segment requests.
- The **Generate** confirmation says how many paragraphs go through
  dialogue. The neighbouring paragraphs' text is sent as context, audio tags
  are performed, and identical paragraphs reuse their takes for free.
- Before cutting, the response is checked: every line has its own stretch of
  audio, in order, long enough for its text and with sound in it, and the
  character ranges match the lines exactly. Each cut goes in the middle of
  the pause between two lines, never inside speech; played back to back,
  the takes are the dialogue exactly. If any check fails, the dialogue audio
  is discarded and the paragraph is generated per segment instead, which
  bills its characters a second time; the status bar says so, and the
  discarded dialogue counts in the credits used. After two such paragraphs
  in one generation, the rest of it goes per segment straight away.
- Takes cut from a dialogue are marked **dialogue** in the take list. Each
  carries the request id and its share of the request's credits, by
  characters. Re-rolling a segment later generates it on its own.
- With automatic silence trimming on, and with **Auto-trim chapter**, only
  the paragraph's outer edges are trimmed: the pauses between speakers are
  the dialogue's own timing.

## Data

All state lives in one library directory (default
`~/.local/share/narration-studio/`, changeable in Settings):

```
library.sqlite              projects, characters, chapters, paragraphs, segments, takes,
                            Claude usage, credit rates, ACX checks of the exports
audio/<take_hash>.wav       every take, 16-bit mono PCM (opens in Audacity)
exports/<project>/          chapter MP3 + FLAC, book M4B, retail sample, ACX report.md
covers/                     cover art copied in from Project settings
claude-cwd/                 empty working directory for Claude Code runs
voice-previews/             downloaded voice previews (<voice_id>-<url hash>.mp3)
models/ggml-<model>.bin     Whisper models for the take check
```

Every pipeline stage resumes from this state after a restart.

## Code map

| File | Role |
|------|------|
| `src/segment.rs` | Paragraph splitting, language-aware quote parser, exact-text validator, split/merge helpers |
| `src/tags.rs` | Audio-tag parsing and the check that tags never change the words |
| `src/llm.rs` | Claude attribution and audio-tag direction requests (structured JSON output), API or Claude Code transport |
| `src/claude_code.rs` | Claude Code CLI: lookup, version and sign-in check, isolated `claude -p` runs, result and error parsing |
| `src/costs.rs` | Claude prices, credit-rate learning, token and credit estimates, number formats |
| `src/tts.rs` | ElevenLabs text-to-speech |
| `src/tts/account.rs` | ElevenLabs balance and model rates |
| `src/tts/voices.rs` | Your ElevenLabs voices, lookups by id, preview downloads |
| `src/voices.rs` | Voice picker search and filters, "used by", preview sources and the preview cache |
| `src/dialogue.rs` | Multi-speaker dialogue: which paragraphs qualify, inputs and settings, hashes, credit split, context text |
| `src/dialogue/cut.rs` | Validating a dialogue response and finding the cut points in the pauses |
| `src/dialogue/client.rs` | ElevenLabs Text-to-Dialogue request and response |
| `src/worker.rs` | Command/event channels, segmentation pass, TTS worker pool |
| `src/worker/account.rs` | Fetching the ElevenLabs balance and model rates |
| `src/worker/claude.rs` | Claude backend selection, usage recording, stopping a pass on usage limits |
| `src/worker/voices.rs` | Voice list, lookups and cached previews |
| `src/worker/retail.rs` | Chapter, book and retail-sample exports with their ACX checks |
| `src/worker/autotrim.rs` | Trim jobs: automatic trimming of new takes, Auto-trim chapter |
| `src/worker/dialogue.rs` | Generating a paragraph as one dialogue, cutting it into takes, falling back per segment |
| `src/worker/takecheck.rs` | The low-priority checker thread, take-check commands, model downloads |
| `src/db.rs` | SQLite schema and queries |
| `src/db/costs.rs` | Claude usage, take costs and model rates |
| `src/db/retail.rs` | Export reports and the retail sample |
| `src/db/takecheck.rs` | `take_check` table queries |
| `src/db/dialogue.rs` | Storing a dialogue paragraph's takes together |
| `src/audio.rs` | PCM/WAV handling, MP3-fallback decode |
| `src/trim.rs` | Silence trimming: detection, cut and fade, waveform peaks, loading a take's audio with its trim |
| `src/export.rs` | Streaming assembly with silences |
| `src/export/mastering.rs` | Loudness, tags, cover art, retail sample |
| `src/acx.rs` | ACX meter (RMS, peak, noise floor, room tone), ffprobe and CBR checks, pass/warning/fail bands, `ACX report.md` |
| `src/retail.rs` | Publication metadata and tag mapping, cover checks, retail-sample planning, book-level checks |
| `src/check.rs` | Take check: settings, results, length check, hallucination filter, combining the findings |
| `src/check/text.rs` | Text normalisation, word alignment and the flag thresholds |
| `src/check/resample.rs` | Anti-aliased resampling to Whisper's 16 kHz input |
| `src/check/models.rs` | Whisper model catalog, checksum verification, download |
| `src/check/whisper.rs` | whisper-rs backend (feature `whisper`) |
| `src/hash.rs` | Take hash |
| `src/player.rs` | rodio playback queue |
| `src/app.rs` | egui UI (Text and Review tabs, dialogs) |
| `src/app/costs.rs` | Balance, estimates, Claude settings |
| `src/app/voice_picker.rs` | Voice picker and previews |
| `src/app/retail.rs` | Publication settings, ACX chip, Retail & ACX window, retail sample |
| `src/app/waveform.rs` | Take waveform with trim handles, Auto-trim buttons, trim settings |
| `src/app/check.rs` | Take-check marker, toolbar chip, detail section with the highlighted differences, settings |
| `src/app/dialogue.rs` | Dialogue setting, the Generate confirmation's note, the take list's marker |
| `src/theme.rs` | Light (default) and dark themes, semantic colours |
