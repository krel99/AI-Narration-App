//! Delivery mastering, replacing the single-pass loudnorm (whose RMS landed
//! anywhere from -21.3 to -18.2 dB depending on how many pauses a chapter
//! has). Measure the assembled WAV, gain it to -20 dB RMS through a -4 dBFS
//! limiter, encode, then measure the encoded file; if it landed outside
//! -22.5…-18.5 dB RMS or above -3.5 dB peak, correct once and encode again.
//! Tags and cover art go into every delivery file in the same ffmpeg call.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{AssemblyWriter, export_dir, run_ffmpeg, slug, work_rate};
use crate::acx::{self, Levels};
use crate::audio;
use crate::db::{Db, Library};
use crate::model::*;
use crate::retail::{self, SamplePlan, SampleSource, SampleStop};

pub const TARGET_RMS_DB: f64 = -20.0;
/// Limiter ceiling; MP3 encoding adds a few tenths on top.
pub const LIMIT_DB: f64 = -4.0;
const MAX_GAIN_DB: f64 = 30.0;
/// Encoded files outside these get one corrective pass.
const RMS_OK: (f64, f64) = (-22.5, -18.5);
const PEAK_OK: f64 = -3.5;
/// anoisesrc pink at amplitude 0.0012: about -72.7 dB RMS, a -73.4 dB floor.
/// Mixed in after the gain stage, so the floor does not move with the gain.
const ROOM_TONE_AMPLITUDE: f64 = 0.0012;

type Tags = [(&'static str, String)];

/// One delivery file of an encode.
pub(super) enum Output<'a> {
    Mp3 {
        path: &'a Path,
        tags: &'a Tags,
        cover: bool,
    },
    Flac {
        path: &'a Path,
        tags: &'a Tags,
    },
    /// Tags and chapters come from the FFMETADATA input.
    M4b {
        path: &'a Path,
    },
}

impl Output<'_> {
    fn path(&self) -> &Path {
        match self {
            Output::Mp3 { path, .. } | Output::Flac { path, .. } | Output::M4b { path } => path,
        }
    }
}

pub(super) struct Fade {
    pub in_s: f64,
    pub out_s: f64,
    pub total_s: f64,
}

pub(super) struct Encode<'a> {
    pub wav: &'a Path,
    pub ffmetadata: Option<&'a Path>,
    pub cover: Option<&'a Path>,
    pub room_tone: bool,
    pub fade: Option<Fade>,
    /// The first output is the one measured.
    pub outputs: Vec<Output<'a>>,
}

/// What mastering did and how the delivery file measured.
#[derive(Debug, Clone, PartialEq)]
pub struct Mastered {
    pub gain_db: f64,
    pub limit_db: f64,
    /// 1, or 2 after a correction.
    pub passes: u8,
    /// The first output as measured, or why it could not be.
    pub levels: std::result::Result<Levels, String>,
    pub file: PathBuf,
}

fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

impl Encode<'_> {
    pub(super) fn filter_graph(&self, gain_db: f64, limit_db: f64) -> String {
        let mut g = format!(
            "[0:a]volume={gain_db:.2}dB,alimiter=limit={:.4}:level=false:latency=true:attack=5:release=50,\
             aresample={rate},aformat=sample_fmts=fltp:channel_layouts=mono",
            db_to_linear(limit_db).clamp(0.0625, 1.0),
            rate = super::OUTPUT_RATE
        );
        // Fades shape the voice only; the room tone under it stays even, or
        // the faded ends would measure as dead silence.
        if let Some(f) = &self.fade {
            g.push_str(&format!(
                ",afade=t=in:st=0:d={:.2},afade=t=out:st={:.3}:d={:.2}",
                f.in_s,
                (f.total_s - f.out_s).max(0.0),
                f.out_s
            ));
        }
        if self.room_tone {
            g.push_str(&format!(
                "[v];anoisesrc=color=pink:amplitude={ROOM_TONE_AMPLITUDE}:r={rate}:seed=7,\
                 aformat=sample_fmts=fltp:channel_layouts=mono[n];[v][n]amix=inputs=2:duration=first:normalize=0",
                rate = super::OUTPUT_RATE
            ));
        }
        let n = self.outputs.len();
        if n > 1 {
            g.push_str(&format!(",asplit={n}"));
        }
        for k in 0..n {
            g.push_str(&format!("[a{k}]"));
        }
        g
    }

    pub(super) fn args(&self, gain_db: f64, limit_db: f64) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-hide_banner".into(),
            "-y".into(),
            "-i".into(),
            self.wav.display().to_string(),
        ];
        let mut inputs = 1;
        let meta = self.ffmetadata.map(|m| {
            a.extend(["-i".into(), m.display().to_string()]);
            inputs += 1;
            inputs - 1
        });
        let cover = self.cover.map(|c| {
            a.extend(["-i".into(), c.display().to_string()]);
            inputs += 1;
            inputs - 1
        });
        a.extend([
            "-filter_complex".into(),
            self.filter_graph(gain_db, limit_db),
        ]);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        // Cover as an attached picture, copied as is (never re-encoded).
        let pic = |a: &mut Vec<String>, ci: usize| {
            a.extend(s(&[
                "-map",
                &format!("{ci}:v"),
                "-c:v",
                "copy",
                "-disposition:v:0",
                "attached_pic",
            ]));
        };
        let tags = |a: &mut Vec<String>, tags: &Tags| {
            for (k, v) in tags {
                a.extend(["-metadata".into(), format!("{k}={v}")]);
            }
        };
        for (k, o) in self.outputs.iter().enumerate() {
            a.extend(["-map".into(), format!("[a{k}]")]);
            match o {
                Output::Mp3 {
                    path,
                    tags: t,
                    cover: with_cover,
                } => {
                    if let (Some(ci), true) = (cover, *with_cover) {
                        pic(&mut a, ci);
                        a.extend(s(&[
                            "-metadata:s:v",
                            "title=Album cover",
                            "-metadata:s:v",
                            "comment=Cover (front)",
                        ]));
                    }
                    a.extend(s(&[
                        "-c:a",
                        "libmp3lame",
                        "-b:a",
                        "192k",
                        "-ar",
                        "44100",
                        "-ac",
                        "1",
                        "-id3v2_version",
                        "3",
                        "-write_id3v1",
                        "1",
                    ]));
                    tags(&mut a, t);
                    a.push(path.display().to_string());
                }
                Output::Flac { path, tags: t } => {
                    if let Some(ci) = cover {
                        pic(&mut a, ci);
                        a.extend(s(&["-metadata:s:v", "comment=Cover (front)"]));
                    }
                    a.extend(s(&["-c:a", "flac", "-sample_fmt", "s16"]));
                    tags(&mut a, t);
                    a.push(path.display().to_string());
                }
                Output::M4b { path } => {
                    if let Some(mi) = meta {
                        a.extend(s(&[
                            "-map_metadata",
                            &mi.to_string(),
                            "-map_chapters",
                            &mi.to_string(),
                        ]));
                    }
                    if let Some(ci) = cover {
                        pic(&mut a, ci);
                    }
                    a.extend(s(&[
                        "-c:a",
                        "aac",
                        "-b:a",
                        "128k",
                        "-ac",
                        "1",
                        "-ar",
                        "44100",
                        "-movflags",
                        "+faststart",
                        "-brand",
                        "M4B ",
                        "-f",
                        "mp4",
                    ]));
                    a.push(path.display().to_string());
                }
            }
        }
        a
    }

    /// Measure, encode, measure the result, correct once if needed.
    pub(super) fn run(&self) -> Result<Mastered> {
        let Some(first) = self.outputs.first() else {
            bail!("nothing to encode");
        };
        let pre = acx::measure_wav(self.wav)?;
        let mut gain = if pre.rms_db.is_finite() {
            (TARGET_RMS_DB - pre.rms_db).clamp(-MAX_GAIN_DB, MAX_GAIN_DB)
        } else {
            0.0
        };
        let mut limit = LIMIT_DB;
        run_ffmpeg(&self.args(gain, limit))?;
        let mut levels = acx::measure_file(first.path()).map_err(|e| format!("{e:#}"));
        let mut passes = 1;
        if let Ok(l) = &levels {
            let rms_off = l.rms_db.is_finite() && (l.rms_db < RMS_OK.0 || l.rms_db > RMS_OK.1);
            let peak_off = l.peak_db > PEAK_OK;
            if rms_off || peak_off {
                if rms_off {
                    gain = (gain + TARGET_RMS_DB - l.rms_db).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
                }
                if peak_off {
                    limit = (limit - (l.peak_db - PEAK_OK) - 0.5).max(-12.0);
                }
                log::info!(
                    "{}: RMS {:.2} dB, peak {:.2} dB; encoding again with {gain:.2} dB gain, limiter {limit:.2} dB",
                    first.path().display(),
                    l.rms_db,
                    l.peak_db
                );
                run_ffmpeg(&self.args(gain, limit))?;
                levels = acx::measure_file(first.path()).map_err(|e| format!("{e:#}"));
                passes = 2;
            }
        }
        Ok(Mastered {
            gain_db: gain,
            limit_db: limit,
            passes,
            levels,
            file: first.path().to_path_buf(),
        })
    }
}

/// The project's cover, if it is set and still there. A missing file is
/// reported in `notes` and the export goes on without it.
pub(super) fn cover(lib: &Library, project: &Project, notes: &mut Vec<String>) -> Option<PathBuf> {
    let path = retail::cover_file(&lib.root, &project.settings.publication.cover_path)?;
    if !path.is_file() {
        notes.push(format!(
            "Cover {} not found; exported without cover art.",
            path.display()
        ));
        return None;
    }
    // Only JPG and PNG go into ID3, FLAC and MP4 as they are.
    let mut head = [0u8; 16];
    let n = std::fs::File::open(&path)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    match imagesize::image_type(&head[..n]) {
        Ok(imagesize::ImageType::Jpeg | imagesize::ImageType::Png) => Some(path),
        _ => {
            notes.push(format!(
                "Cover {} is not a JPG or PNG; exported without cover art.",
                path.display()
            ));
            None
        }
    }
}

impl Encode<'_> {
    /// `run`, and if ffmpeg fails with the cover attached, once more without
    /// it: cover art never stops an export.
    fn run_or_without_cover(self, notes: &mut Vec<String>) -> Result<Mastered> {
        match self.run() {
            Err(e) if self.cover.is_some() => {
                log::warn!("encoding with the cover failed: {e:#}");
                notes.push(
                    "The cover could not be embedded (ffmpeg refused it); exported without cover art."
                        .into(),
                );
                Encode {
                    cover: None,
                    ..self
                }
                .run()
            }
            r => r,
        }
    }
}

/// Track n/N of a chapter: its position among the project's chapters.
fn track(db: &Db, chapter: &Chapter) -> Result<(usize, usize)> {
    let chapters = db.chapters(chapter.project_id)?;
    let n = chapters
        .iter()
        .position(|c| c.id == chapter.id)
        .map_or(chapter.order.max(1) as usize, |i| i + 1);
    Ok((n, chapters.len().max(n)))
}

/// Chapter MP3 (with ID3v2.3 tags and, if wanted, the cover) and the FLAC
/// master (tags and cover), mastered together. Also returns notes for the
/// user (a missing cover).
pub(super) fn encode_chapter(
    db: &Db,
    lib: &Library,
    project: &Project,
    chapter: &Chapter,
    wav: &Path,
    mp3: &Path,
    flac: &Path,
) -> Result<(Mastered, Vec<String>)> {
    let p = &project.settings.publication;
    let mut notes = Vec::new();
    let cover = cover(lib, project, &mut notes);
    let track = Some(track(db, chapter)?);
    let mp3_tags = p.mp3_tags(&project.name, &chapter.title, track);
    let flac_tags = p.flac_tags(&project.name, &chapter.title, track);
    Encode {
        wav,
        ffmetadata: None,
        cover: cover.as_deref(),
        room_tone: p.room_tone,
        fade: None,
        outputs: vec![
            Output::Mp3 {
                path: mp3,
                tags: &mp3_tags,
                cover: p.cover_in_mp3,
            },
            Output::Flac {
                path: flac,
                tags: &flac_tags,
            },
        ],
    }
    .run_or_without_cover(&mut notes)
    .map(|m| (m, notes))
}

/// The book M4B: AAC, tags and chapters from `ffmetadata`, cover attached.
pub(super) fn encode_book(
    lib: &Library,
    project: &Project,
    wav: &Path,
    ffmetadata: &Path,
    m4b: &Path,
) -> Result<(Mastered, Vec<String>)> {
    let mut notes = Vec::new();
    let cover = cover(lib, project, &mut notes);
    Encode {
        wav,
        ffmetadata: Some(ffmetadata),
        cover: cover.as_deref(),
        room_tone: project.settings.publication.room_tone,
        fade: None,
        outputs: vec![Output::M4b { path: m4b }],
    }
    .run_or_without_cover(&mut notes)
    .map(|m| (m, notes))
}

// ---- retail sample ------------------------------------------------------------------

/// The chapter's segments for the sample planner, with the length of each
/// approved segment's current take (read from the WAV header).
pub fn sample_sources(db: &Db, lib: &Library, chapter_id: Id) -> Result<Vec<SampleSource>> {
    let paragraphs = db.paragraphs(chapter_id)?;
    let index: std::collections::HashMap<Id, usize> = paragraphs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id, i))
        .collect();
    let mut out = Vec::new();
    for s in db.chapter_segments(chapter_id)? {
        let paragraph = index.get(&s.paragraph_id).copied().unwrap_or(0);
        let mut approved = s.status == SegmentStatus::Approved && s.current_take_id.is_some();
        let mut duration_ms = 0;
        if approved {
            let take = db.take(s.current_take_id.unwrap_or_default())?;
            match hound::WavReader::open(lib.audio_path(&take.audio_path)) {
                Ok(r) => {
                    let spec = r.spec();
                    duration_ms = r.duration() as u64 * 1000 / spec.sample_rate.max(1) as u64;
                }
                Err(e) => {
                    log::warn!("take {}: {e}", take.id);
                    approved = false;
                }
            }
        }
        out.push(SampleSource {
            segment_id: s.id,
            paragraph,
            scene_break_before: paragraphs
                .get(paragraph)
                .is_some_and(|p| p.scene_break_before),
            approved,
            duration_ms,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct SampleExport {
    pub plan: SamplePlan,
    pub mastered: Mastered,
    pub notes: Vec<String>,
}

pub fn sample_file_name(project: &Project) -> String {
    format!(
        "{} - Retail Sample.mp3",
        slug(project.settings.publication.book_title(&project.name))
    )
}

/// Export "<Book title> - Retail Sample.mp3": approved takes from
/// `start_segment` (or the chapter start), at most `max_ms`, faded in and
/// out, mastered like the chapters.
pub fn export_sample(
    db: &Db,
    lib: &Library,
    project: &Project,
    chapter: &Chapter,
    start_segment: Option<Id>,
    max_ms: u64,
) -> Result<SampleExport> {
    let src = sample_sources(db, lib, chapter.id)?;
    let start = match start_segment {
        Some(id) => src
            .iter()
            .position(|s| s.segment_id == id)
            .context("the start segment is not in this chapter")?,
        None => 0,
    };
    let plan = retail::plan_sample(&src, start, max_ms, &project.settings.silence)
        .map_err(anyhow::Error::msg)?;
    let dir = export_dir(lib, project)?;
    let wav = dir.join("retail sample.assembly.wav");
    let mp3 = dir.join(sample_file_name(project));
    let segments = db.chapter_segments(chapter.id)?;
    let mut notes = Vec::new();
    let result = (|| -> Result<Mastered> {
        let mut w = AssemblyWriter::create(&wav, work_rate(project))?;
        w.silence(retail::SAMPLE_LEAD_IN_MS as u32)?;
        for i in plan.start..plan.end {
            if i > plan.start {
                w.silence(retail::gap_before(&src, i, &project.settings.silence) as u32)?;
            }
            let seg = segments
                .iter()
                .find(|s| s.id == src[i].segment_id)
                .context("segment vanished")?;
            let take = db.take(seg.current_take_id.context("segment has no take")?)?;
            let mut pcm = audio::read_wav(&lib.audio_path(&take.audio_path))?;
            if plan.stop == SampleStop::Cut {
                let keep = (pcm.rate as u64 * plan.speech_ms / 1000) as usize;
                pcm.samples.truncate(keep);
            }
            w.append(&pcm)?;
        }
        w.silence(retail::SAMPLE_TAIL_MS as u32)?;
        let total_s = w.position_ms() as f64 / 1000.0;
        w.finish()?;
        let tags = project.settings.publication.sample_tags(&project.name);
        let cover = cover(lib, project, &mut notes);
        let p = &project.settings.publication;
        Encode {
            wav: &wav,
            ffmetadata: None,
            cover: cover.as_deref(),
            room_tone: p.room_tone,
            fade: Some(Fade {
                in_s: retail::SAMPLE_FADE_IN_S,
                out_s: retail::SAMPLE_FADE_OUT_S,
                total_s,
            }),
            outputs: vec![Output::Mp3 {
                path: &mp3,
                tags: &tags,
                cover: p.cover_in_mp3,
            }],
        }
        .run_or_without_cover(&mut notes)
    })();
    let _ = std::fs::remove_file(&wav);
    let mastered = result?;
    if let SampleStop::Unapproved(id) = plan.stop {
        let n = src
            .iter()
            .position(|s| s.segment_id == id)
            .map_or(0, |i| i + 1);
        notes.push(format!(
            "The sample stops before segment {n}, which is not approved yet (samples use approved takes only)."
        ));
    }
    Ok(SampleExport {
        plan,
        mastered,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retail::fixture::{self, approved_book, has_cover, lighthouse, tag};

    /// Chapter MP3 + FLAC, book M4B and retail sample: tags, cover art,
    /// chapters and levels as ffprobe and the meter see them.
    #[test]
    fn delivery_files_carry_tags_cover_and_levels() {
        if !fixture::have_ffmpeg() {
            eprintln!("ffmpeg/ffprobe not found; skipping");
            return;
        }
        let mut book = approved_book(lighthouse());
        let cover_src = fixture::make_cover(book.dir.path(), 2400);
        book.project.settings.publication.cover_path =
            retail::import_cover(&book.lib.root, book.project.id, &cover_src).unwrap();
        let mut db = book.lib.open().unwrap();
        db.update_project(&book.project).unwrap();
        // A second chapter makes the track numbers n/2.
        db.import_chapter(book.project.id, "Two", "Later.").unwrap();

        let res = super::super::export_chapter(&db, &book.lib, &book.project, &book.chapter, false)
            .unwrap();
        assert!(res.notes.is_empty(), "{:?}", res.notes);
        let (mp3, flac) = (&res.files[0], &res.files[1]);
        let v = fixture::probe(mp3);
        for (k, want) in [
            ("title", "Opening"),
            ("album", "The Lighthouse"),
            ("artist", "Jane Author"),
            ("album_artist", "Jane Author"),
            ("composer", "Nar Rator"),
            ("genre", "Audiobook"),
            ("date", "2026"),
            ("track", "1/2"),
            ("publisher", "Indie Press"),
            ("TIT3", "A Tale"),
        ] {
            assert_eq!(tag(&v, k).as_deref(), Some(want), "mp3 {k}: {v}");
        }
        assert!(has_cover(&v), "mp3 cover: {v}");
        let audio = &v["streams"][0];
        assert_eq!(
            (audio["codec_name"].as_str(), audio["sample_rate"].as_str()),
            (Some("mp3"), Some("44100"))
        );
        assert_eq!(acx::mp3_cbr(mp3).unwrap(), Some(true));
        let c = acx::probe(mp3).unwrap();
        assert_eq!((c.channels, c.bit_rate), (1, Some(192_000)));

        let v = fixture::probe(flac);
        assert_eq!(tag(&v, "composer").as_deref(), Some("Nar Rator"));
        assert_eq!(tag(&v, "album").as_deref(), Some("The Lighthouse"));
        assert!(has_cover(&v), "flac cover: {v}");

        // Levels inside the ACX ranges, with room tone instead of silence.
        let m = res.mastered.as_ref().unwrap();
        let l = m.levels.clone().unwrap();
        assert!((-22.5..=-18.5).contains(&l.rms_db), "{m:?}");
        assert!(l.peak_db <= -3.0, "{m:?}");
        assert!((-80.0..=-65.0).contains(&l.noise_floor_db), "{m:?}");
        assert!((600..=1500).contains(&l.head_ms), "{m:?}");
        assert!((1500..=3000).contains(&l.tail_ms), "{m:?}");
        let checks = acx::evaluate(acx::FileKind::Chapter, &l, Ok(&c), Some(true), 1000);
        assert_eq!(acx::worst(&checks), acx::Verdict::Pass, "{checks:#?}");

        // M4B: book tags, stik audiobook, cover, chapter markers.
        let m4b = super::super::export_book(&db, &book.lib, &book.project).unwrap();
        let v = fixture::probe(&m4b.files[0]);
        for (k, want) in [
            ("title", "The Lighthouse"),
            ("album", "The Lighthouse: A Tale"),
            ("artist", "Jane Author"),
            ("composer", "Nar Rator"),
            ("genre", "Audiobook"),
            ("date", "2026"),
            ("media_type", "2"),
            ("copyright", "© 2026 Indie Press"),
            ("description", "A short book; with = signs."),
            ("major_brand", "M4B "),
        ] {
            assert_eq!(tag(&v, k).as_deref(), Some(want), "m4b {k}: {v}");
        }
        assert!(has_cover(&v), "m4b cover: {v}");
        let chapters = v["chapters"].as_array().unwrap();
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0]["tags"]["title"], "Opening");
        let l = m4b.mastered.unwrap().levels.unwrap();
        assert!((-23.0..=-18.0).contains(&l.rms_db), "{l:?}");

        // Retail sample from the second segment: faded, tagged, measured.
        let segs = db.chapter_segments(book.chapter.id).unwrap();
        let out = export_sample(
            &db,
            &book.lib,
            &book.project,
            &book.chapter,
            Some(segs[1].id),
            180_000,
        )
        .unwrap();
        assert_eq!(
            out.mastered.file.file_name().unwrap(),
            "The Lighthouse - Retail Sample.mp3"
        );
        assert_eq!(
            (out.plan.start, out.plan.end, out.plan.stop),
            (1, 4, SampleStop::ChapterEnd)
        );
        let l = out.mastered.levels.clone().unwrap();
        assert!(
            (l.duration_ms as i64 - out.plan.total_ms() as i64).abs() < 100,
            "{l:?} {:?}",
            out.plan
        );
        assert!((-23.0..=-18.0).contains(&l.rms_db), "{l:?}");
        // The fades shape the voice, not the room tone: no dead silence.
        assert!((-80.0..=-65.0).contains(&l.noise_floor_db), "{l:?}");
        let v = fixture::probe(&out.mastered.file);
        assert_eq!(tag(&v, "title").as_deref(), Some("Retail Sample"));
        assert_eq!(tag(&v, "album").as_deref(), Some("The Lighthouse"));
        assert!(has_cover(&v));

        // Without room tone the pauses are digital silence again; without
        // cover_in_mp3 the MP3 has no picture but the FLAC keeps it.
        book.project.settings.publication.room_tone = false;
        book.project.settings.publication.cover_in_mp3 = false;
        let res = super::super::export_chapter(&db, &book.lib, &book.project, &book.chapter, true)
            .unwrap();
        let l = res.mastered.unwrap().levels.unwrap();
        assert!(l.noise_floor_db < -90.0, "{l:?}");
        assert!(!has_cover(&fixture::probe(&res.files[0])));
        assert!(has_cover(&fixture::probe(&res.files[1])));

        // A cover that went missing is reported, and the export goes on.
        book.project.settings.publication.cover_path = "covers/gone.jpg".into();
        let res = super::super::export_chapter(&db, &book.lib, &book.project, &book.chapter, true)
            .unwrap();
        assert_eq!(res.notes.len(), 1, "{:?}", res.notes);
        assert!(!has_cover(&fixture::probe(&res.files[1])));

        // Neither a non-image nor a JPEG ffmpeg cannot read stops an export.
        for (name, bytes) in [
            ("covers/anim.gif", b"GIF89a\x01\x00\x01\x00".to_vec()),
            (
                "covers/broken.jpg",
                [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[0x13; 64]].concat(),
            ),
        ] {
            let path = book.lib.root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            book.project.settings.publication.cover_path = name.into();
            let res =
                super::super::export_chapter(&db, &book.lib, &book.project, &book.chapter, true)
                    .unwrap();
            assert_eq!(res.notes.len(), 1, "{name}: {:?}", res.notes);
            assert!(
                res.notes[0].contains("without cover art"),
                "{:?}",
                res.notes
            );
            assert!(res.mastered.unwrap().levels.is_ok());
        }
    }

    #[test]
    fn a_quiet_master_gets_one_correction() {
        if !fixture::have_ffmpeg() {
            return;
        }
        // Speech under long pauses: one gain from the WAV, then the limiter
        // and MP3 encode shift it; the result must land in range anyway.
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("in.wav");
        let mut pcm = fixture::speech(44100, 2.0, 160.0);
        // Loud bursts, so the limiter takes a lot off and RMS lands low.
        for s in pcm.samples.iter_mut() {
            *s = (*s as i32 * 2).clamp(-32767, 32767) as i16;
        }
        pcm.append_silence(9000);
        let tail = fixture::speech(44100, 1.0, 200.0);
        pcm.samples.extend(tail.samples);
        pcm.append_silence(2000);
        audio::write_wav(&wav, &pcm).unwrap();
        let mp3 = dir.path().join("out.mp3");
        let m = Encode {
            wav: &wav,
            ffmetadata: None,
            cover: None,
            room_tone: true,
            fade: None,
            outputs: vec![Output::Mp3 {
                path: &mp3,
                tags: &[],
                cover: false,
            }],
        }
        .run()
        .unwrap();
        assert_eq!(m.passes, 2, "{m:?}");
        let l = m.levels.unwrap();
        assert!(
            (-22.5..=-18.5).contains(&l.rms_db),
            "{l:?} gain {}",
            m.gain_db
        );
        assert!(l.peak_db <= -3.0, "{l:?}");
    }

    #[test]
    fn filter_graph_and_arguments() {
        let wav = Path::new("/x/in.wav");
        let mp3 = Path::new("/x/out.mp3");
        let flac = Path::new("/x/out.flac");
        let tags = vec![("title", "One = 1".to_string())];
        let enc = Encode {
            wav,
            ffmetadata: None,
            cover: Some(Path::new("/x/cover.jpg")),
            room_tone: true,
            fade: None,
            outputs: vec![
                Output::Mp3 {
                    path: mp3,
                    tags: &tags,
                    cover: false,
                },
                Output::Flac {
                    path: flac,
                    tags: &tags,
                },
            ],
        };
        let g = enc.filter_graph(3.5, -4.0);
        assert!(
            g.starts_with("[0:a]volume=3.50dB,alimiter=limit=0.6310:level=false:latency=true"),
            "{g}"
        );
        assert!(g.contains("anoisesrc=color=pink:amplitude=0.0012:r=44100:seed=7"));
        assert!(g.contains("amix=inputs=2:duration=first:normalize=0"));
        assert!(g.ends_with(",asplit=2[a0][a1]"));
        let a = enc.args(3.5, -4.0);
        let joined = a.join(" ");
        // The MP3 opted out of the cover; the FLAC gets it.
        let split = joined.find("-map [a1]").unwrap();
        let (mp3_part, flac_part) = joined.split_at(split);
        assert!(!mp3_part.contains("attached_pic"));
        assert!(flac_part.contains("-map 1:v -c:v copy -disposition:v:0 attached_pic"));
        assert!(mp3_part.contains("-id3v2_version 3"));
        assert!(a.contains(&"title=One = 1".to_string()));

        let m4b = Path::new("/x/book.m4b");
        let enc = Encode {
            wav,
            ffmetadata: Some(Path::new("/x/book.ffmetadata")),
            cover: Some(Path::new("/x/cover.png")),
            room_tone: false,
            fade: Some(Fade {
                in_s: 0.5,
                out_s: 3.0,
                total_s: 30.0,
            }),
            outputs: vec![Output::M4b { path: m4b }],
        };
        let g = enc.filter_graph(-2.0, -4.0);
        assert!(!g.contains("anoisesrc"));
        assert!(
            g.ends_with(",afade=t=in:st=0:d=0.50,afade=t=out:st=27.000:d=3.00[a0]"),
            "{g}"
        );
        // With room tone, the bed is mixed in after the fades.
        let with_bed = Encode {
            room_tone: true,
            fade: Some(Fade {
                in_s: 0.5,
                out_s: 3.0,
                total_s: 30.0,
            }),
            outputs: vec![Output::M4b { path: m4b }],
            ..enc
        };
        let g = with_bed.filter_graph(-2.0, -4.0);
        assert!(
            g.find("afade=t=out").unwrap() < g.find("anoisesrc").unwrap(),
            "{g}"
        );
        let enc = Encode {
            room_tone: false,
            fade: None,
            outputs: vec![Output::M4b { path: m4b }],
            ..with_bed
        };
        let joined = enc.args(-2.0, -4.0).join(" ");
        assert!(
            joined.contains("-map_metadata 1 -map_chapters 1 -map 2:v -c:v copy"),
            "{joined}"
        );
        assert!(joined.contains("-brand M4B  -f mp4"));
    }
}
