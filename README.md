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
- Runtime: `ffmpeg` with `libmp3lame` on `PATH` (export only), and a desktop
  session (X11 or Wayland)

```sh
sudo apt install pkg-config libasound2-dev ffmpeg   # Debian/Ubuntu
cargo build --release
./target/release/narration-studio
```

Run the tests with `cargo test`. The export test also encodes with ffmpeg when
it is installed. The pipeline tests drive the workers against local fake
Claude and ElevenLabs servers, so they need no API keys.

## Setup

1. **Settings**: enter your Anthropic and ElevenLabs API keys. They are stored
   in `~/.config/narration-studio/config.json` (mode 0600). The environment
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
4. **Voices**: give every speaker an ElevenLabs voice id, including the
   narrator. *Load my ElevenLabs voices* fills a picker from your account.

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
4. **Generate N drafts**. A confirmation shows the character count first.
   Identical inputs reuse cached takes at no cost.
5. Review with the keyboard and approve or re-roll. Earlier takes stay
   available in the take picker.
6. **Export chapter** once every segment is approved, or use **Draft export**
   at any time. **Export book (M4B)** combines all fully approved chapters.

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

## Data

All state lives in one library directory (default
`~/.local/share/narration-studio/`, changeable in Settings):

```
library.sqlite              projects, characters, chapters, paragraphs, segments, takes
audio/<take_hash>.wav       every take, 16-bit mono PCM (opens in Audacity)
exports/<project>/          chapter MP3 + FLAC, book M4B
```

Every pipeline stage resumes from this state after a restart.

## Code map

| File | Role |
|------|------|
| `src/segment.rs` | Paragraph splitting, language-aware quote parser, exact-text validator, split/merge helpers |
| `src/tags.rs` | Audio-tag parsing and the check that tags never change the words |
| `src/llm.rs` | Claude attribution and audio-tag direction requests (structured JSON output) |
| `src/tts.rs` | ElevenLabs text-to-speech and voice list |
| `src/worker.rs` | Command/event channels, segmentation pass, TTS worker pool, export jobs |
| `src/worker/autotrim.rs` | Trim jobs: automatic trimming of new takes, Auto-trim chapter |
| `src/db.rs` | SQLite schema and queries |
| `src/audio.rs` | PCM/WAV handling, MP3-fallback decode |
| `src/export.rs` | Streaming assembly with silences; ffmpeg loudnorm and encode |
| `src/trim.rs` | Silence trimming: detection, cut and fade, waveform peaks, loading a take's audio with its trim |
| `src/hash.rs` | Take hash |
| `src/player.rs` | rodio playback queue |
| `src/app.rs` | egui UI (Text and Review tabs, dialogs) |
| `src/app/waveform.rs` | Take waveform with trim handles, Auto-trim buttons, trim settings |
| `src/theme.rs` | Light (default) and dark themes, semantic colours |
