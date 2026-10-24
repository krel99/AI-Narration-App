//! Assemble approved takes into a chapter (or a whole book) and encode the
//! delivery formats in one ffmpeg pass. Assembly streams straight to a WAV
//! file so a long book never has to fit in memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::audio::{self, Pcm};
use crate::db::{Db, Library};
use crate::model::*;

/// loudnorm targets inside ACX limits (RMS -23…-18 dB, peaks below -3 dB).
const LOUDNORM: &str = "loudnorm=I=-19:TP=-3.5:LRA=11";
const OUTPUT_RATE: u32 = 44100;
/// Placeholder for a missing take in a draft export.
const GAP_MS: u32 = 1000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readiness {
    pub segments: usize,
    pub approved: usize,
    /// Segments without a current take.
    pub missing_audio: usize,
    /// Paragraphs that have no segments yet.
    pub unsegmented_paragraphs: usize,
}

impl Readiness {
    pub fn complete(&self) -> bool {
        self.segments > 0
            && self.approved == self.segments
            && self.missing_audio == 0
            && self.unsegmented_paragraphs == 0
    }
}

pub fn readiness(db: &Db, chapter_id: Id) -> Result<Readiness> {
    let paragraphs = db.paragraphs(chapter_id)?;
    let segments = db.chapter_segments(chapter_id)?;
    let mut with_segments = std::collections::HashSet::new();
    let mut r = Readiness {
        segments: segments.len(),
        ..Default::default()
    };
    for s in &segments {
        with_segments.insert(s.paragraph_id);
        if s.status == SegmentStatus::Approved {
            r.approved += 1;
        }
        if s.current_take_id.is_none() {
            r.missing_audio += 1;
        }
    }
    r.unsegmented_paragraphs = paragraphs
        .iter()
        .filter(|p| !with_segments.contains(&p.id))
        .count();
    Ok(r)
}

/// Streams mono 16-bit samples into a WAV file.
struct AssemblyWriter {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    rate: u32,
    samples: u64,
}

impl AssemblyWriter {
    fn create(path: &Path, rate: u32) -> Result<Self> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        Ok(Self {
            writer: hound::WavWriter::create(path, spec)
                .with_context(|| format!("creating {}", path.display()))?,
            rate,
            samples: 0,
        })
    }

    fn silence(&mut self, ms: u32) -> Result<()> {
        let n = self.rate as u64 * ms as u64 / 1000;
        for _ in 0..n {
            self.writer.write_sample(0i16)?;
        }
        self.samples += n;
        Ok(())
    }

    fn append(&mut self, pcm: &Pcm) -> Result<()> {
        let resampled;
        let pcm = if pcm.rate == self.rate {
            pcm
        } else {
            resampled = audio::resample_linear(pcm, self.rate);
            &resampled
        };
        for &s in &pcm.samples {
            self.writer.write_sample(s)?;
        }
        self.samples += pcm.samples.len() as u64;
        Ok(())
    }

    fn position_ms(&self) -> u64 {
        self.samples * 1000 / self.rate as u64
    }

    fn finish(self) -> Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChapterReport {
    /// Segments that had no audio (draft export only).
    pub gaps: Vec<Id>,
    pub duration_ms: u64,
}

fn write_chapter(
    w: &mut AssemblyWriter,
    db: &Db,
    lib: &Library,
    project: &Project,
    chapter_id: Id,
    draft: bool,
) -> Result<ChapterReport> {
    let silence = &project.settings.silence;
    let paragraphs = db.paragraphs(chapter_id)?;
    let mut by_paragraph: HashMap<Id, Vec<Segment>> = HashMap::new();
    for s in db.chapter_segments(chapter_id)? {
        by_paragraph.entry(s.paragraph_id).or_default().push(s);
    }
    let start = w.position_ms();
    let mut report = ChapterReport::default();
    w.silence(silence.chapter_lead_in_ms)?;
    for (pi, para) in paragraphs.iter().enumerate() {
        if pi > 0 {
            w.silence(if para.scene_break_before {
                silence.scene_break_ms
            } else {
                silence.between_paragraphs_ms
            })?;
        }
        let segs = by_paragraph.get(&para.id).map(Vec::as_slice).unwrap_or(&[]);
        if segs.is_empty() {
            if !draft {
                bail!("paragraph {} has no segments", pi + 1);
            }
            w.silence(GAP_MS)?;
            continue;
        }
        for (si, seg) in segs.iter().enumerate() {
            if si > 0 {
                w.silence(silence.within_paragraph_ms)?;
            }
            let usable = draft || seg.status == SegmentStatus::Approved;
            match seg.current_take_id.filter(|_| usable) {
                Some(take_id) => {
                    let take = db.take(take_id)?;
                    let pcm = crate::trim::load_take(lib, &take, &project.settings.trim)?;
                    w.append(&pcm)?;
                }
                None if draft => {
                    report.gaps.push(seg.id);
                    w.silence(GAP_MS)?;
                }
                None => bail!("segment {} is not approved", seg.id),
            }
        }
    }
    w.silence(silence.chapter_tail_ms)?;
    report.duration_ms = w.position_ms() - start;
    Ok(report)
}

fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() { "untitled".into() } else { s }
}

fn run_ffmpeg(args: &[String]) -> Result<()> {
    log::info!("ffmpeg {}", args.join(" "));
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .context("running ffmpeg (is it installed and on PATH?)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(8).collect();
        bail!(
            "ffmpeg failed:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn export_dir(lib: &Library, project: &Project) -> Result<PathBuf> {
    let dir = lib.exports_dir().join(slug(&project.name));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn work_rate(project: &Project) -> u32 {
    audio::pcm_format_rate(&project.settings.output_format).unwrap_or(OUTPUT_RATE)
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub files: Vec<PathBuf>,
    pub gaps: Vec<Id>,
    pub duration_ms: u64,
}

/// Export one chapter as MP3 192 kbps CBR 44.1 kHz (ACX) plus a FLAC master.
/// A draft export allows unapproved segments and fills gaps with silence.
pub fn export_chapter(
    db: &Db,
    lib: &Library,
    project: &Project,
    chapter: &Chapter,
    draft: bool,
) -> Result<ExportResult> {
    let r = readiness(db, chapter.id)?;
    if !draft && !r.complete() {
        bail!(
            "{} of {} segments approved, {} without audio, {} paragraphs unsegmented",
            r.approved,
            r.segments,
            r.missing_audio,
            r.unsegmented_paragraphs
        );
    }
    let dir = export_dir(lib, project)?;
    let base = format!(
        "{:02} - {}{}",
        chapter.order,
        slug(&chapter.title),
        if draft { " (draft)" } else { "" }
    );
    let master = dir.join(format!("{base}.assembly.wav"));
    let mut w = AssemblyWriter::create(&master, work_rate(project))?;
    let report = write_chapter(&mut w, db, lib, project, chapter.id, draft)?;
    w.finish()?;

    let mp3 = dir.join(format!("{base}.mp3"));
    let flac = dir.join(format!("{base}.flac"));
    let args: Vec<String> = vec![
        "-hide_banner".into(),
        "-y".into(),
        "-i".into(),
        master.display().to_string(),
        "-filter_complex".into(),
        format!("[0:a]{LOUDNORM},aresample={OUTPUT_RATE},asplit=2[mp3][flac]"),
        "-map".into(),
        "[mp3]".into(),
        "-c:a".into(),
        "libmp3lame".into(),
        "-b:a".into(),
        "192k".into(),
        "-ar".into(),
        OUTPUT_RATE.to_string(),
        "-ac".into(),
        "1".into(),
        "-metadata".into(),
        format!("title={}", chapter.title),
        "-metadata".into(),
        format!("album={}", project.name),
        "-metadata".into(),
        format!("track={}", chapter.order),
        mp3.display().to_string(),
        "-map".into(),
        "[flac]".into(),
        "-c:a".into(),
        "flac".into(),
        "-sample_fmt".into(),
        "s16".into(),
        "-metadata".into(),
        format!("title={}", chapter.title),
        flac.display().to_string(),
    ];
    let result = run_ffmpeg(&args);
    let _ = std::fs::remove_file(&master);
    result?;
    Ok(ExportResult {
        files: vec![mp3, flac],
        gaps: report.gaps,
        duration_ms: report.duration_ms,
    })
}

/// FFMETADATA chapter list for the M4B container.
pub fn ffmetadata(title: &str, chapters: &[(String, u64, u64)]) -> String {
    let esc = |s: &str| {
        s.chars()
            .flat_map(|c| match c {
                '=' | ';' | '#' | '\\' | '\n' => vec!['\\', c],
                c => vec![c],
            })
            .collect::<String>()
    };
    let mut s = format!(";FFMETADATA1\ntitle={}\n", esc(title));
    for (t, start, end) in chapters {
        s.push_str(&format!(
            "\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle={}\n",
            esc(t)
        ));
    }
    s
}

/// Export every fully approved chapter as one M4B with chapter markers.
pub fn export_book(db: &Db, lib: &Library, project: &Project) -> Result<ExportResult> {
    let chapters: Vec<Chapter> = db
        .chapters(project.id)?
        .into_iter()
        .filter(|c| readiness(db, c.id).is_ok_and(|r| r.complete()))
        .collect();
    if chapters.is_empty() {
        bail!("no chapter is fully approved yet");
    }
    let dir = export_dir(lib, project)?;
    let master = dir.join("book.assembly.wav");
    let meta_path = dir.join("book.ffmetadata");
    let mut w = AssemblyWriter::create(&master, work_rate(project))?;
    let mut marks = Vec::new();
    for ch in &chapters {
        let start = w.position_ms();
        write_chapter(&mut w, db, lib, project, ch.id, false)?;
        marks.push((ch.title.clone(), start, w.position_ms()));
    }
    let duration_ms = w.position_ms();
    w.finish()?;
    std::fs::write(&meta_path, ffmetadata(&project.name, &marks))?;

    let m4b = dir.join(format!("{}.m4b", slug(&project.name)));
    let args: Vec<String> = vec![
        "-hide_banner".into(),
        "-y".into(),
        "-i".into(),
        master.display().to_string(),
        "-i".into(),
        meta_path.display().to_string(),
        "-map".into(),
        "0:a".into(),
        "-map_metadata".into(),
        "1".into(),
        "-map_chapters".into(),
        "1".into(),
        "-af".into(),
        format!("{LOUDNORM},aresample={OUTPUT_RATE}"),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "128k".into(),
        "-ac".into(),
        "1".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-f".into(),
        "mp4".into(),
        m4b.display().to_string(),
    ];
    let result = run_ffmpeg(&args);
    let _ = std::fs::remove_file(&master);
    let _ = std::fs::remove_file(&meta_path);
    result?;
    Ok(ExportResult {
        files: vec![m4b],
        gaps: vec![],
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewTake;
    use crate::segment;

    #[test]
    fn metadata_escapes_and_lists_chapters() {
        let m = ffmetadata(
            "My=Book",
            &[("One; start".into(), 0, 1500), ("Two".into(), 1500, 3000)],
        );
        assert!(m.starts_with(";FFMETADATA1\ntitle=My\\=Book\n"));
        assert!(m.contains("START=1500\nEND=3000\ntitle=Two"));
        assert!(m.contains("title=One\\; start"));
    }

    /// Full assembly + ffmpeg run on synthetic takes. Skipped without ffmpeg.
    #[test]
    fn export_chapter_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let mut db = lib.open().unwrap();
        let pid = db.create_project("Test Book", "en").unwrap();
        let mut project = db.project(pid).unwrap();
        project.settings.output_format = "pcm_24000".into();
        let cid = db
            .import_chapter(pid, "Opening", "\"Hi,\" she said.\n\n***\n\nThe end.")
            .unwrap();
        for p in db.paragraphs(cid).unwrap() {
            let segs: Vec<_> = segment::parse_paragraph(&p.text, crate::lang::by_code("en"))
                .into_iter()
                .map(|d| (d.kind, d.speaker.clone(), d.text(&p.text).to_string()))
                .collect();
            db.replace_paragraph_segments(p.id, &segs).unwrap();
        }
        let segs = db.chapter_segments(cid).unwrap();
        assert_eq!(segs.len(), 3);

        // Not ready: nothing approved.
        assert!(!readiness(&db, cid).unwrap().complete());

        for (i, s) in segs.iter().enumerate() {
            let rel = format!("t{i}.wav");
            let tone: Vec<i16> = (0..2400)
                .map(|n| ((n as f32 * 0.05).sin() * 8000.0) as i16)
                .collect();
            audio::write_wav(
                &lib.audio_path(&rel),
                &Pcm {
                    rate: 24000,
                    samples: tone,
                },
            )
            .unwrap();
            let t = db
                .insert_take(&NewTake {
                    segment_id: s.id,
                    seed: 1,
                    request_id: None,
                    audio_path: &rel,
                    hash: &rel,
                    credits_used: 1,
                    credits_exact: true,
                })
                .unwrap();
            db.set_current_take(s.id, t).unwrap();
            db.set_segment_status(s.id, SegmentStatus::Approved)
                .unwrap();
        }
        assert!(readiness(&db, cid).unwrap().complete());

        // Assembly length: lead-in + 3×100 ms takes + scene break + tail.
        let chapter = db.chapter(cid).unwrap();
        let wav = dir.path().join("check.wav");
        let mut w = AssemblyWriter::create(&wav, 24000).unwrap();
        let report = write_chapter(&mut w, &db, &lib, &project, cid, false).unwrap();
        w.finish().unwrap();
        let s = &project.settings.silence;
        let expected = s.chapter_lead_in_ms
            + 300
            + s.within_paragraph_ms
            + s.scene_break_ms
            + s.chapter_tail_ms;
        assert_eq!(report.duration_ms, expected as u64);

        // A trimmed take is assembled at its trimmed length: keep 20..80 ms.
        let segs = db.chapter_segments(cid).unwrap();
        db.set_take_trim(segs[0].current_take_id.unwrap(), Some((20, 80)))
            .unwrap();
        let mut w = AssemblyWriter::create(&wav, 24000).unwrap();
        let report = write_chapter(&mut w, &db, &lib, &project, cid, false).unwrap();
        w.finish().unwrap();
        assert_eq!(report.duration_ms, expected as u64 - 40);

        if !ffmpeg_available() {
            eprintln!("ffmpeg not found; skipping encode");
            return;
        }
        let res = export_chapter(&db, &lib, &project, &chapter, false).unwrap();
        for f in &res.files {
            assert!(
                std::fs::metadata(f).unwrap().len() > 0,
                "{} is empty",
                f.display()
            );
        }
        let book = export_book(&db, &lib, &project).unwrap();
        assert!(book.files[0].extension().unwrap() == "m4b");
    }
}
