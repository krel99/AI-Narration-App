//! Retail-ready delivery: the book's publication metadata and how it maps
//! onto ID3 (chapter MP3s), Vorbis comments (FLAC) and MP4 atoms (M4B);
//! cover-art checks; the retail-sample planner; and the book-level lines of
//! the ACX report. Pure logic: the ffmpeg side lives in `export/mastering.rs`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::acx::{Check, FileReport, Verdict};
use crate::model::{Chapter, Id, SilenceSettings};

/// Book metadata and delivery options (`ProjectSettings.publication`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Publication {
    /// Empty: the project name.
    pub title: String,
    pub subtitle: String,
    pub author: String,
    pub narrator: String,
    pub publisher: String,
    pub year: String,
    pub description: String,
    /// Empty: "© <year> <publisher>" in the M4B, nothing in the MP3s.
    pub copyright: String,
    pub genre: String,
    /// JPG or PNG; relative paths are inside the library (`covers/…`).
    pub cover_path: String,
    /// Embed the cover in the chapter MP3s too (the FLAC and M4B always get
    /// it). Whether retail uploaders accept embedded art is not documented.
    pub cover_in_mp3: bool,
    /// Lay a very quiet pink-noise bed (about -73 dB) under the mastered
    /// audio, so pauses are room tone instead of digital silence.
    pub room_tone: bool,
}

impl Default for Publication {
    fn default() -> Self {
        Self {
            title: String::new(),
            subtitle: String::new(),
            author: String::new(),
            narrator: String::new(),
            publisher: String::new(),
            year: String::new(),
            description: String::new(),
            copyright: String::new(),
            genre: "Audiobook".into(),
            cover_path: String::new(),
            cover_in_mp3: true,
            room_tone: true,
        }
    }
}

type Tags = Vec<(&'static str, String)>;

fn push(tags: &mut Tags, key: &'static str, value: &str) {
    let v = value.trim();
    if !v.is_empty() {
        tags.push((key, v.to_string()));
    }
}

impl Publication {
    pub fn book_title<'a>(&'a self, project_name: &'a str) -> &'a str {
        let t = self.title.trim();
        if t.is_empty() { project_name.trim() } else { t }
    }

    pub fn genre(&self) -> &str {
        let g = self.genre.trim();
        if g.is_empty() { "Audiobook" } else { g }
    }

    /// The M4B has no publisher atom (ffmpeg drops the key), so the
    /// publisher goes into the copyright line when none is given.
    pub fn m4b_copyright(&self) -> String {
        if !self.copyright.trim().is_empty() {
            return self.copyright.trim().to_string();
        }
        match (self.year.trim(), self.publisher.trim()) {
            (_, "") => String::new(),
            ("", p) => format!("© {p}"),
            (y, p) => format!("© {y} {p}"),
        }
    }

    /// Tags shared by every MP3 and FLAC file of the book.
    fn common(&self, project_name: &str, title: &str) -> Tags {
        let mut t = Tags::new();
        push(&mut t, "title", title);
        push(&mut t, "album", self.book_title(project_name));
        push(&mut t, "artist", &self.author);
        push(&mut t, "album_artist", &self.author);
        push(&mut t, "composer", &self.narrator);
        push(&mut t, "genre", self.genre());
        push(&mut t, "date", &self.year);
        t
    }

    /// ID3v2.3 frames of a chapter MP3 (TIT2, TALB, TPE1, TPE2, TCOM, TCON,
    /// TYER, TRCK, TPUB, TCOP, TIT3). Description and synopsis are left out:
    /// ffmpeg cannot write COMM and would turn them into TXXX frames.
    pub fn mp3_tags(&self, project_name: &str, title: &str, track: Option<(usize, usize)>) -> Tags {
        let mut t = self.common(project_name, title);
        if let Some((n, total)) = track {
            t.push(("track", format!("{n}/{total}")));
        }
        push(&mut t, "publisher", &self.publisher);
        push(&mut t, "copyright", &self.copyright);
        push(&mut t, "TIT3", &self.subtitle);
        t
    }

    /// Vorbis comments of a chapter's FLAC master.
    pub fn flac_tags(
        &self,
        project_name: &str,
        title: &str,
        track: Option<(usize, usize)>,
    ) -> Tags {
        let mut t = self.common(project_name, title);
        if let Some((n, total)) = track {
            t.push(("track", format!("{n}/{total}")));
        }
        push(&mut t, "publisher", &self.publisher);
        push(&mut t, "copyright", &self.copyright);
        push(&mut t, "subtitle", &self.subtitle);
        t
    }

    /// Global FFMETADATA keys of the M4B. Book title in title and album
    /// (album carries the subtitle, as Audiobookshelf writes it), author in
    /// artist and album_artist, narrator in composer, `media_type=2` (stik
    /// Audiobook).
    pub fn m4b_tags(&self, project_name: &str) -> Tags {
        let title = self.book_title(project_name);
        let mut t = Tags::new();
        push(&mut t, "title", title);
        let album = if self.subtitle.trim().is_empty() {
            title.to_string()
        } else {
            format!("{title}: {}", self.subtitle.trim())
        };
        push(&mut t, "album", &album);
        push(&mut t, "artist", &self.author);
        push(&mut t, "album_artist", &self.author);
        push(&mut t, "composer", &self.narrator);
        push(&mut t, "genre", self.genre());
        push(&mut t, "date", &self.year);
        push(&mut t, "comment", &self.description);
        push(&mut t, "description", &self.description);
        push(&mut t, "synopsis", &self.description);
        push(&mut t, "copyright", &self.m4b_copyright());
        t.push(("media_type", "2".into()));
        t
    }

    pub fn sample_tags(&self, project_name: &str) -> Tags {
        let mut t = self.common(project_name, "Retail Sample");
        push(&mut t, "publisher", &self.publisher);
        push(&mut t, "copyright", &self.copyright);
        t
    }

    /// Whether any publication detail has been filled in.
    pub fn started(&self) -> bool {
        [
            &self.title,
            &self.subtitle,
            &self.author,
            &self.narrator,
            &self.publisher,
            &self.year,
            &self.description,
            &self.copyright,
            &self.cover_path,
        ]
        .iter()
        .any(|f| !f.trim().is_empty())
    }

    /// Small problems worth an amber hint in the dialog (never blocking).
    /// Missing names are only pointed out once the user has started filling
    /// in the details, so projects not meant for retail stay quiet.
    pub fn hints(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.started() {
            return out;
        }
        if self.author.trim().is_empty() {
            out.push("No author: the files will have no artist tag.");
        }
        if self.narrator.trim().is_empty() {
            out.push("No narrator: the files will have no composer (narrator) tag.");
        }
        let y = self.year.trim();
        if !y.is_empty() && !(y.len() == 4 && y.chars().all(|c| c.is_ascii_digit())) {
            out.push("The year should be four digits (ID3v2.3 stores it as TYER).");
        }
        out
    }
}

// ---- cover ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverFormat {
    Jpeg,
    Png,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorModel {
    Rgb,
    Rgba,
    Gray,
    Cmyk,
    Palette,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CoverInfo {
    pub format: CoverFormat,
    pub width: usize,
    pub height: usize,
    pub bytes: u64,
    pub color: ColorModel,
    /// Bits per channel.
    pub depth: u8,
}

pub const COVER_MIN_PX: usize = 2400;
pub const COVER_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// JPEG: precision and component count of the first SOFn segment
/// (C0–CF without C4, C8, CC). APP segments can push it far from the start,
/// so the whole file is scanned.
pub fn jpeg_components(data: &[u8]) -> Option<(u8, u8)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        if marker == 0xFF {
            i += 1; // fill byte
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2; // no length
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let precision = *data.get(i + 4)?;
            let components = *data.get(i + 9)?;
            return Some((precision, components));
        }
        if marker == 0xDA {
            return None; // start of scan before any frame header
        }
        i += 2 + len;
    }
    None
}

/// PNG: bit depth and colour type from IHDR.
pub fn png_header(data: &[u8]) -> Option<(u8, u8)> {
    if data.len() < 26 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
        return None;
    }
    Some((data[24], data[25]))
}

pub fn inspect_cover_bytes(data: &[u8]) -> Result<CoverInfo> {
    let size =
        imagesize::blob_size(data).map_err(|e| anyhow::anyhow!("not a readable image ({e})"))?;
    let kind = imagesize::image_type(data).ok();
    let (format, color, depth) = match kind {
        Some(imagesize::ImageType::Jpeg) => {
            let (depth, comps) = jpeg_components(data).unwrap_or((0, 0));
            let color = match comps {
                1 => ColorModel::Gray,
                3 => ColorModel::Rgb,
                4 => ColorModel::Cmyk,
                _ => ColorModel::Unknown,
            };
            (CoverFormat::Jpeg, color, depth)
        }
        Some(imagesize::ImageType::Png) => {
            let (depth, ct) = png_header(data).unwrap_or((0, 255));
            let color = match ct {
                2 => ColorModel::Rgb,
                6 => ColorModel::Rgba,
                0 | 4 => ColorModel::Gray,
                3 => ColorModel::Palette,
                _ => ColorModel::Unknown,
            };
            (CoverFormat::Png, color, depth)
        }
        _ => (CoverFormat::Other, ColorModel::Unknown, 0),
    };
    Ok(CoverInfo {
        format,
        width: size.width,
        height: size.height,
        bytes: data.len() as u64,
        color,
        depth,
    })
}

pub fn inspect_cover(path: &Path) -> Result<CoverInfo> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    if meta.len() > 64 * 1024 * 1024 {
        anyhow::bail!(
            "{:.0} MB is far too large for cover art",
            meta.len() as f64 / 1e6
        );
    }
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    inspect_cover_bytes(&data)
}

impl CoverInfo {
    pub fn describe(&self) -> String {
        let format = match self.format {
            CoverFormat::Jpeg => "JPEG",
            CoverFormat::Png => "PNG",
            CoverFormat::Other => "image",
        };
        let size = if self.bytes < 1_000_000 {
            format!("{} KB", self.bytes.div_ceil(1000))
        } else {
            format!("{:.1} MB", self.bytes as f64 / 1e6)
        };
        format!("{}×{} {format}, {size}", self.width, self.height)
    }

    /// What keeps the cover from ACX's art rules (square, at least
    /// 2400×2400, JPG/PNG, RGB, 8 bits per channel or more, at most 8 MB).
    /// Empty when it meets them. Warnings only; the export uses it anyway.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.format == CoverFormat::Other {
            out.push("not a JPG or PNG".into());
        }
        if self.width != self.height {
            out.push(format!("not square ({}×{})", self.width, self.height));
        }
        if self.width.min(self.height) < COVER_MIN_PX {
            out.push(format!("smaller than {COVER_MIN_PX}×{COVER_MIN_PX}"));
        }
        match self.color {
            ColorModel::Cmyk => out.push("CMYK; ACX wants RGB".into()),
            ColorModel::Gray => out.push("greyscale; ACX wants 24-bit RGB".into()),
            ColorModel::Palette => out.push("palette colours; ACX wants 24-bit RGB".into()),
            _ => {}
        }
        if self.depth != 0 && self.depth < 8 {
            out.push(format!(
                "{} bits per channel; ACX wants 24-bit colour",
                self.depth
            ));
        }
        if self.bytes > COVER_MAX_BYTES {
            out.push("larger than 8 MB".into());
        }
        out
    }
}

/// The cover file a project points at (`covers/…` is inside the library).
pub fn cover_file(library_root: &Path, cover_path: &str) -> Option<PathBuf> {
    let p = cover_path.trim();
    if p.is_empty() {
        return None;
    }
    let path = Path::new(p);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        library_root.join(path)
    })
}

/// Copy a chosen cover into `<library>/covers/` so the library stays
/// self-contained; returns the relative path to store. A file already in
/// the library is kept where it is.
pub fn import_cover(library_root: &Path, project_id: Id, source: &Path) -> Result<String> {
    if let Ok(rel) = source.strip_prefix(library_root) {
        return Ok(rel.to_string_lossy().to_string());
    }
    let data = std::fs::read(source).with_context(|| format!("reading {}", source.display()))?;
    let ext = match imagesize::image_type(&data) {
        Ok(imagesize::ImageType::Png) => "png",
        Ok(imagesize::ImageType::Jpeg) => "jpg",
        _ => source.extension().and_then(|e| e.to_str()).unwrap_or("img"),
    }
    .to_ascii_lowercase();
    let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&data));
    let name = format!("project-{project_id}-{}.{ext}", &digest[..10]);
    let dir = library_root.join("covers");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(&name);
    if !dest.exists() {
        let tmp = dir.join(format!("{name}.part"));
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(format!("covers/{name}"))
}

// ---- retail sample --------------------------------------------------------------

/// Room tone before the first word (the 0.5 s fade-in covers it).
pub const SAMPLE_LEAD_IN_MS: u64 = 500;
/// Room tone after the last word, inside the 3 s fade-out.
pub const SAMPLE_TAIL_MS: u64 = 1000;
pub const SAMPLE_FADE_IN_S: f64 = 0.5;
pub const SAMPLE_FADE_OUT_S: f64 = 3.0;
pub const SAMPLE_MIN_MS: u64 = 60_000;
pub const SAMPLE_MAX_MS: u64 = 300_000;
pub const SAMPLE_DEFAULT_MS: u64 = 180_000;

/// One segment of the chapter, in reading order.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleSource {
    pub segment_id: Id,
    /// Paragraph index within the chapter.
    pub paragraph: usize,
    /// Its paragraph follows a scene break.
    pub scene_break_before: bool,
    /// Approved and has a current take.
    pub approved: bool,
    /// Length of the current take.
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleStop {
    /// Ends at the last paragraph end within the length.
    ParagraphEnd,
    /// The first paragraph is longer than the length: ends after a segment.
    SegmentEnd,
    /// The first segment alone is longer: cut inside it.
    Cut,
    /// Reached the end of the chapter.
    ChapterEnd,
    /// Stopped before a segment that is not approved.
    Unapproved(Id),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SamplePlan {
    /// Segment range `[start, end)` into the sources.
    pub start: usize,
    pub end: usize,
    /// Speech and pauses, without the lead-in and tail.
    pub speech_ms: u64,
    pub stop: SampleStop,
}

impl SamplePlan {
    pub fn total_ms(&self) -> u64 {
        SAMPLE_LEAD_IN_MS + self.speech_ms + SAMPLE_TAIL_MS
    }

    pub fn describe(&self) -> String {
        let s = self.total_ms() / 1000;
        let len = format!("{}:{:02}", s / 60, s % 60);
        let n = self.end - self.start;
        let why = match self.stop {
            SampleStop::ParagraphEnd => "ends at a paragraph end",
            SampleStop::SegmentEnd => "ends inside a long paragraph, after a segment",
            SampleStop::Cut => "cut inside a long segment",
            SampleStop::ChapterEnd => "the rest of the chapter",
            SampleStop::Unapproved(_) => "stops before a segment that is not approved",
        };
        format!("{len}, {n} segments, {why}")
    }
}

/// The pause before segment `i` (the same as the chapter assembly).
pub fn gap_before(src: &[SampleSource], i: usize, s: &SilenceSettings) -> u64 {
    if i == 0 {
        return 0;
    }
    (if src[i].paragraph != src[i - 1].paragraph {
        if src[i].scene_break_before {
            s.scene_break_ms
        } else {
            s.between_paragraphs_ms
        }
    } else {
        s.within_paragraph_ms
    }) as u64
}

/// Plan a retail sample: approved takes only, from `start`, as long as fits
/// in `max_ms` (lead-in and tail included), ending on a paragraph end where
/// possible. Stops before the first unapproved segment.
pub fn plan_sample(
    src: &[SampleSource],
    start: usize,
    max_ms: u64,
    silence: &SilenceSettings,
) -> std::result::Result<SamplePlan, String> {
    let Some(first) = src.get(start) else {
        return Err("This chapter has no segments.".into());
    };
    if !first.approved {
        return Err(
            "The start segment is not approved; the sample uses approved takes only.".into(),
        );
    }
    let budget = max_ms
        .min(SAMPLE_MAX_MS)
        .saturating_sub(SAMPLE_LEAD_IN_MS + SAMPLE_TAIL_MS);
    let mut t = 0u64;
    let mut last_para: Option<(usize, u64)> = None;
    let mut last_seg: Option<(usize, u64)> = None;
    let mut i = start;
    let stop = loop {
        if i == src.len() {
            break SampleStop::ChapterEnd;
        }
        let s = &src[i];
        if !s.approved {
            break SampleStop::Unapproved(s.segment_id);
        }
        let gap = if i == start {
            0
        } else {
            gap_before(src, i, silence)
        };
        if t + gap + s.duration_ms > budget {
            break SampleStop::ParagraphEnd;
        }
        t += gap + s.duration_ms;
        last_seg = Some((i + 1, t));
        if i + 1 == src.len() || src[i + 1].paragraph != s.paragraph {
            last_para = Some((i + 1, t));
        }
        i += 1;
    };
    let plan = |end: usize, speech_ms: u64, stop: SampleStop| SamplePlan {
        start,
        end,
        speech_ms,
        stop,
    };
    Ok(match stop {
        SampleStop::ChapterEnd => plan(src.len(), t, stop),
        _ => match (last_para, last_seg) {
            (Some((end, t)), _) => plan(
                end,
                t,
                if matches!(stop, SampleStop::Unapproved(_)) {
                    stop
                } else {
                    SampleStop::ParagraphEnd
                },
            ),
            (None, Some((end, t))) => plan(
                end,
                t,
                if matches!(stop, SampleStop::Unapproved(_)) {
                    stop
                } else {
                    SampleStop::SegmentEnd
                },
            ),
            // The first segment alone does not fit.
            (None, None) => plan(start + 1, budget, SampleStop::Cut),
        },
    })
}

// ---- book-level checks ------------------------------------------------------------

fn mentions(text: &str, needle: &str) -> bool {
    let n = needle.trim().to_lowercase();
    !n.is_empty() && text.to_lowercase().contains(&n)
}

/// The book-level lines of the report: cover, channel consistency, retail
/// sample and credits.
pub fn book_checks(
    publication: &Publication,
    project_name: &str,
    cover: Option<std::result::Result<&CoverInfo, &str>>,
    chapters: &[Chapter],
    reports: &[&FileReport],
    sample: Option<(&FileReport, Option<&Chapter>)>,
) -> Vec<Check> {
    let mut out = Vec::new();
    out.push(match cover {
        None => Check::new(
            "cover",
            "Cover",
            "none".into(),
            Verdict::Warn,
            "add a square JPG/PNG of at least 2400×2400 in Project settings",
        ),
        Some(Err(e)) => Check::new("cover", "Cover", "unreadable".into(), Verdict::Warn, e),
        Some(Ok(info)) => {
            let problems = info.problems();
            if problems.is_empty() {
                Check::new(
                    "cover",
                    "Cover",
                    info.describe(),
                    Verdict::Pass,
                    "square, at least 2400×2400, RGB",
                )
            } else {
                Check::new(
                    "cover",
                    "Cover",
                    info.describe(),
                    Verdict::Warn,
                    &problems.join(", "),
                )
            }
        }
    });

    let channels: std::collections::BTreeSet<&str> = reports
        .iter()
        .filter_map(|r| r.check("channels").map(|c| c.value.as_str()))
        .collect();
    out.push(match channels.len() {
        0 => Check::new(
            "channels_all",
            "Channels",
            "no files".into(),
            Verdict::Pass,
            "every file mono, or every file stereo",
        ),
        1 => Check::new(
            "channels_all",
            "Channels",
            format!("all {}", channels.iter().next().unwrap()),
            Verdict::Pass,
            "every file the same",
        ),
        _ => Check::new(
            "channels_all",
            "Channels",
            channels.into_iter().collect::<Vec<_>>().join(" and "),
            Verdict::Fail,
            "ACX wants every file mono, or every file stereo",
        ),
    });

    out.push(match sample {
        None => Check::new(
            "sample",
            "Retail sample",
            "none".into(),
            Verdict::Warn,
            "export one from the Retail & ACX window",
        ),
        Some((r, ch)) => {
            let from_credits = ch.is_some_and(|c| c.title.to_lowercase().contains("credit"));
            let v = r.verdict();
            if from_credits {
                Check::new(
                    "sample",
                    "Retail sample",
                    r.file.clone(),
                    v.max(Verdict::Warn),
                    "taken from the credits; ACX wants it from the story itself",
                )
            } else {
                Check::new(
                    "sample",
                    "Retail sample",
                    r.file.clone(),
                    v,
                    if v == Verdict::Pass {
                        "at most 5 minutes, levels in range"
                    } else {
                        "see the retail sample section"
                    },
                )
            }
        }
    });

    let title = publication.book_title(project_name);
    let names = [
        title,
        publication.author.as_str(),
        publication.narrator.as_str(),
    ];
    let credits = |c: &Chapter, closing: bool| -> Check {
        let key = if closing { "closing" } else { "opening" };
        let label = if closing {
            "Closing credits"
        } else {
            "Opening credits"
        };
        let mut missing: Vec<&str> = Vec::new();
        for (what, name) in ["title", "author", "narrator"].iter().zip(names) {
            if !mentions(&c.source_text, name) {
                missing.push(what);
            }
        }
        if closing && !mentions(&c.source_text, "the end") {
            missing.push("\"The End\"");
        }
        if missing.is_empty() {
            Check::new(
                key,
                label,
                c.title.clone(),
                Verdict::Pass,
                "names the title, author and narrator",
            )
        } else {
            Check::new(
                key,
                label,
                c.title.clone(),
                Verdict::Warn,
                &format!("does not mention the {}", missing.join(", ")),
            )
        }
    };
    let is_credits = |c: &Chapter| c.title.to_lowercase().contains("credit");
    out.push(match chapters.first().filter(|c| is_credits(c)) {
        Some(c) => credits(c, false),
        None => Check::new(
            "opening",
            "Opening credits",
            "none".into(),
            Verdict::Warn,
            "ACX wants a first file named like \"Opening credits\" with the title, author and narrator",
        ),
    });
    out.push(match chapters.last().filter(|c| is_credits(c) && chapters.len() > 1) {
        Some(c) => credits(c, true),
        None => Check::new(
            "closing",
            "Closing credits",
            "none".into(),
            Verdict::Warn,
            "ACX wants a last file named like \"Closing credits\" (title, author, narrator, \"The End\")",
        ),
    });
    out
}

// ---- report state ---------------------------------------------------------------

/// Everything the Retail & ACX window and `ACX report.md` show.
#[derive(Debug, Clone, Default)]
pub struct RetailState {
    /// Latest export check per chapter.
    pub reports: std::collections::HashMap<Id, FileReport>,
    /// Chapters with takes newer than their check.
    pub stale: std::collections::HashSet<Id>,
    /// The retail sample's check and the chapter it came from.
    pub sample: Option<(FileReport, Option<Id>)>,
    /// The cover's inspection, when one is set.
    pub cover: Option<std::result::Result<CoverInfo, String>>,
}

impl RetailState {
    /// The stored checks (the cover is inspected separately, see
    /// [`inspect_project_cover`]).
    pub fn load(db: &crate::db::Db, project_id: Id) -> Result<Self> {
        Ok(Self {
            reports: db.export_reports(project_id)?,
            stale: db.stale_reports(project_id)?,
            sample: db.sample_report(project_id)?,
            cover: None,
        })
    }

    pub fn book_checks(&self, project: &crate::model::Project, chapters: &[Chapter]) -> Vec<Check> {
        let reports: Vec<&FileReport> = chapters
            .iter()
            .filter_map(|c| self.reports.get(&c.id))
            .collect();
        let sample = self
            .sample
            .as_ref()
            .map(|(r, cid)| (r, cid.and_then(|id| chapters.iter().find(|c| c.id == id))));
        book_checks(
            &project.settings.publication,
            &project.name,
            self.cover
                .as_ref()
                .map(|c| c.as_ref().map_err(String::as_str)),
            chapters,
            &reports,
            sample,
        )
    }

    pub fn markdown(
        &self,
        project: &crate::model::Project,
        chapters: &[Chapter],
        generated: &str,
    ) -> String {
        let rows: Vec<crate::acx::ReportRow> = chapters
            .iter()
            .enumerate()
            .map(|(i, c)| crate::acx::ReportRow {
                number: i + 1,
                title: &c.title,
                report: self.reports.get(&c.id),
                stale: self.stale.contains(&c.id),
            })
            .collect();
        crate::acx::report_markdown(
            project.settings.publication.book_title(&project.name),
            generated,
            &rows,
            &self.book_checks(project, chapters),
            self.sample.as_ref().map(|(r, _)| r),
        )
    }
}

/// The project's cover, inspected; `None` when no cover is set.
pub fn inspect_project_cover(
    library_root: &Path,
    publication: &Publication,
) -> Option<std::result::Result<CoverInfo, String>> {
    cover_file(library_root, &publication.cover_path)
        .map(|p| inspect_cover(&p).map_err(|e| format!("{e:#}")))
}

/// Write `ACX report.md` into the project's export folder.
pub fn write_report(
    db: &crate::db::Db,
    lib: &crate::db::Library,
    project: &crate::model::Project,
) -> Result<PathBuf> {
    let chapters = db.chapters(project.id)?;
    let mut state = RetailState::load(db, project.id)?;
    state.cover = inspect_project_cover(&lib.root, &project.settings.publication);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let md = state.markdown(project, &chapters, &crate::costs::fmt_datetime_utc(now));
    let dir = crate::export::export_dir(lib, project)?;
    let path = dir.join(REPORT_FILE);
    let tmp = dir.join(format!("{REPORT_FILE}.part"));
    std::fs::write(&tmp, md)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub const REPORT_FILE: &str = "ACX report.md";

/// Test fixtures shared by the export, mastering and worker tests.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::{Path, PathBuf};

    use super::Publication;
    use crate::audio::{self, Pcm};
    use crate::db::{Library, NewTake};
    use crate::model::*;

    pub(crate) struct Book {
        pub dir: tempfile::TempDir,
        pub lib: Library,
        pub project: Project,
        pub chapter: Chapter,
    }

    /// Speech-like audio: 200 ms syllables of a voiced tone with harmonics
    /// under a peaky envelope, 60 ms apart, with varying stress.
    pub(crate) fn speech(rate: u32, secs: f64, pitch: f32) -> Pcm {
        let n = (rate as f64 * secs) as usize;
        let syllable = (rate as f64 * 0.26) as usize;
        let voiced = (rate as f64 * 0.2) as usize;
        let samples = (0..n)
            .map(|i| {
                let k = i % syllable;
                if k >= voiced {
                    return 0;
                }
                // Peaky like speech: stressed and unstressed syllables.
                let stress = [1.0, 0.45, 0.7, 0.3][(i / syllable) % 4];
                let env = stress
                    * (std::f32::consts::PI * k as f32 / voiced as f32)
                        .sin()
                        .powi(8);
                let t = i as f32 / rate as f32;
                let w = 2.0 * std::f32::consts::PI * pitch * t;
                let v = w.sin() + 0.5 * (2.0 * w).sin() + 0.25 * (3.0 * w).sin();
                (v / 1.75 * env * 0.6 * 32767.0) as i16
            })
            .collect();
        Pcm { rate, samples }
    }

    pub(crate) fn have_ffmpeg() -> bool {
        crate::export::ffmpeg_available()
            && std::process::Command::new("ffprobe")
                .arg("-version")
                .output()
                .is_ok_and(|o| o.status.success())
    }

    /// A 2400×2400 JPEG made by ffmpeg.
    pub(crate) fn make_cover(dir: &Path, size: u32) -> PathBuf {
        let path = dir.join(format!("cover-{size}.jpg"));
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("color=c=0x1d3557:size={size}x{size}"))
            .args(["-frames:v", "1", "-q:v", "2"])
            .arg(&path)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg could not make a cover");
        path
    }

    /// ffprobe's JSON for tags, streams and chapters.
    pub(crate) fn probe(path: &Path) -> serde_json::Value {
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries"])
            .arg("format=format_name:format_tags:stream=codec_name,codec_type,channels,sample_rate,width,height:stream_disposition=attached_pic:chapter=start_time,end_time:chapter_tags=title")
            .args(["-of", "json"])
            .arg(path)
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// A format tag, matched case-insensitively (Vorbis comments may be upper case).
    pub(crate) fn tag(v: &serde_json::Value, key: &str) -> Option<String> {
        v["format"]["tags"]
            .as_object()?
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| v.as_str().map(String::from))
    }

    pub(crate) fn has_cover(v: &serde_json::Value) -> bool {
        v["streams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["disposition"]["attached_pic"] == 1)
    }

    /// "Test Book" with one chapter ("Opening", four segments in three
    /// paragraphs, a scene break) whose segments are all approved.
    pub(crate) fn approved_book(publication: Publication) -> Book {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().join("library"));
        let mut db = lib.open().unwrap();
        let pid = db.create_project("Test Book", "en").unwrap();
        let mut project = db.project(pid).unwrap();
        project.settings.output_format = "pcm_24000".into();
        project.settings.publication = publication;
        db.update_project(&project).unwrap();
        let cid = db
            .import_chapter(
                pid,
                "Opening",
                "\"Hi,\" she said.\n\nThe lamp was lit.\n\n***\n\nThe end.",
            )
            .unwrap();
        for p in db.paragraphs(cid).unwrap() {
            let segs: Vec<_> = crate::segment::parse_paragraph(&p.text, crate::lang::by_code("en"))
                .into_iter()
                .map(|d| (d.kind, d.speaker.clone(), d.text(&p.text).to_string()))
                .collect();
            db.replace_paragraph_segments(p.id, &segs).unwrap();
        }
        for (i, s) in db.chapter_segments(cid).unwrap().iter().enumerate() {
            let rel = format!("t{i}.wav");
            audio::write_wav(
                &lib.audio_path(&rel),
                &speech(24000, 1.5, 140.0 + 30.0 * i as f32),
            )
            .unwrap();
            let t = db
                .insert_take(&NewTake {
                    segment_id: s.id,
                    seed: 1,
                    request_id: None,
                    audio_path: &rel,
                    hash: &rel,
                    credits_used: 10,
                    credits_exact: true,
                })
                .unwrap();
            db.set_current_take(s.id, t).unwrap();
            db.set_segment_status(s.id, SegmentStatus::Approved)
                .unwrap();
        }
        let project = db.project(pid).unwrap();
        let chapter = db.chapter(cid).unwrap();
        Book {
            dir,
            lib,
            project,
            chapter,
        }
    }

    pub(crate) fn lighthouse() -> Publication {
        Publication {
            title: "The Lighthouse".into(),
            subtitle: "A Tale".into(),
            author: "Jane Author".into(),
            narrator: "Nar Rator".into(),
            publisher: "Indie Press".into(),
            year: "2026".into(),
            description: "A short book; with = signs.".into(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChapterStatus;

    fn publication() -> Publication {
        Publication {
            title: "The Lighthouse".into(),
            subtitle: "A Tale".into(),
            author: "Jane Author".into(),
            narrator: "Nar Rator".into(),
            publisher: "Indie Press".into(),
            year: "2026".into(),
            description: "A short book.".into(),
            ..Default::default()
        }
    }

    fn keys(t: &Tags) -> Vec<&str> {
        t.iter().map(|(k, _)| *k).collect()
    }

    #[test]
    fn old_projects_get_defaults() {
        let p: Publication = serde_json::from_str("{}").unwrap();
        assert_eq!(p.genre, "Audiobook");
        assert!(p.cover_in_mp3 && p.room_tone);
        let p: Publication = serde_json::from_str(r#"{"author":"A","unknown":1}"#).unwrap();
        assert_eq!(p.author, "A");
        // Project settings stored before the retail export.
        let s: crate::model::ProjectSettings =
            serde_json::from_str(r#"{"output_format":"mp3_44100_192","concurrency":2}"#).unwrap();
        assert_eq!(s.publication, Publication::default());
        assert_eq!(s.concurrency, 2);
    }

    #[test]
    fn tags_per_container() {
        let p = publication();
        let mp3 = p.mp3_tags("proj", "Chapter 1: Opening", Some((1, 12)));
        assert_eq!(
            keys(&mp3),
            [
                "title",
                "album",
                "artist",
                "album_artist",
                "composer",
                "genre",
                "date",
                "track",
                "publisher",
                "TIT3"
            ]
        );
        assert!(mp3.contains(&("track", "1/12".into())));
        assert!(mp3.contains(&("album", "The Lighthouse".into())));
        assert!(mp3.contains(&("composer", "Nar Rator".into())));

        let m4b = p.m4b_tags("proj");
        assert!(m4b.contains(&("album", "The Lighthouse: A Tale".into())));
        assert!(m4b.contains(&("copyright", "© 2026 Indie Press".into())));
        assert!(m4b.contains(&("media_type", "2".into())));
        assert!(m4b.contains(&("synopsis", "A short book.".into())));
        assert!(!keys(&m4b).contains(&"publisher"));

        // Empty fields are left out; the title falls back to the project.
        let bare = Publication::default();
        let t = bare.mp3_tags("My Project", "One", None);
        assert_eq!(keys(&t), ["title", "album", "genre"]);
        assert_eq!(t[1].1, "My Project");
        assert_eq!(bare.m4b_copyright(), "");
        let own = Publication {
            copyright: "© 2025 Someone".into(),
            ..publication()
        };
        assert_eq!(own.m4b_copyright(), "© 2025 Someone");
        assert!(
            own.flac_tags("p", "x", None)
                .contains(&("subtitle", "A Tale".into()))
        );
    }

    #[test]
    fn hints_for_missing_names_and_odd_years() {
        assert!(publication().hints().is_empty());
        assert!(Publication::default().hints().is_empty());
        let p = Publication {
            year: "26".into(),
            ..Default::default()
        };
        assert_eq!(p.hints().len(), 3);
    }

    /// Minimal JPEG: SOI, an APP0 segment, SOF0 with `comps` components.
    fn jpeg(w: u16, h: u16, comps: u8) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        d.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        let len = 8 + 3 * comps as u16;
        d.extend([0xFF, 0xC0]);
        d.extend(len.to_be_bytes());
        d.push(8);
        d.extend(h.to_be_bytes());
        d.extend(w.to_be_bytes());
        d.push(comps);
        for c in 0..comps {
            d.extend([c + 1, 0x11, 0]);
        }
        d.extend([0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9]);
        d
    }

    fn png(w: u32, h: u32, depth: u8, color: u8) -> Vec<u8> {
        let mut d = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        d.extend(w.to_be_bytes());
        d.extend(h.to_be_bytes());
        d.extend([depth, color, 0, 0, 0]);
        d.extend([0, 0, 0, 0]);
        d
    }

    #[test]
    fn cover_rules() {
        let ok = inspect_cover_bytes(&jpeg(2400, 2400, 3)).unwrap();
        assert_eq!(
            (ok.format, ok.color, ok.depth),
            (CoverFormat::Jpeg, ColorModel::Rgb, 8)
        );
        assert!(ok.problems().is_empty(), "{:?}", ok.problems());

        let small = inspect_cover_bytes(&jpeg(2400, 1600, 4)).unwrap();
        let p = small.problems();
        assert!(p.iter().any(|s| s.starts_with("not square")), "{p:?}");
        assert!(p.iter().any(|s| s.starts_with("smaller than")));
        assert!(p.iter().any(|s| s.starts_with("CMYK")));
        let gray = inspect_cover_bytes(&jpeg(3000, 3000, 1)).unwrap();
        assert_eq!(gray.color, ColorModel::Gray);

        let rgb = inspect_cover_bytes(&png(3000, 3000, 8, 2)).unwrap();
        assert_eq!((rgb.format, rgb.color), (CoverFormat::Png, ColorModel::Rgb));
        assert!(rgb.problems().is_empty());
        let rgba = inspect_cover_bytes(&png(2400, 2400, 8, 6)).unwrap();
        assert!(rgba.problems().is_empty());
        let pal = inspect_cover_bytes(&png(2400, 2400, 1, 3)).unwrap();
        let p = pal.problems();
        assert!(p.iter().any(|s| s.starts_with("palette")), "{p:?}");
        assert!(p.iter().any(|s| s.starts_with("1 bits")));

        assert!(inspect_cover_bytes(b"not an image at all").is_err());
        assert_eq!(jpeg_components(b"\xFF\xD8\xFF\xDA\x00\x02"), None);
    }

    #[test]
    fn covers_are_copied_into_the_library() {
        let lib = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let src = outside.path().join("Cover Art.JPG");
        std::fs::write(&src, jpeg(2400, 2400, 3)).unwrap();
        let rel = import_cover(lib.path(), 7, &src).unwrap();
        assert!(
            rel.starts_with("covers/project-7-") && rel.ends_with(".jpg"),
            "{rel}"
        );
        let file = cover_file(lib.path(), &rel).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), jpeg(2400, 2400, 3));
        // Importing it again (or a file already inside) keeps the path.
        assert_eq!(import_cover(lib.path(), 7, &src).unwrap(), rel);
        assert_eq!(import_cover(lib.path(), 7, &file).unwrap(), rel);
        assert_eq!(cover_file(lib.path(), "  "), None);
        assert_eq!(
            cover_file(lib.path(), "/abs/c.png").unwrap(),
            PathBuf::from("/abs/c.png")
        );
    }

    fn src(spec: &[(usize, bool, u64)]) -> Vec<SampleSource> {
        spec.iter()
            .enumerate()
            .map(|(i, &(paragraph, approved, ms))| SampleSource {
                segment_id: i as Id + 100,
                paragraph,
                scene_break_before: paragraph == 3,
                approved,
                duration_ms: ms,
            })
            .collect()
    }

    #[test]
    fn sample_ends_on_a_paragraph_and_skips_nothing() {
        let silence = SilenceSettings::default(); // 0 / 700 / 2000 ms
        // Paragraphs of 40 s, 50 s (two segments), 60 s, then a scene break.
        let s = src(&[
            (0, true, 40_000),
            (1, true, 30_000),
            (1, true, 20_000),
            (2, true, 60_000),
            (3, true, 50_000),
            (4, false, 10_000),
        ]);
        let p = plan_sample(&s, 0, 180_000, &silence).unwrap();
        // 40 + 0.7 + 50 + 0.7 + 60 = 151.4 s; the next paragraph does not fit.
        assert_eq!((p.start, p.end, p.speech_ms), (0, 4, 151_400));
        assert_eq!(p.stop, SampleStop::ParagraphEnd);
        assert_eq!(p.total_ms(), 152_900);

        // Longer: reaches the unapproved segment and stops before it.
        let p = plan_sample(&s, 0, 300_000, &silence).unwrap();
        assert_eq!((p.end, p.stop), (5, SampleStop::Unapproved(105)));
        assert_eq!(p.speech_ms, 151_400 + 2000 + 50_000);

        // From the middle of a paragraph; the paragraph is too long.
        let p = plan_sample(&s, 1, 50_000, &silence).unwrap();
        assert_eq!(
            (p.end, p.stop, p.speech_ms),
            (2, SampleStop::SegmentEnd, 30_000)
        );

        // A single segment longer than the sample is cut.
        let p = plan_sample(&s, 3, 60_000, &silence).unwrap();
        assert_eq!((p.end, p.stop, p.speech_ms), (4, SampleStop::Cut, 58_500));

        // The rest of a short chapter.
        let short = src(&[(0, true, 5_000), (1, true, 5_000)]);
        let p = plan_sample(&short, 0, 180_000, &silence).unwrap();
        assert_eq!(
            (p.end, p.stop, p.speech_ms),
            (2, SampleStop::ChapterEnd, 10_700)
        );

        assert!(plan_sample(&s, 5, 180_000, &silence).is_err());
        assert!(plan_sample(&s, 9, 180_000, &silence).is_err());
        // Never more than five minutes.
        let long = src(&[(0, true, 200_000), (1, true, 200_000)]);
        let p = plan_sample(&long, 0, 900_000, &silence).unwrap();
        assert!(p.total_ms() <= SAMPLE_MAX_MS);
    }

    fn chapter(title: &str, text: &str) -> Chapter {
        Chapter {
            id: 1,
            project_id: 1,
            order: 1,
            title: title.into(),
            source_text: text.into(),
            status: ChapterStatus::Exported,
        }
    }

    #[test]
    fn book_level_checks() {
        let p = publication();
        let chapters = vec![
            chapter(
                "Opening credits",
                "The Lighthouse, written by Jane Author, narrated by Nar Rator.",
            ),
            chapter("One", "Story."),
            chapter(
                "Closing credits",
                "You have been listening to The Lighthouse by Jane Author, read by Nar Rator.",
            ),
        ];
        let cover = CoverInfo {
            format: CoverFormat::Jpeg,
            width: 2400,
            height: 2400,
            bytes: 900_000,
            color: ColorModel::Rgb,
            depth: 8,
        };
        let checks = book_checks(&p, "proj", Some(Ok(&cover)), &chapters, &[], None);
        let by = |k: &str| checks.iter().find(|c| c.key == k).unwrap().clone();
        assert_eq!(by("cover").verdict, Verdict::Pass);
        assert_eq!(by("opening").verdict, Verdict::Pass);
        assert_eq!(by("closing").verdict, Verdict::Warn);
        assert!(by("closing").note.contains("\"The End\""));
        assert_eq!(by("sample").verdict, Verdict::Warn);

        let none = book_checks(
            &Publication::default(),
            "proj",
            None,
            &chapters[1..2],
            &[],
            None,
        );
        assert!(none.iter().all(|c| c.verdict != Verdict::Fail));
        assert_eq!(
            none.iter().filter(|c| c.verdict == Verdict::Warn).count(),
            4
        );
    }
}
