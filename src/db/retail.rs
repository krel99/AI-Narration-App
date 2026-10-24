//! Retail export reports: the ACX check of each chapter's latest export
//! (`export_report`) and of the project's retail sample (`retail_sample`).

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};

use super::Db;
use crate::acx::{Check, FileReport, Levels};
use crate::model::Id;

/// New tables, plus the `takes_key` column for tables made before it.
pub(super) fn init(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    let has_key: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('export_report') WHERE name = 'takes_key'")?
        .exists([])?;
    if !has_key {
        conn.execute_batch("ALTER TABLE export_report ADD COLUMN takes_key TEXT")?;
    }
    Ok(())
}

const SCHEMA: &str = r#"
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
    checked_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    takes_key      TEXT
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

const REPORT_COLS: &str = "chapter_id, file, draft, rms_db, peak_db, noise_floor_db, head_ms, tail_ms, duration_ms, checks, checked_at, takes_key";

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
            takes_key: r.get(11)?,
        },
    ))
}

impl Db {
    /// Store the check of the chapter's latest export. A final export
    /// replaces any report; a draft only replaces a draft (or nothing), so
    /// the report keeps describing the delivery file next to it. Returns
    /// whether it was stored.
    pub fn save_export_report(&self, chapter_id: Id, r: &FileReport) -> Result<bool> {
        let l = &r.levels;
        let n = self.conn.execute(
            "INSERT INTO export_report (chapter_id, file, draft, rms_db, peak_db,
                    noise_floor_db, head_ms, tail_ms, duration_ms, checks, passed, takes_key,
                    checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
             ON CONFLICT(chapter_id) DO UPDATE SET
                file = excluded.file, draft = excluded.draft, rms_db = excluded.rms_db,
                peak_db = excluded.peak_db, noise_floor_db = excluded.noise_floor_db,
                head_ms = excluded.head_ms, tail_ms = excluded.tail_ms,
                duration_ms = excluded.duration_ms, checks = excluded.checks,
                passed = excluded.passed, takes_key = excluded.takes_key,
                checked_at = excluded.checked_at
             WHERE excluded.draft = 0 OR export_report.draft = 1",
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
                r.takes_key,
            ],
        )?;
        Ok(n > 0)
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
                            takes_key: None,
                        },
                        r.get(0)?,
                    ))
                },
            )
            .optional()?)
    }

    /// `retail::takes_key` of every chapter of the project that has
    /// segments, in one query (the same order as `Db::chapter_segments`).
    pub fn takes_keys(&self, project_id: Id) -> Result<HashMap<Id, String>> {
        let mut st = self.conn.prepare(
            "SELECT p.chapter_id, s.id, s.current_take_id, s.status = 'approved',
                    t.trim_start_ms, t.trim_end_ms
             FROM segment s JOIN paragraph p ON p.id = s.paragraph_id
             JOIN chapter c ON c.id = p.chapter_id
             LEFT JOIN take t ON t.id = s.current_take_id
             WHERE c.project_id = ?1
             ORDER BY p.chapter_id, p.ord, s.ord",
        )?;
        let mut by_chapter: HashMap<Id, Vec<crate::retail::KeyPart>> = HashMap::new();
        for row in st.query_map([project_id], |r| {
            let trim = match (r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?) {
                (Some(a), Some(b)) => Some((a.max(0) as u32, b.max(0) as u32)),
                _ => None,
            };
            Ok((r.get::<_, Id>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, trim)))
        })? {
            let (chapter_id, seg) = row?;
            by_chapter.entry(chapter_id).or_default().push(seg);
        }
        Ok(by_chapter
            .into_iter()
            .map(|(id, segs)| (id, crate::retail::takes_key(segs)))
            .collect())
    }

    /// Silence trims of the chapter's takes that have one, by take id (for
    /// `retail::segments_key`).
    pub fn chapter_trims(&self, chapter_id: Id) -> Result<HashMap<Id, (u32, u32)>> {
        let mut st = self.conn.prepare(
            "SELECT t.id, t.trim_start_ms, t.trim_end_ms FROM take t
             JOIN segment s ON s.id = t.segment_id
             JOIN paragraph p ON p.id = s.paragraph_id
             WHERE p.chapter_id = ?1 AND t.trim_start_ms IS NOT NULL AND t.trim_end_ms IS NOT NULL",
        )?;
        let rows = st.query_map([chapter_id], |r| {
            Ok((
                r.get::<_, Id>(0)?,
                (
                    r.get::<_, i64>(1)?.max(0) as u32,
                    r.get::<_, i64>(2)?.max(0) as u32,
                ),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Chapters whose segments, current takes, approvals or trims no longer match
    /// their report (`retail::takes_key`). Reports stored before the key
    /// existed fall back to "a take was created after the measurement".
    pub fn stale_reports(&self, project_id: Id) -> Result<std::collections::HashSet<Id>> {
        let mut stale = std::collections::HashSet::new();
        let mut st = self.conn.prepare(
            "SELECT r.chapter_id, r.takes_key FROM export_report r
             JOIN chapter c ON c.id = r.chapter_id
             WHERE c.project_id = ?1 AND r.takes_key IS NOT NULL",
        )?;
        let stored: Vec<(Id, String)> = st
            .query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if !stored.is_empty() {
            let current = self.takes_keys(project_id)?;
            for (chapter_id, key) in stored {
                let now = current
                    .get(&chapter_id)
                    .cloned()
                    .unwrap_or_else(|| crate::retail::takes_key([]));
                if key != now {
                    stale.insert(chapter_id);
                }
            }
        }
        let mut st = self.conn.prepare(
            "SELECT r.chapter_id FROM export_report r
             JOIN chapter c ON c.id = r.chapter_id
             WHERE c.project_id = ?1 AND r.takes_key IS NULL AND EXISTS (
                SELECT 1 FROM take t JOIN segment s ON s.id = t.segment_id
                JOIN paragraph p ON p.id = s.paragraph_id
                WHERE p.chapter_id = r.chapter_id AND t.created_at > r.checked_at)",
        )?;
        for id in st.query_map([project_id], |r| r.get(0))? {
            stale.insert(id?);
        }
        Ok(stale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acx::{Container, FileKind, Verdict, evaluate};
    use crate::db::NewTake;
    use crate::model::SegmentStatus;

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
            takes_key: None,
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

        // A draft export does not replace the final export's report (the
        // report describes the delivery file)...
        let mut d = report(-24.5, -72.0);
        d.draft = true;
        assert!(!db.save_export_report(cid, &d).unwrap());
        let back = db.export_report(cid).unwrap().unwrap();
        assert!(!back.draft);
        assert_eq!(back.levels.rms_db, -20.3);
        // ...but a draft stands in while there is no final export, a newer
        // draft replaces it, and a final export replaces any draft.
        let other_cid = db.import_chapter(pid, "Two", "More.").unwrap();
        assert!(db.save_export_report(other_cid, &d).unwrap());
        let mut d2 = report(-21.0, -72.0);
        d2.draft = true;
        assert!(db.save_export_report(other_cid, &d2).unwrap());
        assert_eq!(
            db.export_report(other_cid).unwrap().unwrap().levels.rms_db,
            -21.0
        );
        let mut f = report(-19.5, -72.0);
        f.takes_key = Some("abc".into());
        assert!(db.save_export_report(other_cid, &f).unwrap());
        let back = db.export_report(other_cid).unwrap().unwrap();
        assert!(!back.draft);
        assert_eq!(back.takes_key.as_deref(), Some("abc"));
        assert!(db.save_export_report(cid, &report(-19.0, -70.0)).unwrap());
        assert_eq!(db.export_report(cid).unwrap().unwrap().levels.rms_db, -19.0);
        db.delete_chapter(other_cid).unwrap();

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

    /// With a takes key, "outdated" follows what the export is made of:
    /// approvals and the current take, not only new takes.
    #[test]
    fn approvals_and_take_switches_make_a_report_outdated() {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db.import_chapter(pid, "One", "Text.").unwrap();
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
        let take = |db: &Db, h: &str| {
            db.insert_take(&NewTake {
                segment_id: seg,
                seed: 1,
                request_id: None,
                audio_path: "a.wav",
                hash: h,
                credits_used: 1,
                credits_exact: true,
            })
            .unwrap()
        };
        let t1 = take(&db, "h1");
        let t2 = take(&db, "h2");
        db.set_current_take(seg, t2).unwrap();
        db.set_segment_status(seg, SegmentStatus::Approved).unwrap();
        let key = |db: &Db| {
            crate::retail::segments_key(
                &db.chapter_segments(cid).unwrap(),
                &db.chapter_trims(cid).unwrap(),
            )
        };
        // The one-query keys agree with the per-chapter ones.
        assert_eq!(db.takes_keys(pid).unwrap().get(&cid), Some(&key(&db)));
        let mut r = report(-20.0, -73.0);
        r.takes_key = Some(key(&db));
        db.save_export_report(cid, &r).unwrap();
        assert!(db.stale_reports(pid).unwrap().is_empty());

        // Unapproving changes the key; approving again restores it.
        db.set_segment_status(seg, SegmentStatus::Generated)
            .unwrap();
        assert!(db.stale_reports(pid).unwrap().contains(&cid));
        db.set_segment_status(seg, SegmentStatus::Approved).unwrap();
        assert!(db.stale_reports(pid).unwrap().is_empty());
        // Picking the older take again (no new take is created).
        db.set_current_take(seg, t1).unwrap();
        assert!(db.stale_reports(pid).unwrap().contains(&cid));
        db.set_current_take(seg, t2).unwrap();
        assert!(db.stale_reports(pid).unwrap().is_empty());
        // A silence trim of the current take changes the exported audio.
        db.set_take_trim(t2, Some((40, 900))).unwrap();
        assert!(db.stale_reports(pid).unwrap().contains(&cid));
        assert_eq!(db.takes_keys(pid).unwrap().get(&cid), Some(&key(&db)));
        db.set_take_trim(t2, None).unwrap();
        assert!(db.stale_reports(pid).unwrap().is_empty());
        // A trim of a take that is not current changes nothing.
        db.set_take_trim(t1, Some((40, 900))).unwrap();
        assert!(db.stale_reports(pid).unwrap().is_empty());
        // A take created later but not made current changes nothing.
        db.conn
            .execute(
                "UPDATE export_report SET checked_at = '2000-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        take(&db, "h3");
        assert!(db.stale_reports(pid).unwrap().is_empty());
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

    #[test]
    fn report_tables_without_the_takes_key_gain_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite");
        let cid;
        {
            // export_report as the first retail build created it.
            let mut db = Db::open(&path).unwrap();
            let pid = db.create_project("Book", "en").unwrap();
            cid = db.import_chapter(pid, "One", "Text.").unwrap();
            db.conn
                .execute_batch(&format!(
                    "DROP TABLE export_report;
                     CREATE TABLE export_report (
                        chapter_id INTEGER PRIMARY KEY REFERENCES chapter(id) ON DELETE CASCADE,
                        file TEXT NOT NULL, draft INTEGER NOT NULL DEFAULT 0, rms_db REAL,
                        peak_db REAL, noise_floor_db REAL, head_ms INTEGER NOT NULL DEFAULT 0,
                        tail_ms INTEGER NOT NULL DEFAULT 0, duration_ms INTEGER NOT NULL,
                        checks TEXT NOT NULL DEFAULT '[]', passed INTEGER NOT NULL,
                        checked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')));
                     INSERT INTO export_report (chapter_id, file, duration_ms, passed)
                        VALUES ({cid}, 'old.mp3', 1000, 1);"
                ))
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let old = db.export_report(cid).unwrap().unwrap();
        assert_eq!((old.file.as_str(), old.takes_key), ("old.mp3", None));
        let mut r = report(-20.0, -73.0);
        r.takes_key = Some("k".into());
        db.save_export_report(cid, &r).unwrap();
        assert_eq!(
            db.export_report(cid).unwrap().unwrap().takes_key.as_deref(),
            Some("k")
        );
        // Opening again is a no-op.
        drop(db);
        Db::open(&path).unwrap();
    }
}
