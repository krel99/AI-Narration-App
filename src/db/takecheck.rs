//! `take_check` queries: one check result per take (the Whisper transcript,
//! WER and findings). Deleting a take deletes its check (ON DELETE CASCADE).

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};

use super::{Db, from_json, json};
use crate::check::{CheckStatus, TakeCheck};
use crate::model::Id;

const CHECK_COLS: &str =
    "c.take_id, c.status, c.transcript, c.wer, c.issues, c.model, c.checked_at, c.text_key";

/// SQL for [`crate::check::text_key`] of segment `s`.
const SEGMENT_TEXT_KEY: &str = "(s.text || char(31) || COALESCE(s.performance, ''))";

/// Libraries created before checks recorded their text lack `text_key`
/// (their results count as made against the current text).
pub(super) fn init(conn: &rusqlite::Connection) -> Result<()> {
    let has: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('take_check') WHERE name = 'text_key'")?
        .exists([])?;
    if !has {
        conn.execute_batch("ALTER TABLE take_check ADD COLUMN text_key TEXT NOT NULL DEFAULT ''")?;
    }
    Ok(())
}

/// A check row whose columns start at index `o`.
fn row_check_at(r: &Row, o: usize) -> rusqlite::Result<TakeCheck> {
    let status: String = r.get(o + 1)?;
    let issues: String = r.get(o + 4)?;
    Ok(TakeCheck {
        take_id: r.get(o)?,
        status: CheckStatus::parse(&status).unwrap_or(CheckStatus::Error),
        transcript: r.get(o + 2)?,
        wer: r.get::<_, Option<f64>>(o + 3)?.map(|w| w as f32),
        issues: from_json(&issues),
        model: r.get(o + 5)?,
        checked_at: r.get(o + 6)?,
        text_key: r.get(o + 7)?,
    })
}

fn row_check(r: &Row) -> rusqlite::Result<TakeCheck> {
    row_check_at(r, 0)
}

impl Db {
    /// Store (or replace) a take's check result; `checked_at` is set to now.
    pub fn set_take_check(&self, c: &TakeCheck) -> Result<()> {
        self.conn.execute(
            "INSERT INTO take_check (take_id, status, transcript, wer, issues, model, text_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(take_id) DO UPDATE SET status = ?2, transcript = ?3, wer = ?4,
                 issues = ?5, model = ?6, text_key = ?7,
                 checked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            params![
                c.take_id,
                c.status.as_str(),
                c.transcript,
                c.wer.map(|w| w as f64),
                json(&c.issues),
                c.model,
                c.text_key
            ],
        )?;
        Ok(())
    }

    pub fn take_check(&self, take_id: Id) -> Result<Option<TakeCheck>> {
        let sql = format!("SELECT {CHECK_COLS} FROM take_check c WHERE c.take_id = ?1");
        Ok(self.conn.query_row(&sql, [take_id], row_check).optional()?)
    }

    /// Check results of the chapter's CURRENT takes, by segment id. A result
    /// made against another text (the segment was merged, split or re-tagged
    /// since, and an older take picked) is left out: it says nothing about
    /// the current text, and Check chapter redoes it.
    pub fn chapter_checks(&self, chapter_id: Id) -> Result<HashMap<Id, TakeCheck>> {
        let sql = format!(
            "SELECT s.id, {CHECK_COLS} FROM take_check c
             JOIN segment s ON s.current_take_id = c.take_id
             JOIN paragraph p ON p.id = s.paragraph_id WHERE p.chapter_id = ?1
               AND (c.text_key = '' OR c.text_key = {SEGMENT_TEXT_KEY})"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([chapter_id], |r| {
            Ok((r.get::<_, Id>(0)?, row_check_at(r, 1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The chapter's current takes that need a check when the setup
    /// transcribes with `model` (empty: length only), in reading order.
    /// Same rule as [`crate::check::needs_check`].
    pub fn takes_to_check(&self, chapter_id: Id, model: &str) -> Result<Vec<Id>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT s.current_take_id FROM segment s
             JOIN paragraph p ON p.id = s.paragraph_id
             LEFT JOIN take_check c ON c.take_id = s.current_take_id
             WHERE p.chapter_id = ?1 AND s.current_take_id IS NOT NULL
               AND (c.take_id IS NULL OR c.status = 'error'
                    OR (c.text_key != '' AND c.text_key != {SEGMENT_TEXT_KEY})
                    OR (?2 != '' AND c.status != 'dismissed' AND c.model != ?2))
             ORDER BY p.ord, s.ord"
        ))?;
        Ok(st
            .query_map(params![chapter_id, model], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Mark a flagged take as fine. Returns false when it was not flagged.
    pub fn dismiss_take_check(&self, take_id: Id) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE take_check SET status = 'dismissed' WHERE take_id = ?1 AND status = 'flagged'",
            [take_id],
        )? > 0)
    }

    /// A finished check by `model` of another take with the same hash, i.e.
    /// the same audio file and the same spoken text (a cache hit), made
    /// against the text `key`.
    pub fn shared_take_check(
        &self,
        take_id: Id,
        model: &str,
        key: &str,
    ) -> Result<Option<TakeCheck>> {
        let sql = format!(
            "SELECT {CHECK_COLS} FROM take t
             JOIN take o ON o.hash = t.hash AND o.id != t.id
             JOIN take_check c ON c.take_id = o.id
             WHERE t.id = ?1 AND c.model = ?2 AND c.status != 'error'
               AND (c.text_key = '' OR c.text_key = ?3)
             ORDER BY c.checked_at DESC, c.take_id DESC LIMIT 1"
        );
        Ok(self
            .conn
            .query_row(&sql, params![take_id, model, key], row_check)
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::super::{Library, NewTake, SCHEMA};
    use super::*;
    use crate::check::{Issue, IssueKind, needs_check};
    use crate::model::*;

    fn result(take_id: Id, status: CheckStatus, model: &str) -> TakeCheck {
        TakeCheck {
            take_id,
            status,
            transcript: Some("It is.".into()),
            wer: Some(0.25),
            issues: vec![Issue {
                kind: IssueKind::Words,
                flag: status == CheckStatus::Flagged,
                message: "Minor differences.".into(),
            }],
            model: model.into(),
            checked_at: String::new(),
            text_key: String::new(),
        }
    }

    /// A chapter with three segments; the first two have current takes, and
    /// the first also has an older take.
    fn setup(db: &mut Db) -> (Id, Vec<Id>, Vec<Id>) {
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db.import_chapter(pid, "One", "A.\n\nB.\n\nC.").unwrap();
        for p in db.paragraphs(cid).unwrap() {
            db.replace_paragraph_segments(
                p.id,
                &[(SegmentKind::Narration, NARRATOR.into(), p.text.clone())],
            )
            .unwrap();
        }
        let segs: Vec<Id> = db
            .chapter_segments(cid)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        let take = |seg: Id, hash: &str| {
            db.insert_take(&NewTake {
                segment_id: seg,
                seed: 1,
                request_id: None,
                audio_path: &format!("{hash}.wav"),
                hash,
                credits_used: 2,
                credits_exact: true,
            })
            .unwrap()
        };
        let old = take(segs[0], "h-old");
        let t0 = take(segs[0], "h0");
        let t1 = take(segs[1], "h1");
        db.set_current_take(segs[0], t0).unwrap();
        db.set_current_take(segs[1], t1).unwrap();
        (cid, segs, vec![old, t0, t1])
    }

    #[test]
    fn results_roundtrip_and_follow_current_takes() {
        let mut db = Db::open_in_memory().unwrap();
        let (cid, segs, takes) = setup(&mut db);
        assert_eq!(db.take_check(takes[1]).unwrap(), None);
        db.set_take_check(&result(takes[0], CheckStatus::Flagged, "small"))
            .unwrap();
        db.set_take_check(&result(takes[1], CheckStatus::Ok, "small"))
            .unwrap();
        let got = db.take_check(takes[1]).unwrap().unwrap();
        assert_eq!(got.status, CheckStatus::Ok);
        assert_eq!(got.issues[0].kind, IssueKind::Words);
        assert_eq!(got.wer, Some(0.25));
        assert!(!got.checked_at.is_empty());
        // Only current takes count for the chapter view.
        let by_seg = db.chapter_checks(cid).unwrap();
        assert_eq!(by_seg.len(), 1);
        assert_eq!(by_seg[&segs[0]].take_id, takes[1]);
        // Replacing keeps one row per take.
        db.set_take_check(&result(takes[1], CheckStatus::Flagged, "base"))
            .unwrap();
        let got = db.take_check(takes[1]).unwrap().unwrap();
        assert_eq!(
            (got.status, got.model.as_str()),
            (CheckStatus::Flagged, "base")
        );
        // Dismissing only applies to flags.
        assert!(db.dismiss_take_check(takes[1]).unwrap());
        assert!(!db.dismiss_take_check(takes[1]).unwrap());
        assert_eq!(
            db.take_check(takes[1]).unwrap().unwrap().status,
            CheckStatus::Dismissed
        );
        assert!(!db.dismiss_take_check(takes[2]).unwrap(), "no result yet");
    }

    #[test]
    fn takes_to_check_matches_needs_check() {
        let mut db = Db::open_in_memory().unwrap();
        let (cid, _segs, takes) = setup(&mut db);
        let (t0, t1) = (takes[1], takes[2]);
        // Nothing checked yet: both current takes, in reading order.
        assert_eq!(db.takes_to_check(cid, "small").unwrap(), vec![t0, t1]);
        assert_eq!(db.takes_to_check(cid, "").unwrap(), vec![t0, t1]);
        let cases = [
            (CheckStatus::Ok, "small"),
            (CheckStatus::Ok, "base"),
            (CheckStatus::Skipped, ""),
            (CheckStatus::Error, "small"),
            (CheckStatus::Dismissed, "base"),
            (CheckStatus::Flagged, "base"),
        ];
        for (status, model) in cases {
            let r = result(t0, status, model);
            db.set_take_check(&r).unwrap();
            for current in ["small", ""] {
                let sql = db.takes_to_check(cid, current).unwrap().contains(&t0);
                assert_eq!(
                    sql,
                    needs_check(Some(&r), current, &crate::check::text_key("A.", None)),
                    "{status:?} by {model:?}, now {current:?}"
                );
            }
        }
        assert!(needs_check(None, "", ""));
    }

    /// A check made against an earlier text (the segment was merged, and an
    /// older take picked again) neither shows nor counts, and is redone.
    #[test]
    fn checks_against_another_text_are_redone() {
        let mut db = Db::open_in_memory().unwrap();
        let (cid, segs, takes) = setup(&mut db);
        let t0 = takes[1];
        let key = crate::check::text_key;
        for status in [CheckStatus::Ok, CheckStatus::Dismissed] {
            db.set_take_check(&TakeCheck {
                text_key: key("A.", None),
                ..result(t0, status, "small")
            })
            .unwrap();
            assert!(db.chapter_checks(cid).unwrap().contains_key(&segs[0]));
            assert!(!db.takes_to_check(cid, "small").unwrap().contains(&t0));
        }
        // The segment's text or tags change; the old take is current again.
        db.set_segment_performance(segs[0], Some("[sighs] A."))
            .unwrap();
        db.set_current_take(segs[0], t0).unwrap();
        let stored = db.take_check(t0).unwrap().unwrap();
        let now = key("A.", Some("[sighs] A."));
        assert!(needs_check(Some(&stored), "small", &now));
        assert!(needs_check(Some(&stored), "", &now), "even length-only");
        assert!(!db.chapter_checks(cid).unwrap().contains_key(&segs[0]));
        assert!(db.takes_to_check(cid, "small").unwrap().contains(&t0));
        assert!(db.takes_to_check(cid, "").unwrap().contains(&t0));
        // Results from before the text was recorded count as current.
        db.set_take_check(&result(t0, CheckStatus::Ok, "small"))
            .unwrap();
        assert!(db.chapter_checks(cid).unwrap().contains_key(&segs[0]));
        assert!(!db.takes_to_check(cid, "small").unwrap().contains(&t0));
    }

    #[test]
    fn cache_hits_share_a_check_and_deletes_cascade() {
        let mut db = Db::open_in_memory().unwrap();
        let (cid, segs, takes) = setup(&mut db);
        // A cache hit: another take row with the same hash.
        let hit = db
            .insert_take(&NewTake {
                segment_id: segs[2],
                seed: 1,
                request_id: None,
                audio_path: "h0.wav",
                hash: "h0",
                credits_used: 0,
                credits_exact: true,
            })
            .unwrap();
        let k = crate::check::text_key("C.", None);
        assert_eq!(db.shared_take_check(hit, "small", &k).unwrap(), None);
        db.set_take_check(&result(takes[1], CheckStatus::Error, "small"))
            .unwrap();
        assert_eq!(
            db.shared_take_check(hit, "small", &k).unwrap(),
            None,
            "failed checks are not shared"
        );
        db.set_take_check(&result(takes[1], CheckStatus::Flagged, "small"))
            .unwrap();
        assert_eq!(
            db.shared_take_check(hit, "small", &k)
                .unwrap()
                .unwrap()
                .take_id,
            takes[1]
        );
        assert_eq!(db.shared_take_check(hit, "base", &k).unwrap(), None);
        assert_eq!(
            db.shared_take_check(takes[1], "small", &k).unwrap(),
            None,
            "not itself"
        );
        // A check made against another text is not shared.
        db.set_take_check(&TakeCheck {
            text_key: crate::check::text_key("A.", None),
            ..result(takes[1], CheckStatus::Flagged, "small")
        })
        .unwrap();
        assert_eq!(db.shared_take_check(hit, "small", &k).unwrap(), None);
        // Deleting the chapter deletes its takes and their checks.
        db.delete_chapter(cid).unwrap();
        assert_eq!(db.take_check(takes[1]).unwrap(), None);
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM take_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn old_libraries_gain_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        std::fs::create_dir_all(&lib.root).unwrap();
        {
            let conn = Connection::open(lib.db_path()).unwrap();
            let start = SCHEMA
                .find("CREATE TABLE IF NOT EXISTS take_check")
                .unwrap();
            let end = start + SCHEMA[start..].find(");\n").unwrap() + 3;
            conn.execute_batch(&format!("{}{}", &SCHEMA[..start], &SCHEMA[end..]))
                .unwrap();
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'take_check'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0);
        }
        let mut db = lib.open().unwrap();
        let (_, _, takes) = setup(&mut db);
        db.set_take_check(&result(takes[1], CheckStatus::Ok, "small"))
            .unwrap();
        drop(db);
        assert!(lib.open().unwrap().take_check(takes[1]).unwrap().is_some());
    }

    #[test]
    fn old_check_tables_gain_the_text_key() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        std::fs::create_dir_all(&lib.root).unwrap();
        let old = SCHEMA.replace(
                "    checked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),\n    text_key   TEXT NOT NULL DEFAULT ''\n",
                "    checked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))\n",
            );
        assert_ne!(old, SCHEMA, "the old schema has no text_key");
        Connection::open(lib.db_path())
            .unwrap()
            .execute_batch(&old)
            .unwrap();
        let mut db = lib.open().unwrap();
        let (cid, segs, takes) = setup(&mut db);
        db.set_take_check(&result(takes[1], CheckStatus::Ok, "small"))
            .unwrap();
        assert_eq!(db.take_check(takes[1]).unwrap().unwrap().text_key, "");
        assert!(db.chapter_checks(cid).unwrap().contains_key(&segs[0]));
        drop(db);
        lib.open().unwrap();
    }
}
