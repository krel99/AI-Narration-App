//! Retail export reports: the ACX check of each chapter's latest export
//! (`export_report`) and of the project's retail sample (`retail_sample`).

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};

use super::Db;
use crate::acx::{Check, FileReport, Levels};
use crate::model::Id;

/// New tables only, so `CREATE TABLE IF NOT EXISTS` is the whole migration.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS export_report (
    chapter_id     INTEGER PRIMARY KEY REFERENCES chapter(id) ON DELETE CASCADE,
    file           TEXT NOT NULL,
    draft          INTEGER NOT NULL DEFAULT 0,
    rms_db         REAL,
    peak_db        REAL,
    noise_floor_db REAL,
    head_ms        INTEGER NOT NULL DEFAULT 0,
    tail_ms        INTEGER NOT NULL DEFAULT 0,
    duration_ms    INTEGER NOT NULL,
    checks         TEXT NOT NULL DEFAULT '[]',
    passed         INTEGER NOT NULL,
    checked_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE TABLE IF NOT EXISTS retail_sample (
    project_id     INTEGER PRIMARY KEY REFERENCES project(id) ON DELETE CASCADE,
    chapter_id     INTEGER REFERENCES chapter(id) ON DELETE SET NULL,
    file           TEXT NOT NULL,
    rms_db         REAL,
    peak_db        REAL,
    noise_floor_db REAL,
    duration_ms    INTEGER NOT NULL,
    checks         TEXT NOT NULL DEFAULT '[]',
    passed         INTEGER NOT NULL,
    checked_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
"#;

/// -inf (silence) is stored as NULL.
fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

fn level(v: Option<f64>) -> f64 {
    v.unwrap_or(f64::NEG_INFINITY)
}

fn checks(json: &str) -> Vec<Check> {
    serde_json::from_str(json).unwrap_or_default()
}

const REPORT_COLS: &str = "chapter_id, file, draft, rms_db, peak_db, noise_floor_db, head_ms, tail_ms, duration_ms, checks, checked_at";

fn row_report(r: &Row) -> rusqlite::Result<(Id, FileReport)> {
    let json: String = r.get(9)?;
    Ok((
        r.get(0)?,
        FileReport {
            file: r.get(1)?,
            draft: r.get::<_, i64>(2)? != 0,
            levels: Levels {
                rms_db: level(r.get(3)?),
                peak_db: level(r.get(4)?),
                noise_floor_db: level(r.get(5)?),
                head_ms: r.get::<_, i64>(6)?.max(0) as u64,
                tail_ms: r.get::<_, i64>(7)?.max(0) as u64,
                duration_ms: r.get::<_, i64>(8)?.max(0) as u64,
            },
            checks: checks(&json),
            checked_at: r.get(10)?,
        },
    ))
}

impl Db {
    /// Replace the chapter's report with the check of its latest export.
    pub fn save_export_report(&self, chapter_id: Id, r: &FileReport) -> Result<()> {
        let l = &r.levels;
        self.conn.execute(
            "INSERT OR REPLACE INTO export_report (chapter_id, file, draft, rms_db, peak_db,
                    noise_floor_db, head_ms, tail_ms, duration_ms, checks, passed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                chapter_id,
                r.file,
                r.draft as i64,
                finite(l.rms_db),
                finite(l.peak_db),
                finite(l.noise_floor_db),
                l.head_ms as i64,
                l.tail_ms as i64,
                l.duration_ms as i64,
                serde_json::to_string(&r.checks)?,
                (r.verdict() == crate::acx::Verdict::Pass) as i64,
            ],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn export_report(&self, chapter_id: Id) -> Result<Option<FileReport>> {
        let sql = format!("SELECT {REPORT_COLS} FROM export_report WHERE chapter_id = ?1");
        Ok(self
            .conn
            .query_row(&sql, [chapter_id], row_report)
            .optional()?
            .map(|(_, r)| r))
    }

    /// Reports of every chapter of the project, by chapter id.
    pub fn export_reports(&self, project_id: Id) -> Result<HashMap<Id, FileReport>> {
        let sql = format!(
            "SELECT {REPORT_COLS} FROM export_report
             WHERE chapter_id IN (SELECT id FROM chapter WHERE project_id = ?1)"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([project_id], row_report)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_sample_report(
        &self,
        project_id: Id,
        chapter_id: Option<Id>,
        r: &FileReport,
    ) -> Result<()> {
        let l = &r.levels;
        self.conn.execute(
            "INSERT OR REPLACE INTO retail_sample (project_id, chapter_id, file, rms_db, peak_db,
                    noise_floor_db, duration_ms, checks, passed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                project_id,
                chapter_id,
                r.file,
                finite(l.rms_db),
                finite(l.peak_db),
                finite(l.noise_floor_db),
                l.duration_ms as i64,
                serde_json::to_string(&r.checks)?,
                (r.verdict() == crate::acx::Verdict::Pass) as i64,
            ],
        )?;
        Ok(())
    }

    /// The project's retail sample report and the chapter it came from.
    pub fn sample_report(&self, project_id: Id) -> Result<Option<(FileReport, Option<Id>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT chapter_id, file, rms_db, peak_db, noise_floor_db, duration_ms, checks,
                        checked_at
                 FROM retail_sample WHERE project_id = ?1",
                [project_id],
                |r| {
                    let json: String = r.get(6)?;
                    Ok((
                        FileReport {
                            file: r.get(1)?,
                            draft: false,
                            levels: Levels {
                                rms_db: level(r.get(2)?),
                                peak_db: level(r.get(3)?),
                                noise_floor_db: level(r.get(4)?),
                                duration_ms: r.get::<_, i64>(5)?.max(0) as u64,
                                head_ms: 0,
                                tail_ms: 0,
                            },
                            checks: checks(&json),
                            checked_at: r.get(7)?,
                        },
                        r.get(0)?,
                    ))
                },
            )
            .optional()?)
    }

    /// Chapters with takes created after their report was measured.
    pub fn stale_reports(&self, project_id: Id) -> Result<std::collections::HashSet<Id>> {
        let mut st = self.conn.prepare(
            "SELECT r.chapter_id FROM export_report r
             JOIN chapter c ON c.id = r.chapter_id
             WHERE c.project_id = ?1 AND EXISTS (
                SELECT 1 FROM take t JOIN segment s ON s.id = t.segment_id
                JOIN paragraph p ON p.id = s.paragraph_id
                WHERE p.chapter_id = r.chapter_id AND t.created_at > r.checked_at)",
        )?;
        let rows = st.query_map([project_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acx::{Container, FileKind, Verdict, evaluate};
    use crate::db::NewTake;

    fn report(rms: f64, floor: f64) -> FileReport {
        let levels = Levels {
            duration_ms: 61_000,
            rms_db: rms,
            peak_db: -4.2,
            noise_floor_db: floor,
            head_ms: 1050,
            tail_ms: 2100,
        };
        let c = Container {
            codec: "mp3".into(),
            sample_rate: 44100,
            channels: 1,
            bit_rate: Some(192_000),
        };
        FileReport {
            file: "01 - One.mp3".into(),
            draft: false,
            levels,
            checks: evaluate(FileKind::Chapter, &levels, Ok(&c), Some(true), 1_500_000),
            checked_at: String::new(),
        }
    }

    #[test]
    fn reports_round_trip_with_silence_as_null() {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db.import_chapter(pid, "One", "Text.").unwrap();
        let other = db.create_project("Other", "en").unwrap();
        let ocid = db.import_chapter(other, "X", "Y.").unwrap();
        assert!(db.export_report(cid).unwrap().is_none());

        let r = report(-20.3, f64::NEG_INFINITY);
        db.save_export_report(cid, &r).unwrap();
        db.save_export_report(ocid, &report(-19.0, -70.0)).unwrap();
        let back = db.export_report(cid).unwrap().unwrap();
        assert_eq!(back.levels, r.levels);
        assert_eq!(back.checks, r.checks);
        assert_eq!(back.verdict(), Verdict::Warn); // dead silence
        assert!(back.checked_at.ends_with('Z'));
        let all = db.export_reports(pid).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all.contains_key(&cid));

        // A newer export replaces the report.
        let mut d = report(-24.5, -72.0);
        d.draft = true;
        db.save_export_report(cid, &d).unwrap();
        let back = db.export_report(cid).unwrap().unwrap();
        assert!(back.draft);
        assert_eq!(back.levels.rms_db, -24.5);

        // Deleting the chapter drops its report.
        db.delete_chapter(cid).unwrap();
        assert!(db.export_reports(pid).unwrap().is_empty());
    }

    #[test]
    fn sample_report_and_staleness() {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db.import_chapter(pid, "One", "Text.").unwrap();
        assert!(db.sample_report(pid).unwrap().is_none());
        let mut r = report(-20.0, -73.0);
        r.file = "Book - Retail Sample.mp3".into();
        db.save_sample_report(pid, Some(cid), &r).unwrap();
        let (back, from) = db.sample_report(pid).unwrap().unwrap();
        assert_eq!(
            (back.file.as_str(), from),
            ("Book - Retail Sample.mp3", Some(cid))
        );
        assert_eq!(back.levels.rms_db, -20.0);

        db.save_export_report(cid, &report(-20.0, -73.0)).unwrap();
        assert!(db.stale_reports(pid).unwrap().is_empty());
        // A take recorded after the measurement makes the report stale.
        let p = db.paragraphs(cid).unwrap()[0].clone();
        db.replace_paragraph_segments(
            p.id,
            &[(
                crate::model::SegmentKind::Narration,
                "narrator".into(),
                "Text.".into(),
            )],
        )
        .unwrap();
        let seg = db.chapter_segments(cid).unwrap()[0].id;
        db.insert_take(&NewTake {
            segment_id: seg,
            seed: 1,
            request_id: None,
            audio_path: "a.wav",
            hash: "h",
            credits_used: 1,
            credits_exact: true,
        })
        .unwrap();
        db.conn
            .execute(
                "UPDATE export_report SET checked_at = '2000-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        assert!(db.stale_reports(pid).unwrap().contains(&cid));
        // Deleting the source chapter keeps the sample report.
        db.delete_chapter(cid).unwrap();
        assert_eq!(db.sample_report(pid).unwrap().unwrap().1, None);
    }

    #[test]
    fn old_libraries_gain_the_report_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite");
        {
            // A library from before the retail export.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(super::super::SCHEMA).unwrap();
            conn.execute_batch(
                "DROP TABLE IF EXISTS export_report; DROP TABLE IF EXISTS retail_sample;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        assert!(db.export_reports(pid).unwrap().is_empty());
        assert!(db.sample_report(pid).unwrap().is_none());
    }
}
