# Narration Studio

A native Linux desktop app that turns long prose into multi-voice audiobook
narration through the ElevenLabs API. Claude attributes speakers, and every
segment is reviewed individually: approve it, or re-roll it with a new seed.
Approved chapters are then assembled and exported as MP3 (ACX spec), FLAC and
M4B with chapter markers.

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

1. **Settings…**: enter your Anthropic and ElevenLabs API keys. They are stored
   in `~/.config/narration-studio/config.json` (mode 0600). The environment
   variables `ANTHROPIC_API_KEY` and `ELEVENLABS_API_KEY` are used when the
   fields are empty.
2. **＋** creates a project. Choose its language, which sets the quote styles
   and the default TTS model.
3. **Project…**: pick the TTS model and output format (`pcm_44100` needs the
   Pro plan or higher; `mp3_44100_192` is the fallback). Set the concurrency to
   your plan's limit, and adjust silence durations and segmentation chunking.
4. **Voices…**: give every speaker an ElevenLabs voice id, including the
   narrator. *Load my ElevenLabs voices* fills a picker from your account.

## Workflow

1. **Import** a chapter from a text file, or **Paste** one. Paragraphs are
   separated by blank lines; a line like `***` or `#` marks a scene break.
2. **Segment with Claude** splits paragraphs into narration and dialogue and
   attributes speakers. Every result is checked against the source text, and a
   paragraph that keeps failing falls back to a quote parser with speaker
   `unknown`. Paragraphs without dialogue never go to the API.
3. Fix attribution while it is free: change speakers, merge or split segments
   (*Split…* takes `|` markers). Segments whose speaker has no voice show
   ⚠ and are skipped.
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
| `src/llm.rs` | Claude attribution request (structured JSON output) and response parsing |
| `src/tts.rs` | ElevenLabs text-to-speech and voice list |
| `src/worker.rs` | Command/event channels, segmentation pass, TTS worker pool, export jobs |
| `src/db.rs` | SQLite schema and queries |
| `src/audio.rs` | PCM/WAV handling, MP3-fallback decode |
| `src/export.rs` | Streaming assembly with silences; ffmpeg loudnorm and encode |
| `src/hash.rs` | Take hash |
| `src/player.rs` | rodio playback queue |
| `src/app.rs` | egui UI |
