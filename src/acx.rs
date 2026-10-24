//! ACX/Findaway technical checks of an exported file.
//!
//! - A native meter (fed with f32 samples, from ffmpeg or a WAV): whole-file
//!   unweighted RMS with the DC offset removed, sample peak, the noise floor
//!   as the quietest 0.5 s window (50 ms blocks, 0.1 s hop), and the room
//!   tone at the head and tail (50 ms blocks below -50 dBFS). This is how the
//!   ACX Check plugin measures; ffmpeg's astats "Noise floor" is peak-based
//!   and must not be used.
//! - Container facts from ffprobe and the LAME `Info`/`Xing` tag (CBR/VBR).
//! - Pass / warning / fail bands of the ACX Check plugin.
//! - `ACX report.md`, one table for the whole book.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// What the checks are called in the UI: they cover the published technical
/// audio spec, not whether a retailer accepts the title.
pub const SPEC_NAME: &str = "ACX/Findaway technical spec";

/// Information only; never blocks an export.
pub const POLICY_NOTE: &str = "ACX does not accept unauthorized text-to-speech or AI narration (Amazon's routes are KDP Virtual Voice and its invite-only Voice Replica beta). Findaway Voices by Spotify accepts audiobooks narrated with ElevenLabs and labels them as digitally narrated. These checks cover the technical audio spec only.";

const BLOCK_MS: u64 = 50;
/// 0.5 s noise-floor window and its 0.1 s hop, in 50 ms blocks.
const FLOOR_WINDOW_BLOCKS: usize = 10;
const FLOOR_HOP_BLOCKS: usize = 2;
/// Room tone at the head and tail: blocks quieter than this.
const SILENCE_DBFS: f64 = -50.0;
/// Rate the meter decodes files at (the delivery rate).
const METER_RATE: u32 = 44100;

pub const MAX_CHAPTER_MS: u64 = 120 * 60 * 1000;
pub const MAX_SAMPLE_MS: u64 = 5 * 60 * 1000;
pub const MAX_FILE_BYTES: u64 = 170_000_000;

/// Measured levels of one file, in dBFS (a full-scale sine reads -3.01 dB
/// RMS). Silence measures as `-inf`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Levels {
    pub duration_ms: u64,
    pub rms_db: f64,
    pub peak_db: f64,
    pub noise_floor_db: f64,
    /// Leading room tone (blocks below -50 dBFS).
    pub head_ms: u64,
    pub tail_ms: u64,
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            duration_ms: 0,
            rms_db: f64::NEG_INFINITY,
            peak_db: f64::NEG_INFINITY,
            noise_floor_db: f64::NEG_INFINITY,
            head_ms: 0,
            tail_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Block {
    n: u32,
    sum: f64,
    sq: f64,
}

impl Block {
    /// Mean square with the file's DC offset `m` removed.
    fn mean_square(&self, m: f64) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let n = self.n as f64;
        ((self.sq - 2.0 * m * self.sum + n * m * m) / n).max(0.0)
    }

    fn add(&mut self, o: &Block) {
        self.n += o.n;
        self.sum += o.sum;
        self.sq += o.sq;
    }
}

fn db_power(ms: f64) -> f64 {
    if ms > 0.0 {
        10.0 * ms.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// Streaming meter. Keeps one small record per 50 ms block, so a two-hour
/// chapter needs about 150k of them.
pub struct Meter {
    rate: u32,
    block_len: u32,
    blocks: Vec<Block>,
    cur: Block,
    peak: f32,
}

impl Meter {
    pub fn new(rate: u32) -> Self {
        let rate = rate.max(1);
        Self {
            rate,
            block_len: ((rate as u64 * BLOCK_MS / 1000) as u32).max(1),
            blocks: Vec::new(),
            cur: Block::default(),
            peak: 0.0,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        for &x in samples {
            let a = x.abs();
            if a > self.peak {
                self.peak = a;
            }
            let x = x as f64;
            self.cur.n += 1;
            self.cur.sum += x;
            self.cur.sq += x * x;
            if self.cur.n == self.block_len {
                self.blocks.push(self.cur);
                self.cur = Block::default();
            }
        }
    }

    pub fn finish(mut self) -> Levels {
        if self.cur.n > 0 {
            self.blocks.push(self.cur);
        }
        let mut total = Block::default();
        for b in &self.blocks {
            total.add(b);
        }
        if total.n == 0 {
            return Levels::default();
        }
        let n = total.n as u64;
        let mean = total.sum / total.n as f64;
        let rms_db = db_power(total.mean_square(mean));
        let peak_db = if self.peak > 0.0 {
            20.0 * (self.peak as f64).log10()
        } else {
            f64::NEG_INFINITY
        };

        // Floor: complete blocks only (a short final block is dropped).
        let complete = if self.blocks.last().is_some_and(|b| b.n < self.block_len) {
            self.blocks.len() - 1
        } else {
            self.blocks.len()
        };
        let noise_floor_db = if complete >= FLOOR_WINDOW_BLOCKS {
            let mut min = f64::INFINITY;
            let mut start = 0;
            while start + FLOOR_WINDOW_BLOCKS <= complete {
                let mut w = Block::default();
                for b in &self.blocks[start..start + FLOOR_WINDOW_BLOCKS] {
                    w.add(b);
                }
                min = min.min(w.mean_square(mean));
                start += FLOOR_HOP_BLOCKS;
            }
            db_power(min)
        } else {
            rms_db
        };

        // Room tone, counted in samples (the last block by its real length).
        let threshold = 10f64.powf(SILENCE_DBFS / 10.0);
        let quiet = |b: &Block| b.mean_square(mean) < threshold;
        let head: u64 = self
            .blocks
            .iter()
            .take_while(|b| quiet(b))
            .map(|b| b.n as u64)
            .sum();
        let tail: u64 = self
            .blocks
            .iter()
            .rev()
            .take_while(|b| quiet(b))
            .map(|b| b.n as u64)
            .sum();
        let ms = |samples: u64| samples * 1000 / self.rate as u64;
        Levels {
            duration_ms: ms(n),
            rms_db,
            peak_db,
            noise_floor_db,
            head_ms: ms(head),
            tail_ms: ms(tail),
        }
    }
}

#[cfg(test)]
pub fn measure_samples(rate: u32, samples: &[f32]) -> Levels {
    let mut m = Meter::new(rate);
    m.push(samples);
    m.finish()
}

/// Measure a 16-bit WAV (the assembly file, before mastering).
pub fn measure_wav(path: &Path) -> Result<Levels> {
    let mut r =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!("{}: expected 16-bit integer WAV", path.display());
    }
    let channels = spec.channels.max(1) as usize;
    let mut meter = Meter::new(spec.sample_rate);
    let mut buf: Vec<f32> = Vec::with_capacity(8192);
    let mut frame = 0f32;
    let mut k = 0usize;
    for s in r.samples::<i16>() {
        frame += s? as f32 / 32768.0;
        k += 1;
        if k == channels {
            buf.push(frame / channels as f32);
            frame = 0.0;
            k = 0;
            if buf.len() == buf.capacity() {
                meter.push(&buf);
                buf.clear();
            }
        }
    }
    meter.push(&buf);
    Ok(meter.finish())
}

/// Decode any audio file with ffmpeg (mono, 44.1 kHz, f32) and measure it.
pub fn measure_file(path: &Path) -> Result<Levels> {
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-ac", "1", "-ar"])
        .arg(METER_RATE.to_string())
        .args(["-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Never piped: ffmpeg would block on a full stderr pipe nobody reads.
        .stderr(Stdio::null())
        .spawn()
        .context("running ffmpeg (is it installed and on PATH?)")?;
    let mut out = child.stdout.take().context("ffmpeg stdout")?;
    let mut meter = Meter::new(METER_RATE);
    let mut buf = vec![0u8; 1 << 16];
    let mut pending: Vec<u8> = Vec::new();
    let mut samples: Vec<f32> = Vec::with_capacity(1 << 14);
    loop {
        let n = match out.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("reading decoded audio");
            }
        };
        pending.extend_from_slice(&buf[..n]);
        let whole = pending.len() / 4 * 4;
        samples.clear();
        samples.extend(
            pending[..whole]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );
        meter.push(&samples);
        pending.drain(..whole);
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg could not decode {}", path.display());
    }
    Ok(meter.finish())
}

// ---- container ----------------------------------------------------------------

/// The audio stream as ffprobe reports it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Container {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// The audio stream's bit rate (the format bit rate includes the cover).
    pub bit_rate: Option<u64>,
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as u64),
        _ => None,
    }
}

/// `ffprobe -show_entries stream=codec_name,sample_rate,channels,bit_rate -of json`.
pub fn parse_probe(json: &[u8]) -> Result<Container> {
    let v: serde_json::Value = serde_json::from_slice(json).context("ffprobe output")?;
    let s = v
        .get("streams")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .context("no audio stream")?;
    Ok(Container {
        codec: s
            .get("codec_name")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string(),
        sample_rate: s.get("sample_rate").and_then(json_u64).unwrap_or(0) as u32,
        channels: s.get("channels").and_then(json_u64).unwrap_or(0) as u32,
        bit_rate: s.get("bit_rate").and_then(json_u64),
    })
}

pub fn probe(path: &Path) -> Result<Container> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries"])
        .arg("stream=codec_name,sample_rate,channels,bit_rate")
        .args(["-of", "json"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("running ffprobe (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_probe(&out.stdout)
}

/// Bytes taken by a leading ID3v2 tag (0 when there is none).
pub fn id3_len(header: &[u8]) -> u64 {
    if header.len() < 10 || &header[..3] != b"ID3" {
        return 0;
    }
    let size = header[6..10]
        .iter()
        .fold(0u64, |a, &b| (a << 7) | (b & 0x7f) as u64);
    let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

/// The first MPEG frame's Xing/Info header: `Some(true)` = `Info` (constant
/// bit rate; ffmpeg rewrites Xing to Info when no frame's rate differed),
/// `Some(false)` = `Xing`/`VBRI` (variable), `None` = no such header.
pub fn xing_kind(audio: &[u8]) -> Option<bool> {
    let start = audio.iter().position(|&b| b != 0)?;
    let a = &audio[start..];
    let sync = a
        .windows(2)
        .take(4096)
        .position(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)?;
    let frame = &a[sync..];
    // MPEG-1 mono: offset 21; stereo: 36; MPEG-2 mono: 13; VBRI: 36.
    let end = frame.len().min(4 + 48);
    let head = frame.get(4..end)?;
    if head.windows(4).any(|w| w == b"Info") {
        Some(true)
    } else if head.windows(4).any(|w| w == b"Xing" || w == b"VBRI") {
        Some(false)
    } else {
        None
    }
}

/// Whether an MP3 file is CBR, from its LAME/Xing header.
pub fn mp3_cbr(path: &Path) -> Result<Option<bool>> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut header = [0u8; 10];
    let n = f.read(&mut header)?;
    let skip = id3_len(&header[..n]);
    f.seek(SeekFrom::Start(skip))?;
    let mut buf = Vec::new();
    f.take(16 * 1024).read_to_end(&mut buf)?;
    Ok(xing_kind(&buf))
}

// ---- checks -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Warn => "warning",
            Verdict::Fail => "FAIL",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Verdict::Pass => "✓",
            Verdict::Warn => "⚠",
            Verdict::Fail => "✗",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// Stable id: rms, peak, floor, head, tail, codec, rate, bitrate, cbr,
    /// channels, length, size (book level: cover, sample, …).
    pub key: String,
    pub label: String,
    /// "-20.3 dB", "0:42:10", "192 kbps".
    pub value: String,
    pub verdict: Verdict,
    /// The rule, or what is wrong.
    pub note: String,
}

impl Check {
    pub fn new(key: &str, label: &str, value: String, verdict: Verdict, note: &str) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            value,
            verdict,
            note: note.into(),
        }
    }
}

pub fn worst(checks: &[Check]) -> Verdict {
    checks
        .iter()
        .map(|c| c.verdict)
        .max()
        .unwrap_or(Verdict::Pass)
}

pub fn fmt_db(db: f64) -> String {
    if db.is_finite() {
        format!("{db:.1} dB")
    } else if db < 0.0 {
        "-inf dB".into()
    } else {
        "n/a".into()
    }
}

/// 1_234_000 ms → "0:20:34".
pub fn fmt_length(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

pub fn fmt_secs(ms: u64) -> String {
    format!("{:.2} s", ms as f64 / 1000.0)
}

/// RMS bands of the ACX Check plugin: pass -23…-18 dB, warning 2 dB either
/// side, fail beyond.
pub fn rms_verdict(db: f64) -> (Verdict, &'static str) {
    if !db.is_finite() {
        (Verdict::Fail, "silent file")
    } else if db > -16.0 {
        (Verdict::Fail, "too loud: above -16 dB (ACX: -23 to -18 dB)")
    } else if db > -18.0 {
        (Verdict::Warn, "louder than ACX's -23 to -18 dB")
    } else if db >= -23.0 {
        (Verdict::Pass, "-23 to -18 dB")
    } else if db >= -25.0 {
        (Verdict::Warn, "quieter than ACX's -23 to -18 dB")
    } else {
        (
            Verdict::Fail,
            "too quiet: below -25 dB (ACX: -23 to -18 dB)",
        )
    }
}

/// Peak bands: fail above -3 dB, pass -6…-3, warning -9…-6, fail below -9.
pub fn peak_verdict(db: f64) -> (Verdict, &'static str) {
    if !db.is_finite() {
        (Verdict::Fail, "silent file")
    } else if db > -3.0 {
        (Verdict::Fail, "above ACX's -3 dB ceiling")
    } else if db >= -6.0 {
        (Verdict::Pass, "at most -3 dB")
    } else if db >= -9.0 {
        (Verdict::Warn, "low: peaks should be between -6 and -3 dB")
    } else {
        (Verdict::Fail, "too low: below -9 dB")
    }
}

/// Noise floor: fail above -60 dB; warning below -90 dB ("dead silence").
pub fn floor_verdict(db: f64) -> (Verdict, &'static str) {
    if db > -60.0 {
        (Verdict::Fail, "above ACX's -60 dB limit")
    } else if !db.is_finite() || db < -90.0 {
        (
            Verdict::Warn,
            "dead silence (below -90 dB) sounds unnatural; turn on room tone",
        )
    } else {
        (Verdict::Pass, "at most -60 dB")
    }
}

/// Room tone: no more than 5 s at either end; at least 0.5 s at the head
/// and 1 s at the tail.
pub fn head_verdict(ms: u64) -> (Verdict, &'static str) {
    if ms > 5000 {
        (Verdict::Fail, "more than 5 s of room tone at the start")
    } else if ms < 500 {
        (Verdict::Warn, "under 0.5 s of room tone at the start")
    } else {
        (Verdict::Pass, "0.5 to 5 s (about 1 s is typical)")
    }
}

pub fn tail_verdict(ms: u64) -> (Verdict, &'static str) {
    if ms > 5000 {
        (Verdict::Fail, "more than 5 s of room tone at the end")
    } else if ms < 1000 {
        (Verdict::Warn, "under 1 s of room tone at the end")
    } else {
        (Verdict::Pass, "1 to 5 s (2 to 3 s is typical)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Chapter,
    Sample,
}

/// What the meter, ffprobe and the LAME header found, as report lines.
/// `container` is `Err` when ffprobe could not run; the format then shows
/// as "not checked".
pub fn evaluate(
    kind: FileKind,
    levels: &Levels,
    container: std::result::Result<&Container, &str>,
    cbr: Option<bool>,
    size_bytes: u64,
) -> Vec<Check> {
    let mut out = Vec::new();
    let (v, n) = rms_verdict(levels.rms_db);
    out.push(Check::new("rms", "RMS", fmt_db(levels.rms_db), v, n));
    let (v, n) = peak_verdict(levels.peak_db);
    out.push(Check::new("peak", "Peak", fmt_db(levels.peak_db), v, n));
    let (v, n) = floor_verdict(levels.noise_floor_db);
    out.push(Check::new(
        "floor",
        "Noise floor",
        fmt_db(levels.noise_floor_db),
        v,
        n,
    ));
    if kind == FileKind::Chapter {
        let (v, n) = head_verdict(levels.head_ms);
        out.push(Check::new("head", "Head", fmt_secs(levels.head_ms), v, n));
        let (v, n) = tail_verdict(levels.tail_ms);
        out.push(Check::new("tail", "Tail", fmt_secs(levels.tail_ms), v, n));
    }
    match container {
        Ok(c) => {
            out.push(if c.codec == "mp3" {
                Check::new(
                    "codec",
                    "Codec",
                    "MP3".into(),
                    Verdict::Pass,
                    "ACX wants MP3",
                )
            } else {
                Check::new(
                    "codec",
                    "Codec",
                    c.codec.clone(),
                    Verdict::Fail,
                    "ACX wants MP3",
                )
            });
            let rate = format!("{:.1} kHz", c.sample_rate as f64 / 1000.0);
            out.push(if c.sample_rate == 44100 {
                Check::new(
                    "rate",
                    "Sample rate",
                    rate,
                    Verdict::Pass,
                    "ACX wants 44.1 kHz",
                )
            } else {
                Check::new(
                    "rate",
                    "Sample rate",
                    rate,
                    Verdict::Fail,
                    "ACX wants 44.1 kHz",
                )
            });
            out.push(match c.bit_rate {
                Some(b) if b >= 192_000 => Check::new(
                    "bitrate",
                    "Bit rate",
                    format!("{} kbps", b / 1000),
                    Verdict::Pass,
                    "at least 192 kbps",
                ),
                Some(b) => Check::new(
                    "bitrate",
                    "Bit rate",
                    format!("{} kbps", b / 1000),
                    Verdict::Fail,
                    "ACX wants at least 192 kbps",
                ),
                None => Check::new(
                    "bitrate",
                    "Bit rate",
                    "unknown".into(),
                    Verdict::Warn,
                    "ffprobe reported no bit rate",
                ),
            });
            out.push(match c.channels {
                1 => Check::new(
                    "channels",
                    "Channels",
                    "mono".into(),
                    Verdict::Pass,
                    "mono (every file must match)",
                ),
                2 => Check::new(
                    "channels",
                    "Channels",
                    "stereo".into(),
                    Verdict::Pass,
                    "stereo (every file must match)",
                ),
                n => Check::new(
                    "channels",
                    "Channels",
                    n.to_string(),
                    Verdict::Fail,
                    "ACX wants mono or stereo",
                ),
            });
        }
        Err(why) => out.push(Check::new(
            "codec",
            "Format",
            "not checked".into(),
            Verdict::Warn,
            why,
        )),
    }
    out.push(match cbr {
        Some(true) => Check::new(
            "cbr",
            "CBR",
            "constant".into(),
            Verdict::Pass,
            "constant bit rate",
        ),
        Some(false) => Check::new(
            "cbr",
            "CBR",
            "variable".into(),
            Verdict::Fail,
            "ACX wants a constant bit rate",
        ),
        None => Check::new(
            "cbr",
            "CBR",
            "unknown".into(),
            Verdict::Warn,
            "no LAME/Xing header; constant bit rate not verified",
        ),
    });
    let length = fmt_length(levels.duration_ms);
    out.push(match kind {
        FileKind::Chapter if levels.duration_ms > MAX_CHAPTER_MS => Check::new(
            "length",
            "Length",
            length,
            Verdict::Fail,
            "over 120 minutes: split the chapter",
        ),
        FileKind::Chapter if levels.duration_ms > MAX_CHAPTER_MS - 60_000 => Check::new(
            "length",
            "Length",
            length,
            Verdict::Warn,
            "close to the 120-minute limit",
        ),
        FileKind::Chapter => Check::new(
            "length",
            "Length",
            length,
            Verdict::Pass,
            "up to 120 minutes",
        ),
        FileKind::Sample if levels.duration_ms > MAX_SAMPLE_MS => Check::new(
            "length",
            "Length",
            length,
            Verdict::Fail,
            "a retail sample may be at most 5 minutes",
        ),
        FileKind::Sample if levels.duration_ms < 60_000 => Check::new(
            "length",
            "Length",
            length,
            Verdict::Warn,
            "under a minute (older ACX wording asks for 1 to 5 minutes)",
        ),
        FileKind::Sample => {
            Check::new("length", "Length", length, Verdict::Pass, "up to 5 minutes")
        }
    });
    let mb = format!("{:.1} MB", size_bytes as f64 / 1e6);
    out.push(if size_bytes > MAX_FILE_BYTES {
        Check::new(
            "size",
            "Size",
            mb,
            Verdict::Fail,
            "over 170 MB: split the file",
        )
    } else {
        Check::new("size", "Size", mb, Verdict::Pass, "up to 170 MB")
    });
    out
}

/// The stored result of checking one exported file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileReport {
    /// File name inside the export folder.
    pub file: String,
    /// From a draft export (unapproved takes, gaps as silence).
    pub draft: bool,
    pub levels: Levels,
    pub checks: Vec<Check>,
    /// UTC, `2026-10-24T12:00:00Z`.
    pub checked_at: String,
    /// `retail::takes_key` of the chapter when it was exported (chapters
    /// only; reports stored before the key existed have none).
    pub takes_key: Option<String>,
}

impl FileReport {
    pub fn verdict(&self) -> Verdict {
        worst(&self.checks)
    }

    pub fn check(&self, key: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.key == key)
    }

    /// The first check with the worst verdict ("RMS -24.1 dB").
    pub fn headline(&self) -> Option<&Check> {
        let v = self.verdict();
        self.checks
            .iter()
            .find(|c| c.verdict == v && v != Verdict::Pass)
    }

    /// "ACX ✓", "ACX ⚠ Noise floor -inf", "ACX ✗ RMS -24.1".
    pub fn chip_text(&self) -> String {
        match self.headline() {
            None => "ACX ✓".into(),
            Some(c) => format!(
                "ACX {} {} {}",
                c.verdict.symbol(),
                c.label,
                c.value.trim_end_matches(" dB")
            ),
        }
    }

    /// "MP3 192 kbps 44.1 kHz mono CBR" and the worst of the format checks.
    pub fn format_summary(&self) -> (String, Verdict) {
        const KEYS: [&str; 6] = ["codec", "rate", "bitrate", "cbr", "channels", "size"];
        let worst = self
            .checks
            .iter()
            .filter(|c| KEYS.contains(&c.key.as_str()))
            .map(|c| c.verdict)
            .max()
            .unwrap_or(Verdict::Pass);
        let text = match (
            self.check("bitrate"),
            self.check("rate"),
            self.check("channels"),
        ) {
            (Some(b), Some(rt), Some(ch)) => format!(
                "{} {} {} {}{}",
                self.check("codec").map_or("MP3", |c| c.value.as_str()),
                b.value,
                rt.value,
                ch.value,
                if self
                    .check("cbr")
                    .is_some_and(|c| c.verdict == Verdict::Pass)
                {
                    " CBR"
                } else {
                    ""
                }
            ),
            _ => "not checked".into(),
        };
        (text, worst)
    }

    /// One line for the status bar.
    pub fn summary(&self, what: &str) -> String {
        let levels = format!(
            "RMS {}, peak {}, floor {}",
            fmt_db(self.levels.rms_db),
            fmt_db(self.levels.peak_db),
            fmt_db(self.levels.noise_floor_db)
        );
        let problems: Vec<String> = self
            .checks
            .iter()
            .filter(|c| c.verdict != Verdict::Pass)
            .map(|c| format!("{} {} ({})", c.label, c.value, c.note))
            .collect();
        if problems.is_empty() {
            format!("{what}: ACX ✓ {levels}.")
        } else {
            format!(
                "{what}: ACX {} {levels}; {}.",
                self.verdict().symbol(),
                problems.join("; ")
            )
        }
    }
}

// ---- report -------------------------------------------------------------------

/// A chapter row of the report.
pub struct ReportRow<'a> {
    pub number: usize,
    pub title: &'a str,
    pub report: Option<&'a FileReport>,
    /// Takes or approvals changed after the measurement.
    pub stale: bool,
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn cell(r: &FileReport, key: &str) -> String {
    match r.check(key) {
        Some(c) if c.verdict == Verdict::Pass => c.value.clone(),
        Some(c) => format!("{} {}", c.value, c.verdict.symbol()),
        None => "–".into(),
    }
}

/// `ACX report.md`: one table for every chapter, then the book-level checks,
/// the retail sample, every problem in words, and the policy note.
pub fn report_markdown(
    book_title: &str,
    generated: &str,
    rows: &[ReportRow],
    book_checks: &[Check],
    sample: Option<&FileReport>,
) -> String {
    let mut s = format!(
        "# ACX report: {}\n\n{SPEC_NAME}, measured on the exported MP3 files. Generated {generated}.\n\n",
        md_escape(book_title)
    );
    s.push_str("| # | Chapter | File | Length | RMS | Peak | Noise floor | Head | Tail | Format | Result |\n");
    s.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    let mut problems = Vec::new();
    for row in rows {
        let Some(r) = row.report else {
            s.push_str(&format!(
                "| {} | {} | not exported | | | | | | | | |\n",
                row.number,
                md_escape(row.title)
            ));
            continue;
        };
        let (format, format_worst) = r.format_summary();
        let format = if format_worst == Verdict::Pass {
            format
        } else {
            format!("{format} {}", format_worst.symbol())
        };
        let mut result = r.verdict().word().to_string();
        if r.draft {
            result.push_str(" (draft)");
        }
        if row.stale {
            result.push_str(" (takes or approvals changed since)");
        }
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.number,
            md_escape(row.title),
            md_escape(&r.file),
            cell(r, "length"),
            cell(r, "rms"),
            cell(r, "peak"),
            cell(r, "floor"),
            cell(r, "head"),
            cell(r, "tail"),
            format,
            result
        ));
        for c in r.checks.iter().filter(|c| c.verdict != Verdict::Pass) {
            problems.push(format!(
                "- {} {}: {} {} ({}: {})",
                row.number,
                row.title,
                c.label,
                c.value,
                c.verdict.word(),
                c.note
            ));
        }
    }
    s.push_str("\n## Book\n\n");
    for c in book_checks {
        s.push_str(&format!(
            "- {} **{}**: {} ({})\n",
            c.verdict.symbol(),
            c.label,
            c.value,
            c.note
        ));
        if c.verdict != Verdict::Pass {
            problems.push(format!(
                "- {}: {} ({}: {})",
                c.label,
                c.value,
                c.verdict.word(),
                c.note
            ));
        }
    }
    s.push_str("\n## Retail sample\n\n");
    match sample {
        None => s.push_str("No retail sample exported yet.\n"),
        Some(r) => {
            s.push_str(&format!(
                "{}: {}, RMS {}, peak {}, noise floor {}: {}.\n",
                md_escape(&r.file),
                cell(r, "length"),
                cell(r, "rms"),
                cell(r, "peak"),
                cell(r, "floor"),
                r.verdict().word()
            ));
            for c in r.checks.iter().filter(|c| c.verdict != Verdict::Pass) {
                problems.push(format!(
                    "- Retail sample: {} {} ({}: {})",
                    c.label,
                    c.value,
                    c.verdict.word(),
                    c.note
                ));
            }
        }
    }
    s.push_str("\n## Problems\n\n");
    if problems.is_empty() {
        s.push_str("None.\n");
    } else {
        s.push_str(&problems.join("\n"));
        s.push('\n');
    }
    s.push_str(&format!(
        "\n## Rules\n\n\
         - RMS -23 to -18 dB over the whole file (warning 2 dB either side), peak at most -3 dB,\n  \
           noise floor at most -60 dB (the quietest 0.5 s), as in the ACX Check plugin.\n\
         - MP3, 192 kbps or more, constant bit rate, 44.1 kHz; every file mono, or every file stereo.\n\
         - Room tone: at most 5 s at the start and at the end (0.5 s and 1 s at least).\n\
         - At most 120 minutes and 170 MB per file; a retail sample of at most 5 minutes.\n\n\
         > {POLICY_NOTE}\n"
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, secs: f64, amp: f32, freq: f32) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// Deterministic white-ish noise with RMS `rms` (linear).
    fn noise(rate: u32, secs: f64, rms: f32) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        let mut x: u32 = 12345;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Uniform in [-1, 1) has RMS 1/sqrt(3).
                let u = (x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                u * rms * 3f32.sqrt()
            })
            .collect()
    }

    #[test]
    fn sine_levels_match_astats_convention() {
        // 1/8 amplitude: peak -18.06 dB, RMS -21.07 dB (as astats reports).
        let l = measure_samples(44100, &sine(44100, 2.0, 0.125, 441.0));
        assert!((l.peak_db - -18.06).abs() < 0.02, "{l:?}");
        assert!((l.rms_db - -21.07).abs() < 0.02, "{l:?}");
        // A steady tone: the floor is the tone itself, no room tone.
        assert!((l.noise_floor_db - -21.07).abs() < 0.05);
        assert_eq!((l.head_ms, l.tail_ms), (0, 0));
        assert_eq!(l.duration_ms, 2000);
        // Full scale reads -3.01 dB RMS.
        let l = measure_samples(48000, &sine(48000, 1.0, 1.0, 480.0));
        assert!((l.rms_db - -3.01).abs() < 0.02);
    }

    #[test]
    fn floor_head_and_tail_from_noise_and_speech() {
        let rate = 44100;
        let mut s = noise(rate, 1.2, 0.0002); // -74 dB room tone
        s.extend(sine(rate, 3.0, 0.3, 300.0));
        s.extend(noise(rate, 0.6, 0.002)); // -54 dB, still below -50
        s.extend(noise(rate, 2.0, 0.0002));
        let l = measure_samples(rate, &s);
        // The quietest 0.5 s window is the -74 dB bed.
        assert!((l.noise_floor_db - -73.98).abs() < 0.3, "{l:?}");
        // Head: 1.2 s, give or take the block the tone starts in.
        assert!((1150..=1250).contains(&l.head_ms), "{l:?}");
        // Tail: 2.6 s of quiet blocks.
        assert!((2550..=2650).contains(&l.tail_ms), "{l:?}");
        assert_eq!(l.duration_ms, 6800);
    }

    #[test]
    fn dc_offset_does_not_count_as_level() {
        let rate = 22050;
        let mut s: Vec<f32> = sine(rate, 1.0, 0.1, 200.0)
            .iter()
            .map(|x| x + 0.2)
            .collect();
        s.extend(std::iter::repeat_n(0.2f32, rate as usize));
        let l = measure_samples(rate, &s);
        // RMS of a 0.1 sine over half the file: -23.0 - 3.01 dB, DC removed.
        assert!(
            (l.rms_db - (20.0 * (0.1f64 / 2f64.sqrt()).log10() - 3.01)).abs() < 0.2,
            "{l:?}"
        );
        // The constant part counts as silence once DC is removed.
        assert!(l.noise_floor_db < -90.0 || !l.noise_floor_db.is_finite());
    }

    #[test]
    fn digital_silence_is_minus_infinity() {
        let l = measure_samples(44100, &vec![0.0; 44100]);
        assert_eq!(l.rms_db, f64::NEG_INFINITY);
        assert_eq!(l.peak_db, f64::NEG_INFINITY);
        assert_eq!(l.noise_floor_db, f64::NEG_INFINITY);
        assert_eq!((l.head_ms, l.tail_ms), (1000, 1000));
        assert_eq!(measure_samples(44100, &[]), Levels::default());
        // Shorter than a floor window: the floor is the whole file.
        let l = measure_samples(44100, &sine(44100, 0.2, 0.5, 500.0));
        assert!((l.noise_floor_db - l.rms_db).abs() < 1e-9);
    }

    #[test]
    fn tail_counts_a_short_last_block_by_its_length() {
        let rate = 1000; // 50-sample blocks
        let mut s = sine(rate, 0.5, 0.5, 50.0);
        s.extend(vec![0.0f32; 120]); // 2 full blocks + 20 samples
        let l = measure_samples(rate, &s);
        assert_eq!(l.tail_ms, 120);
        assert_eq!(l.duration_ms, 620);
    }

    #[test]
    fn verdict_bands() {
        assert_eq!(rms_verdict(-20.0).0, Verdict::Pass);
        assert_eq!(rms_verdict(-18.0).0, Verdict::Pass);
        assert_eq!(rms_verdict(-23.0).0, Verdict::Pass);
        assert_eq!(rms_verdict(-17.0).0, Verdict::Warn);
        assert_eq!(rms_verdict(-24.1).0, Verdict::Warn);
        assert_eq!(rms_verdict(-15.9).0, Verdict::Fail);
        assert_eq!(rms_verdict(-25.5).0, Verdict::Fail);
        assert_eq!(rms_verdict(f64::NEG_INFINITY).0, Verdict::Fail);
        assert_eq!(peak_verdict(-2.9).0, Verdict::Fail);
        assert_eq!(peak_verdict(-3.0).0, Verdict::Pass);
        assert_eq!(peak_verdict(-6.0).0, Verdict::Pass);
        assert_eq!(peak_verdict(-7.0).0, Verdict::Warn);
        assert_eq!(peak_verdict(-9.5).0, Verdict::Fail);
        assert_eq!(floor_verdict(-59.0).0, Verdict::Fail);
        assert_eq!(floor_verdict(-60.0).0, Verdict::Pass);
        assert_eq!(floor_verdict(-73.4).0, Verdict::Pass);
        assert_eq!(floor_verdict(-95.0).0, Verdict::Warn);
        assert_eq!(floor_verdict(f64::NEG_INFINITY).0, Verdict::Warn);
        assert_eq!(head_verdict(1050).0, Verdict::Pass);
        assert_eq!(head_verdict(300).0, Verdict::Warn);
        assert_eq!(head_verdict(5200).0, Verdict::Fail);
        assert_eq!(tail_verdict(2100).0, Verdict::Pass);
        assert_eq!(tail_verdict(800).0, Verdict::Warn);
        assert_eq!(tail_verdict(6000).0, Verdict::Fail);
    }

    fn good_levels() -> Levels {
        Levels {
            duration_ms: 600_000,
            rms_db: -20.3,
            peak_db: -4.2,
            noise_floor_db: -73.4,
            head_ms: 1050,
            tail_ms: 2100,
        }
    }

    fn mp3_container() -> Container {
        Container {
            codec: "mp3".into(),
            sample_rate: 44100,
            channels: 1,
            bit_rate: Some(192_000),
        }
    }

    #[test]
    fn evaluate_chapter_and_sample() {
        let c = mp3_container();
        let checks = evaluate(
            FileKind::Chapter,
            &good_levels(),
            Ok(&c),
            Some(true),
            14_000_000,
        );
        assert_eq!(worst(&checks), Verdict::Pass, "{checks:#?}");
        let keys: Vec<&str> = checks.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "rms", "peak", "floor", "head", "tail", "codec", "rate", "bitrate", "channels",
                "cbr", "length", "size"
            ]
        );

        let mut quiet = good_levels();
        quiet.rms_db = -24.1;
        let vbr = Container {
            bit_rate: Some(128_000),
            sample_rate: 48000,
            ..c.clone()
        };
        let checks = evaluate(
            FileKind::Chapter,
            &quiet,
            Ok(&vbr),
            Some(false),
            200_000_000,
        );
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| c.verdict == Verdict::Fail)
            .map(|c| c.key.as_str())
            .collect();
        assert_eq!(failed, ["rate", "bitrate", "cbr", "size"]);
        assert_eq!(checks[0].verdict, Verdict::Warn);

        // Samples skip head/tail and allow at most 5 minutes.
        let mut long = good_levels();
        long.duration_ms = 301_000;
        let checks = evaluate(FileKind::Sample, &long, Err("ffprobe not found"), None, 1);
        assert!(checks.iter().all(|c| c.key != "head" && c.key != "tail"));
        let length = checks.iter().find(|c| c.key == "length").unwrap();
        assert_eq!(length.verdict, Verdict::Fail);
        let format = checks.iter().find(|c| c.key == "codec").unwrap();
        assert_eq!(
            (format.value.as_str(), format.verdict),
            ("not checked", Verdict::Warn)
        );
    }

    #[test]
    fn chip_and_summary_name_the_worst_check() {
        let c = mp3_container();
        let mut r = FileReport {
            file: "01 - One.mp3".into(),
            draft: false,
            levels: good_levels(),
            checks: evaluate(FileKind::Chapter, &good_levels(), Ok(&c), Some(true), 1),
            checked_at: "2026-10-24T12:00:00Z".into(),
            takes_key: None,
        };
        assert_eq!(r.chip_text(), "ACX ✓");
        assert!(r.summary("One").starts_with("One: ACX ✓ RMS -20.3 dB"));
        r.levels.rms_db = -26.0;
        r.levels.noise_floor_db = f64::NEG_INFINITY;
        r.checks = evaluate(FileKind::Chapter, &r.levels, Ok(&c), Some(true), 1);
        assert_eq!(r.chip_text(), "ACX ✗ RMS -26.0");
        assert!(
            r.summary("One")
                .contains("Noise floor -inf dB (dead silence")
        );
        r.levels.rms_db = -21.0;
        r.checks = evaluate(FileKind::Chapter, &r.levels, Ok(&c), Some(true), 1);
        assert_eq!(r.chip_text(), "ACX ⚠ Noise floor -inf");
    }

    #[test]
    fn probe_json_and_lame_header() {
        let json = br#"{"programs":[],"streams":[{"codec_name":"mp3","sample_rate":"44100","channels":1,"bit_rate":"192000"}],"format":{"duration":"48.770612"}}"#;
        assert_eq!(parse_probe(json).unwrap(), mp3_container());
        assert!(parse_probe(br#"{"streams":[]}"#).is_err());

        let mut tag = b"ID3\x03\x00\x00\x00\x00\x02\x01".to_vec();
        assert_eq!(id3_len(&tag), 10 + 257);
        tag[5] = 0x10;
        assert_eq!(id3_len(&tag), 10 + 257 + 10);
        assert_eq!(id3_len(b"\xFF\xFB\x90\x00"), 0);

        // Zero padding, then a frame header and the tag at offset 21.
        let mut audio = vec![0u8; 5];
        audio.extend([0xFF, 0xFB, 0xB0, 0xC0]);
        audio.extend(vec![0u8; 17]);
        audio.extend(b"Info");
        audio.extend(vec![0u8; 100]);
        assert_eq!(xing_kind(&audio), Some(true));
        let at = audio.windows(4).position(|w| w == b"Info").unwrap();
        audio[at..at + 4].copy_from_slice(b"Xing");
        assert_eq!(xing_kind(&audio), Some(false));
        audio[at..at + 4].copy_from_slice(b"\0\0\0\0");
        assert_eq!(xing_kind(&audio), None);
    }

    #[test]
    fn report_lists_every_chapter_and_problem() {
        let c = mp3_container();
        let mut bad = good_levels();
        bad.rms_db = -24.1;
        let ok = FileReport {
            file: "01 - One.mp3".into(),
            draft: false,
            levels: good_levels(),
            checks: evaluate(FileKind::Chapter, &good_levels(), Ok(&c), Some(true), 1),
            checked_at: String::new(),
            takes_key: None,
        };
        let quiet = FileReport {
            file: "02 - Two | B.mp3".into(),
            draft: true,
            levels: bad,
            checks: evaluate(FileKind::Chapter, &bad, Ok(&c), Some(true), 1),
            checked_at: String::new(),
            takes_key: None,
        };
        let rows = [
            ReportRow {
                number: 1,
                title: "One",
                report: Some(&ok),
                stale: false,
            },
            ReportRow {
                number: 2,
                title: "Two",
                report: Some(&quiet),
                stale: true,
            },
            ReportRow {
                number: 3,
                title: "Three",
                report: None,
                stale: false,
            },
        ];
        let cover = Check::new(
            "cover",
            "Cover",
            "1600×1600 JPEG".into(),
            Verdict::Warn,
            "ACX wants at least 2400×2400",
        );
        let md = report_markdown("The Book", "24 Oct 2026, 12:00 UTC", &rows, &[cover], None);
        assert!(md.starts_with("# ACX report: The Book\n"));
        assert!(md.contains("| 1 | One | 01 - One.mp3 | 0:10:00 | -20.3 dB | -4.2 dB | -73.4 dB | 1.05 s | 2.10 s | MP3 192 kbps 44.1 kHz mono CBR | pass |"), "{md}");
        assert!(md.contains("02 - Two \\| B.mp3"));
        assert!(md.contains("-24.1 dB ⚠"));
        assert!(md.contains("warning (draft) (takes or approvals changed since)"));
        assert!(md.contains("| 3 | Three | not exported |"));
        assert!(md.contains("- 2 Two: RMS -24.1 dB (warning: quieter than ACX's -23 to -18 dB)"));
        assert!(md.contains("- Cover: 1600×1600 JPEG (warning: ACX wants at least 2400×2400)"));
        assert!(md.contains("No retail sample exported yet."));
        assert!(md.contains(POLICY_NOTE));
    }
}
