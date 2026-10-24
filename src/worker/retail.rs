//! Export jobs with the retail checks: every chapter export is measured
//! against the ACX/Findaway spec and stored; the retail sample likewise;
//! `ACX report.md` is rewritten after each.

use std::path::Path;

use anyhow::Result;

use super::{Ctx, Event};
use crate::acx::{self, FileKind, FileReport};
use crate::db::{Db, Library};
use crate::export::{self, Mastered};
use crate::model::*;
use crate::retail;

/// Probe and check a mastered delivery file.
pub(super) fn check_file(m: &Mastered, kind: FileKind, draft: bool) -> Result<FileReport, String> {
    let levels = m.levels.clone()?;
    let container = acx::probe(&m.file).map_err(|e| format!("{e:#}"));
    let cbr = acx::mp3_cbr(&m.file).ok().flatten();
    let size = std::fs::metadata(&m.file).map_or(0, |md| md.len());
    let checks = acx::evaluate(
        kind,
        &levels,
        container.as_ref().map_err(String::as_str),
        cbr,
        size,
    );
    Ok(FileReport {
        file: file_name(&m.file),
        draft,
        levels,
        checks,
        checked_at: String::new(),
        takes_key: None,
    })
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Rewrite the report; a failure only costs the file, not the export.
fn write_report(db: &Db, lib: &Library, project: &Project) {
    if let Err(e) = retail::write_report(db, lib, project) {
        log::warn!("writing {}: {e:#}", retail::REPORT_FILE);
    }
}

fn emit_check(ctx: &Ctx, what: &str, check: Result<FileReport, String>, stored: bool) {
    match check {
        Ok(r) => ctx.out.emit(Event::AcxChecked {
            summary: if stored {
                r.summary(what)
            } else {
                format!(
                    "{} (Draft: the report keeps the final export's check.)",
                    r.summary(what)
                )
            },
            verdict: r.verdict(),
        }),
        Err(e) => ctx
            .out
            .emit(Event::Info(format!("{what}: ACX check skipped ({e})."))),
    }
}

pub(super) async fn export_chapter(ctx: &Ctx, chapter_id: Id, draft: bool) -> Result<()> {
    let lib = ctx.lib.clone();
    let (res, title, check, stored) = tokio::task::spawn_blocking(move || -> Result<_> {
        let db = lib.open()?;
        let chapter = db.chapter(chapter_id)?;
        let project = db.project(chapter.project_id)?;
        // What the export is made of, to tell later whether it is outdated.
        let takes_key = retail::segments_key(
            &db.chapter_segments(chapter_id)?,
            &db.chapter_trims(chapter_id)?,
        );
        let res = export::export_chapter(&db, &lib, &project, &chapter, draft)?;
        if !draft {
            db.set_chapter_status(chapter_id, ChapterStatus::Exported)?;
        }
        let check = res
            .mastered
            .as_ref()
            .map_or(Err("not measured".to_string()), |m| {
                check_file(m, FileKind::Chapter, draft)
            })
            .map(|r| FileReport {
                takes_key: Some(takes_key),
                ..r
            });
        let mut stored = true;
        if let Ok(r) = &check {
            stored = db.save_export_report(chapter_id, r)?;
        }
        write_report(&db, &lib, &project);
        Ok((res, chapter.title, check, stored))
    })
    .await??;
    for n in &res.notes {
        ctx.out.emit(Event::Info(n.clone()));
    }
    ctx.out.emit(Event::ExportFinished {
        files: res.files,
        gaps: res.gaps.len(),
        duration_ms: res.duration_ms,
    });
    emit_check(ctx, &title, check, stored);
    Ok(())
}

pub(super) async fn export_book(ctx: &Ctx, project_id: Id) -> Result<()> {
    let lib = ctx.lib.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<export::ExportResult> {
        let db = lib.open()?;
        let project = db.project(project_id)?;
        export::export_book(&db, &lib, &project)
    })
    .await??;
    for n in &res.notes {
        ctx.out.emit(Event::Info(n.clone()));
    }
    if let Some(Ok(l)) = res.mastered.as_ref().map(|m| &m.levels) {
        log::info!(
            "book: RMS {}, peak {}",
            acx::fmt_db(l.rms_db),
            acx::fmt_db(l.peak_db)
        );
    }
    ctx.out.emit(Event::ExportFinished {
        files: res.files,
        gaps: 0,
        duration_ms: res.duration_ms,
    });
    Ok(())
}

pub(super) async fn export_sample(
    ctx: &Ctx,
    chapter_id: Id,
    start_segment: Option<Id>,
    max_ms: u64,
) -> Result<()> {
    let lib = ctx.lib.clone();
    let (out, check) = tokio::task::spawn_blocking(move || -> Result<_> {
        let db = lib.open()?;
        let chapter = db.chapter(chapter_id)?;
        let project = db.project(chapter.project_id)?;
        let out = export::export_sample(&db, &lib, &project, &chapter, start_segment, max_ms)?;
        let check = check_file(&out.mastered, FileKind::Sample, false);
        if let Ok(r) = &check {
            db.save_sample_report(project.id, Some(chapter_id), r)?;
        }
        write_report(&db, &lib, &project);
        Ok((out, check))
    })
    .await??;
    for n in &out.notes {
        ctx.out.emit(Event::Info(n.clone()));
    }
    ctx.out.emit(Event::ExportFinished {
        files: vec![out.mastered.file.clone()],
        gaps: 0,
        duration_ms: out.plan.total_ms(),
    });
    emit_check(ctx, "Retail sample", check, true);
    Ok(())
}
