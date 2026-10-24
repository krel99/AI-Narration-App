//! Export jobs with their ACX checks: the report is stored, `ACX report.md`
//! is written, and the status line names the result.

use super::*;
use crate::acx::Verdict;
use crate::retail::fixture::{self, approved_book, lighthouse};

fn events(rx: &Receiver<Event>) -> Vec<Event> {
    rx.try_iter().collect()
}

#[test]
fn chapter_export_is_checked_and_reported() {
    if !fixture::have_ffmpeg() {
        eprintln!("ffmpeg/ffprobe not found; skipping");
        return;
    }
    let book = approved_book(lighthouse());
    let (ctx, rx) = test_ctx(&book.lib, "http://127.0.0.1:9", "http://127.0.0.1:9");
    let rt = rt();
    let cid = book.chapter.id;
    rt.block_on(run(
        &ctx,
        Command::ExportChapter {
            chapter_id: cid,
            draft: false,
        },
    ))
    .unwrap();
    let ev = events(&rx);
    let finished = ev
        .iter()
        .position(|e| matches!(e, Event::ExportFinished { .. }))
        .expect("export finished");
    let checked = ev
        .iter()
        .position(|e| matches!(e, Event::AcxChecked { .. }))
        .expect("acx checked");
    // The check comes last, so it is what the status bar shows.
    assert!(checked > finished);
    match &ev[checked] {
        Event::AcxChecked { summary, verdict } => {
            assert!(summary.starts_with("Opening: ACX"), "{summary}");
            // No cover set: only the book-level line warns, not the file.
            assert_eq!(*verdict, Verdict::Pass, "{summary}");
        }
        _ => unreachable!(),
    }
    let report = ctx.db(|db| db.export_report(cid)).unwrap().unwrap();
    assert_eq!(report.file, "01 - Opening.mp3");
    assert!(!report.draft);
    assert!(
        report
            .check("cbr")
            .is_some_and(|c| c.verdict == Verdict::Pass)
    );
    let project = ctx.db(|db| db.project(book.project.id)).unwrap();
    let dir = crate::export::export_dir(&book.lib, &project).unwrap();
    let md = std::fs::read_to_string(dir.join(crate::retail::REPORT_FILE)).unwrap();
    assert!(md.starts_with("# ACX report: The Lighthouse"), "{md}");
    assert!(md.contains("| 1 | Opening | 01 - Opening.mp3 |"), "{md}");
    assert!(md.contains("**Cover**: none"));
    assert!(md.contains("No retail sample exported yet."));

    // The report remembers what the export was made of.
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    assert_eq!(report.takes_key, Some(crate::retail::segments_key(&segs)));
    let pid = book.project.id;
    assert!(ctx.db(|db| db.stale_reports(pid)).unwrap().is_empty());

    // A draft export afterwards is checked and announced, but the report
    // keeps describing the final file next to it.
    rt.block_on(run(
        &ctx,
        Command::ExportChapter {
            chapter_id: cid,
            draft: true,
        },
    ))
    .unwrap();
    let summary = events(&rx)
        .into_iter()
        .find_map(|e| match e {
            Event::AcxChecked { summary, .. } => Some(summary),
            _ => None,
        })
        .unwrap();
    assert!(
        summary.contains("the report keeps the final export"),
        "{summary}"
    );
    let report = ctx.db(|db| db.export_report(cid)).unwrap().unwrap();
    assert_eq!(
        (report.file.as_str(), report.draft),
        ("01 - Opening.mp3", false)
    );
    let md = std::fs::read_to_string(dir.join(crate::retail::REPORT_FILE)).unwrap();
    assert!(!md.contains("(draft).mp3"), "{md}");

    // The retail sample: stored separately, added to the report.
    rt.block_on(run(
        &ctx,
        Command::ExportSample {
            chapter_id: cid,
            start_segment: None,
            max_ms: 120_000,
        },
    ))
    .unwrap();
    let ev = events(&rx);
    let files = ev.iter().find_map(|e| match e {
        Event::ExportFinished { files, .. } => Some(files.clone()),
        _ => None,
    });
    assert_eq!(
        files.unwrap()[0].file_name().unwrap(),
        "The Lighthouse - Retail Sample.mp3"
    );
    let summary = ev
        .iter()
        .find_map(|e| match e {
            Event::AcxChecked { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .unwrap();
    // Eight seconds of audio: shorter than a minute is a warning.
    assert!(summary.starts_with("Retail sample: ACX ⚠"), "{summary}");
    assert!(summary.contains("under a minute"), "{summary}");
    let (sample, from) = ctx
        .db(|db| db.sample_report(book.project.id))
        .unwrap()
        .unwrap();
    assert_eq!(from, Some(cid));
    assert!(sample.check("head").is_none());
    let md = std::fs::read_to_string(dir.join(crate::retail::REPORT_FILE)).unwrap();
    assert!(
        md.contains("The Lighthouse - Retail Sample.mp3: 0:00:"),
        "{md}"
    );

    // A sample that would start on an unapproved segment is refused up
    // front, and nothing is written.
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    ctx.db(|db| db.set_segment_status(segs[2].id, SegmentStatus::Generated))
        .unwrap();
    // Unapproving a segment makes the chapter's report outdated.
    assert!(ctx.db(|db| db.stale_reports(pid)).unwrap().contains(&cid));
    let _ = events(&rx);
    rt.block_on(run(
        &ctx,
        Command::ExportSample {
            chapter_id: cid,
            start_segment: Some(segs[2].id),
            max_ms: 120_000,
        },
    ))
    .unwrap();
    // The failure comes as ExportFailed (which ends the UI's export state),
    // not as a generic Error.
    let ev = events(&rx);
    let failed: Vec<&String> = ev
        .iter()
        .filter_map(|e| match e {
            Event::ExportFailed(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 1, "{ev:?}");
    assert!(failed[0].contains("not approved"), "{}", failed[0]);
    assert!(!ev.iter().any(|e| matches!(e, Event::Error(_))), "{ev:?}");
}

/// Only an export's own failure ends the UI's export state: other jobs
/// report theirs as Error.
#[test]
fn export_failures_have_their_own_event() {
    let book = approved_book(lighthouse());
    let (ctx, rx) = test_ctx(&book.lib, "http://127.0.0.1:9", "http://127.0.0.1:9");
    let rt = rt();
    let cid = book.chapter.id;
    let segs = ctx.db(|db| db.chapter_segments(cid)).unwrap();
    ctx.db(|db| db.set_segment_status(segs[0].id, SegmentStatus::Generated))
        .unwrap();
    // A final export of a chapter that is not fully approved is refused.
    rt.block_on(run(
        &ctx,
        Command::ExportChapter {
            chapter_id: cid,
            draft: false,
        },
    ))
    .unwrap();
    let ev = events(&rx);
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::ExportFailed(m) if m.contains("approved"))),
        "{ev:?}"
    );
    assert!(!ev.iter().any(|e| matches!(e, Event::Error(_))), "{ev:?}");
    // An unrelated failure stays an Err (reported as Event::Error by the
    // command loop) and never as ExportFailed.
    let r = rt.block_on(run(&ctx, Command::AutoTrimChapter { chapter_id: 9999 }));
    let ev = events(&rx);
    assert!(
        r.is_err() || ev.iter().any(|e| matches!(e, Event::Error(_))),
        "{r:?} {ev:?}"
    );
    assert!(
        !ev.iter().any(|e| matches!(e, Event::ExportFailed(_))),
        "{ev:?}"
    );
}
