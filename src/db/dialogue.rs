//! Takes cut from a multi-speaker dialogue request: one row per segment,
//! sharing the request id and the `dialogue_group` hash.

use anyhow::Result;
use rusqlite::params;

use super::Db;
use crate::model::*;

/// What one dialogue request (or its cache hit) shares across its takes.
pub struct DialogueGroup<'a> {
    /// The group hash, stored as `dialogue_group`.
    pub group: &'a str,
    pub seed: u32,
    pub request_id: Option<&'a str>,
    /// The credits came from the `character-cost` header.
    pub credits_exact: bool,
}

/// One segment's take of a dialogue paragraph.
pub struct DialoguePart<'a> {
    pub segment_id: Id,
    pub audio_path: &'a str,
    pub hash: &'a str,
    /// This take's share of the request's credits (0 for a cache hit).
    pub credits_used: i64,
    /// Reuse this existing take row (the segment's own earlier take with the
    /// same hash) instead of inserting one.
    pub existing: Option<Id>,
    /// Trim to store on a new row (automatic trimming).
    pub trim_ms: Option<(u32, u32)>,
}

impl Db {
    /// Store a dialogue paragraph's takes in one transaction and make each
    /// its segment's current take. A segment approved in the meantime keeps
    /// the take it was approved with (the new take is still kept). Returns
    /// each part's take id and whether the row is new.
    pub fn store_dialogue_takes(
        &mut self,
        g: &DialogueGroup,
        parts: &[DialoguePart],
    ) -> Result<Vec<(Id, bool)>> {
        let tx = self.conn.transaction()?;
        let mut out = Vec::with_capacity(parts.len());
        for p in parts {
            let (id, new) = match p.existing {
                Some(id) => (id, false),
                None => {
                    tx.execute(
                        "INSERT INTO take (segment_id, seed, request_id, audio_path, hash, credits_used,
                                           credits_exact, trim_start_ms, trim_end_ms, dialogue_group)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            p.segment_id,
                            g.seed as i64,
                            g.request_id,
                            p.audio_path,
                            p.hash,
                            p.credits_used,
                            g.credits_exact as i64,
                            p.trim_ms.map(|t| t.0),
                            p.trim_ms.map(|t| t.1),
                            g.group,
                        ],
                    )?;
                    (tx.last_insert_rowid(), true)
                }
            };
            tx.execute(
                "UPDATE segment SET current_take_id = ?2, status = 'generated'
                 WHERE id = ?1 AND status != 'approved'",
                params![p.segment_id, id],
            )?;
            out.push((id, new));
        }
        tx.commit()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::super::{NewTake, SCHEMA};
    use super::*;

    fn library() -> (Db, Vec<Id>) {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db
            .import_chapter(pid, "One", "\"Who?\" asked the guard. \"Me.\"")
            .unwrap();
        let p = db.paragraphs(cid).unwrap()[0].clone();
        db.replace_paragraph_segments(
            p.id,
            &[
                (SegmentKind::Dialogue, "guard".into(), "\"Who?\"".into()),
                (
                    SegmentKind::Narration,
                    NARRATOR.into(),
                    "asked the guard.".into(),
                ),
                (SegmentKind::Dialogue, "vortram".into(), "\"Me.\"".into()),
            ],
        )
        .unwrap();
        let ids = db
            .chapter_segments(cid)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        (db, ids)
    }

    #[test]
    fn dialogue_takes_are_stored_together() {
        let (mut db, ids) = library();
        let g = DialogueGroup {
            group: "g1",
            seed: 1,
            request_id: Some("req-d"),
            credits_exact: true,
        };
        let parts: Vec<DialoguePart> = ids
            .iter()
            .enumerate()
            .map(|(i, &segment_id)| DialoguePart {
                segment_id,
                audio_path: ["a.wav", "b.wav", "c.wav"][i],
                hash: ["ha", "hb", "hc"][i],
                credits_used: [6, 16, 4][i],
                existing: None,
                trim_ms: (i == 0).then_some((90, 600)),
            })
            .collect();
        let stored = db.store_dialogue_takes(&g, &parts).unwrap();
        assert!(stored.iter().all(|&(_, new)| new));
        for (k, &sid) in ids.iter().enumerate() {
            let s = db.segment(sid).unwrap();
            assert_eq!(s.status, SegmentStatus::Generated);
            assert_eq!(s.current_take_id, Some(stored[k].0));
            let t = db.take(stored[k].0).unwrap();
            assert_eq!(t.dialogue_group.as_deref(), Some("g1"));
            assert_eq!(t.request_id.as_deref(), Some("req-d"));
            assert_eq!(t.seed, 1);
            assert!(t.credits_exact);
            assert_eq!(t.credits_used, [6, 16, 4][k]);
            assert_eq!(t.trim_ms, (k == 0).then_some((90, 600)));
        }
        // Single-voice takes have no group.
        let t = db
            .insert_take(&NewTake {
                segment_id: ids[0],
                seed: 7,
                request_id: None,
                audio_path: "x.wav",
                hash: "hx",
                credits_used: 6,
                credits_exact: false,
            })
            .unwrap();
        assert_eq!(db.take(t).unwrap().dialogue_group, None);

        // A reused row is not duplicated; an approved segment keeps its take.
        db.set_current_take(ids[2], t).unwrap();
        db.set_segment_status(ids[2], SegmentStatus::Approved)
            .unwrap();
        let again = vec![
            DialoguePart {
                segment_id: ids[0],
                audio_path: "a.wav",
                hash: "ha",
                credits_used: 0,
                existing: Some(stored[0].0),
                trim_ms: None,
            },
            DialoguePart {
                segment_id: ids[2],
                audio_path: "c.wav",
                hash: "hc",
                credits_used: 0,
                existing: None,
                trim_ms: None,
            },
        ];
        let res = db.store_dialogue_takes(&g, &again).unwrap();
        assert_eq!(res[0], (stored[0].0, false));
        assert!(res[1].1);
        assert_eq!(db.takes(ids[0]).unwrap().len(), 2);
        let s2 = db.segment(ids[2]).unwrap();
        assert_eq!(
            (s2.status, s2.current_take_id),
            (SegmentStatus::Approved, Some(t))
        );
        assert_eq!(db.takes(ids[2]).unwrap().len(), 2, "the new take is kept");
    }

    #[test]
    fn old_libraries_gain_the_dialogue_group_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&SCHEMA.replace("    dialogue_group TEXT,\n", ""))
                .unwrap();
            conn.execute_batch(
                "INSERT INTO project (name, language, tts_model, default_voice_settings) VALUES ('Old', 'en', 'm', '{}');
                 INSERT INTO chapter (project_id, ord, title, source_text, status) VALUES (1, 1, 'One', 'Text.', 'segmented');
                 INSERT INTO paragraph (chapter_id, ord, text) VALUES (1, 0, 'Text.');
                 INSERT INTO segment (paragraph_id, ord, kind, speaker, text, status) VALUES (1, 0, 'narration', 'narrator', 'Text.', 'generated');
                 INSERT INTO take (segment_id, seed, audio_path, hash) VALUES (1, 1, 'old.wav', 'h-old');",
            )
            .unwrap();
            let has: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('take') WHERE name = 'dialogue_group'")
                .unwrap()
                .exists([])
                .unwrap();
            assert!(!has);
        }
        let db = Db::open(&path).unwrap();
        let old = db.take_by_hash("h-old").unwrap().unwrap();
        assert_eq!(old.dialogue_group, None);
        assert_eq!(old.audio_path, "old.wav");
        drop(db);
        // Opening again is a no-op.
        let mut db = Db::open(&path).unwrap();
        let ids = [old.segment_id];
        let r = db
            .store_dialogue_takes(
                &DialogueGroup {
                    group: "g",
                    seed: 1,
                    request_id: None,
                    credits_exact: false,
                },
                &[DialoguePart {
                    segment_id: ids[0],
                    audio_path: "n.wav",
                    hash: "hn",
                    credits_used: 5,
                    existing: None,
                    trim_ms: None,
                }],
            )
            .unwrap();
        assert_eq!(
            db.take(r[0].0).unwrap().dialogue_group.as_deref(),
            Some("g")
        );
    }
}
