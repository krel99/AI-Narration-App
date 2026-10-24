//! SQLite state. One library database holds every project; audio lives next
//! to it in `audio/<take_hash>.wav`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::*;
use crate::segment::{self, ParsedParagraph};

mod costs;
mod retail;
pub use costs::PassUsage;
#[cfg(test)]
pub use costs::UsageRow;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS project (
    id                     INTEGER PRIMARY KEY,
    name                   TEXT NOT NULL,
    language               TEXT NOT NULL,
    tts_model              TEXT NOT NULL,
    voice_map              TEXT NOT NULL DEFAULT '{}',
    default_voice_settings TEXT NOT NULL,
    settings               TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS character (
    id           INTEGER PRIMARY KEY,
    project_id   INTEGER NOT NULL REFERENCES project(id) ON DELETE CASCADE,
    key          TEXT NOT NULL,
    display_name TEXT NOT NULL,
    aliases      TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, key)
);
CREATE TABLE IF NOT EXISTS chapter (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES project(id) ON DELETE CASCADE,
    ord         INTEGER NOT NULL,
    title       TEXT NOT NULL,
    source_text TEXT NOT NULL,
    status      TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS paragraph (
    id                 INTEGER PRIMARY KEY,
    chapter_id         INTEGER NOT NULL REFERENCES chapter(id) ON DELETE CASCADE,
    ord                INTEGER NOT NULL,
    text               TEXT NOT NULL,
    scene_break_before INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS segment (
    id                INTEGER PRIMARY KEY,
    paragraph_id      INTEGER NOT NULL REFERENCES paragraph(id) ON DELETE CASCADE,
    ord               INTEGER NOT NULL,
    kind              TEXT NOT NULL,
    speaker           TEXT NOT NULL,
    text              TEXT NOT NULL,
    settings_override TEXT,
    performance       TEXT,
    status            TEXT NOT NULL,
    current_take_id   INTEGER
);
CREATE TABLE IF NOT EXISTS take (
    id           INTEGER PRIMARY KEY,
    segment_id   INTEGER NOT NULL REFERENCES segment(id) ON DELETE CASCADE,
    seed         INTEGER NOT NULL,
    request_id   TEXT,
    audio_path   TEXT NOT NULL,
    hash         TEXT NOT NULL,
    credits_used INTEGER NOT NULL DEFAULT 0,
    credits_exact INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX IF NOT EXISTS take_hash ON take(hash);
CREATE INDEX IF NOT EXISTS take_segment ON take(segment_id);
CREATE INDEX IF NOT EXISTS segment_paragraph ON segment(paragraph_id, ord);
CREATE INDEX IF NOT EXISTS paragraph_chapter ON paragraph(chapter_id, ord);
CREATE TABLE IF NOT EXISTS llm_usage (
    id                 INTEGER PRIMARY KEY,
    project_id         INTEGER REFERENCES project(id) ON DELETE CASCADE,
    chapter_id         INTEGER REFERENCES chapter(id) ON DELETE SET NULL,
    pass               TEXT NOT NULL,
    backend            TEXT NOT NULL,
    model              TEXT NOT NULL,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd           REAL,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX IF NOT EXISTS llm_usage_chapter ON llm_usage(chapter_id);
CREATE TABLE IF NOT EXISTS take_cost (
    take_id INTEGER PRIMARY KEY REFERENCES take(id) ON DELETE CASCADE,
    model   TEXT NOT NULL,
    chars   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS take_cost_model ON take_cost(model, take_id);
CREATE TABLE IF NOT EXISTS model_rate (
    model_id   TEXT PRIMARY KEY,
    multiplier REAL NOT NULL,
    fetched_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
"#;

/// Where the library lives on disk.
#[derive(Debug, Clone)]
pub struct Library {
    pub root: PathBuf,
}

impl Library {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("library.sqlite")
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.root.join("audio")
    }

    pub fn exports_dir(&self) -> PathBuf {
        self.root.join("exports")
    }

    pub fn audio_path(&self, relative: &str) -> PathBuf {
        self.audio_dir().join(relative)
    }

    pub fn open(&self) -> Result<Db> {
        std::fs::create_dir_all(self.audio_dir())?;
        Db::open(&self.db_path())
    }
}

/// Per-chapter counts for the progress counter and the cost report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChapterStats {
    pub segments: usize,
    pub draft: usize,
    pub generated: usize,
    pub approved: usize,
    pub rejected: usize,
    /// Segments whose speaker has no voice mapped.
    pub unassigned: usize,
    /// Credits charged across all takes of this chapter.
    pub credits_used: i64,
    /// Some of `credits_used` is estimated (takes without a `character-cost` header).
    pub credits_estimated: bool,
    /// Takes that cost credits (not cache hits).
    pub paid_takes: usize,
    /// Segments with at least one paid take.
    pub segments_with_paid_takes: usize,
}

impl ChapterStats {
    /// Paid re-rolls as a share of first generations.
    pub fn reroll_rate(&self) -> f64 {
        if self.segments_with_paid_takes == 0 {
            0.0
        } else {
            (self.paid_takes - self.segments_with_paid_takes) as f64
                / self.segments_with_paid_takes as f64
        }
    }
}

pub struct Db {
    conn: Connection,
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("serializable")
}

fn from_json<T: serde::de::DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}

fn row_project(r: &Row) -> rusqlite::Result<Project> {
    let voice_map: String = r.get(4)?;
    let dvs: String = r.get(5)?;
    let settings: String = r.get(6)?;
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        language: r.get(2)?,
        tts_model: r.get(3)?,
        voice_map: from_json::<BTreeMap<String, String>>(&voice_map),
        default_voice_settings: from_json(&dvs),
        settings: from_json(&settings),
    })
}

fn row_character(r: &Row) -> rusqlite::Result<Character> {
    let aliases: String = r.get(4)?;
    Ok(Character {
        id: r.get(0)?,
        project_id: r.get(1)?,
        key: r.get(2)?,
        display_name: r.get(3)?,
        aliases: from_json(&aliases),
    })
}

fn row_chapter(r: &Row) -> rusqlite::Result<Chapter> {
    let status: String = r.get(5)?;
    Ok(Chapter {
        id: r.get(0)?,
        project_id: r.get(1)?,
        order: r.get(2)?,
        title: r.get(3)?,
        source_text: r.get(4)?,
        status: ChapterStatus::parse(&status).unwrap_or(ChapterStatus::Imported),
    })
}

fn row_paragraph(r: &Row) -> rusqlite::Result<Paragraph> {
    Ok(Paragraph {
        id: r.get(0)?,
        chapter_id: r.get(1)?,
        order: r.get(2)?,
        text: r.get(3)?,
        scene_break_before: r.get::<_, i64>(4)? != 0,
    })
}

const SEGMENT_COLS: &str = "s.id, s.paragraph_id, s.ord, s.kind, s.speaker, s.text, s.settings_override, s.status, s.current_take_id, s.performance";

fn row_segment(r: &Row) -> rusqlite::Result<Segment> {
    let kind: String = r.get(3)?;
    let ovr: Option<String> = r.get(6)?;
    let status: String = r.get(7)?;
    Ok(Segment {
        id: r.get(0)?,
        paragraph_id: r.get(1)?,
        order: r.get(2)?,
        kind: SegmentKind::parse(&kind).unwrap_or(SegmentKind::Narration),
        speaker: r.get(4)?,
        text: r.get(5)?,
        settings_override: ovr.and_then(|s| serde_json::from_str(&s).ok()),
        status: SegmentStatus::parse(&status).unwrap_or(SegmentStatus::Draft),
        current_take_id: r.get(8)?,
        performance: r.get(9)?,
    })
}

const TAKE_COLS: &str =
    "id, segment_id, seed, request_id, audio_path, hash, credits_used, created_at, credits_exact";

fn row_take(r: &Row) -> rusqlite::Result<Take> {
    Ok(Take {
        id: r.get(0)?,
        segment_id: r.get(1)?,
        seed: r.get::<_, i64>(2)? as u32,
        request_id: r.get(3)?,
        audio_path: r.get(4)?,
        hash: r.get(5)?,
        credits_used: r.get(6)?,
        created_at: r.get(7)?,
        credits_exact: r.get::<_, i64>(8)? != 0,
    })
}

/// A new take to record.
pub struct NewTake<'a> {
    pub segment_id: Id,
    pub seed: u32,
    pub request_id: Option<&'a str>,
    pub audio_path: &'a str,
    pub hash: &'a str,
    pub credits_used: i64,
    /// False when `credits_used` is an estimate (no `character-cost` header).
    pub credits_exact: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        retail::init(&conn)?;
        // Libraries created before audio tags lack the performance column.
        let has_performance: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('segment') WHERE name = 'performance'")?
            .exists([])?;
        if !has_performance {
            conn.execute_batch("ALTER TABLE segment ADD COLUMN performance TEXT")?;
        }
        let has_credits_exact: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('take') WHERE name = 'credits_exact'")?
            .exists([])?;
        if !has_credits_exact {
            conn.execute_batch(
                "ALTER TABLE take ADD COLUMN credits_exact INTEGER NOT NULL DEFAULT 1",
            )?;
        }
        Ok(Self { conn })
    }

    // ---- projects -------------------------------------------------------

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut st = self.conn.prepare(
            "SELECT id, name, language, tts_model, voice_map, default_voice_settings, settings
             FROM project ORDER BY name COLLATE NOCASE",
        )?;
        Ok(st
            .query_map([], row_project)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn project(&self, id: Id) -> Result<Project> {
        self.conn
            .query_row(
                "SELECT id, name, language, tts_model, voice_map, default_voice_settings, settings
                 FROM project WHERE id = ?1",
                [id],
                row_project,
            )
            .with_context(|| format!("project {id}"))
    }

    pub fn create_project(&self, name: &str, language: &str) -> Result<Id> {
        let lang = crate::lang::by_code(language);
        let mut voice_map = BTreeMap::new();
        voice_map.insert(NARRATOR.to_string(), String::new());
        self.conn.execute(
            "INSERT INTO project (name, language, tts_model, voice_map, default_voice_settings, settings)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                name,
                lang.code,
                lang.default_tts_model,
                json(&voice_map),
                json(&VoiceSettings::default()),
                json(&ProjectSettings::default()),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_project(&self, p: &Project) -> Result<()> {
        self.conn.execute(
            "UPDATE project SET name = ?2, language = ?3, tts_model = ?4, voice_map = ?5,
                    default_voice_settings = ?6, settings = ?7 WHERE id = ?1",
            params![
                p.id,
                p.name,
                p.language,
                p.tts_model,
                json(&p.voice_map),
                json(&p.default_voice_settings),
                json(&p.settings),
            ],
        )?;
        Ok(())
    }

    pub fn delete_project(&self, id: Id) -> Result<()> {
        self.conn
            .execute("DELETE FROM project WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- character registry ---------------------------------------------

    pub fn characters(&self, project_id: Id) -> Result<Vec<Character>> {
        let mut st = self.conn.prepare(
            "SELECT id, project_id, key, display_name, aliases FROM character
             WHERE project_id = ?1 ORDER BY key",
        )?;
        Ok(st
            .query_map([project_id], row_character)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Insert a character or merge new aliases into an existing one.
    pub fn upsert_character(
        &self,
        project_id: Id,
        key: &str,
        display_name: &str,
        aliases: &[String],
    ) -> Result<()> {
        let existing: Option<(Id, String, String)> = self
            .conn
            .query_row(
                "SELECT id, display_name, aliases FROM character WHERE project_id = ?1 AND key = ?2",
                params![project_id, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match existing {
            Some((id, name, old_aliases)) => {
                let mut all: Vec<String> = from_json(&old_aliases);
                for a in aliases {
                    if !all.iter().any(|x| x.eq_ignore_ascii_case(a)) {
                        all.push(a.clone());
                    }
                }
                let name = if name.is_empty() {
                    display_name.to_string()
                } else {
                    name
                };
                self.conn.execute(
                    "UPDATE character SET display_name = ?2, aliases = ?3 WHERE id = ?1",
                    params![id, name, json(&all)],
                )?;
            }
            None => {
                self.conn.execute(
                    "INSERT INTO character (project_id, key, display_name, aliases) VALUES (?1, ?2, ?3, ?4)",
                    params![project_id, key, display_name, json(&aliases)],
                )?;
            }
        }
        Ok(())
    }

    pub fn update_character(&self, c: &Character) -> Result<()> {
        self.conn.execute(
            "UPDATE character SET display_name = ?2, aliases = ?3 WHERE id = ?1",
            params![c.id, c.display_name, json(&c.aliases)],
        )?;
        Ok(())
    }

    pub fn delete_character(&self, id: Id) -> Result<()> {
        self.conn
            .execute("DELETE FROM character WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Every speaker key used by segments of the project, so the voice map
    /// can show speakers the registry does not know about.
    pub fn speakers_in_use(&self, project_id: Id) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT s.speaker FROM segment s
             JOIN paragraph p ON p.id = s.paragraph_id
             JOIN chapter c ON c.id = p.chapter_id
             WHERE c.project_id = ?1 ORDER BY s.speaker",
        )?;
        Ok(st
            .query_map([project_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Rename a speaker key everywhere in the project (for fixing attribution).
    pub fn rename_speaker(&self, project_id: Id, from: &str, to: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE segment SET speaker = ?3, status = 'draft', current_take_id = NULL
             WHERE speaker = ?2 AND status != 'approved' AND paragraph_id IN (
                SELECT p.id FROM paragraph p JOIN chapter c ON c.id = p.chapter_id
                WHERE c.project_id = ?1)",
            params![project_id, from, to],
        )?;
        Ok(n)
    }

    // ---- chapters & paragraphs ------------------------------------------

    pub fn chapters(&self, project_id: Id) -> Result<Vec<Chapter>> {
        let mut st = self.conn.prepare(
            "SELECT id, project_id, ord, title, source_text, status FROM chapter
             WHERE project_id = ?1 ORDER BY ord, id",
        )?;
        Ok(st
            .query_map([project_id], row_chapter)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn chapter(&self, id: Id) -> Result<Chapter> {
        self.conn
            .query_row(
                "SELECT id, project_id, ord, title, source_text, status FROM chapter WHERE id = ?1",
                [id],
                row_chapter,
            )
            .with_context(|| format!("chapter {id}"))
    }

    /// Import a chapter: store the text and split it into paragraphs.
    pub fn import_chapter(&mut self, project_id: Id, title: &str, source_text: &str) -> Result<Id> {
        let paragraphs = segment::split_paragraphs(source_text);
        let tx = self.conn.transaction()?;
        let ord: i64 = tx.query_row(
            "SELECT COALESCE(MAX(ord), 0) + 1 FROM chapter WHERE project_id = ?1",
            [project_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO chapter (project_id, ord, title, source_text, status) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project_id, ord, title, source_text, ChapterStatus::Imported.as_str()],
        )?;
        let chapter_id = tx.last_insert_rowid();
        insert_paragraphs(&tx, chapter_id, &paragraphs)?;
        tx.commit()?;
        Ok(chapter_id)
    }

    /// Replace a chapter's text and re-split it into paragraphs. Only allowed
    /// while no segment of the chapter has audio, so no paid take is lost.
    pub fn replace_chapter_text(&mut self, id: Id, title: &str, source_text: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let takes: i64 = tx.query_row(
            "SELECT COUNT(*) FROM take t JOIN segment s ON s.id = t.segment_id
             JOIN paragraph p ON p.id = s.paragraph_id WHERE p.chapter_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if takes > 0 {
            return Err(anyhow!(
                "this chapter already has audio; start a new chapter instead"
            ));
        }
        tx.execute(
            "UPDATE chapter SET title = ?2, source_text = ?3, status = ?4 WHERE id = ?1",
            params![id, title, source_text, ChapterStatus::Imported.as_str()],
        )?;
        tx.execute("DELETE FROM paragraph WHERE chapter_id = ?1", [id])?;
        insert_paragraphs(&tx, id, &segment::split_paragraphs(source_text))?;
        tx.commit()?;
        Ok(())
    }

    pub fn rename_chapter(&self, id: Id, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chapter SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    pub fn delete_chapter(&self, id: Id) -> Result<()> {
        self.conn
            .execute("DELETE FROM chapter WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn set_chapter_status(&self, id: Id, status: ChapterStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE chapter SET status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    pub fn paragraphs(&self, chapter_id: Id) -> Result<Vec<Paragraph>> {
        let mut st = self.conn.prepare(
            "SELECT id, chapter_id, ord, text, scene_break_before FROM paragraph
             WHERE chapter_id = ?1 ORDER BY ord",
        )?;
        Ok(st
            .query_map([chapter_id], row_paragraph)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn paragraph(&self, id: Id) -> Result<Paragraph> {
        self.conn
            .query_row(
                "SELECT id, chapter_id, ord, text, scene_break_before FROM paragraph WHERE id = ?1",
                [id],
                row_paragraph,
            )
            .with_context(|| format!("paragraph {id}"))
    }

    // ---- segments -------------------------------------------------------

    /// All segments of a chapter in reading order.
    pub fn chapter_segments(&self, chapter_id: Id) -> Result<Vec<Segment>> {
        let sql = format!(
            "SELECT {SEGMENT_COLS} FROM segment s JOIN paragraph p ON p.id = s.paragraph_id
             WHERE p.chapter_id = ?1 ORDER BY p.ord, s.ord"
        );
        let mut st = self.conn.prepare(&sql)?;
        Ok(st
            .query_map([chapter_id], row_segment)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn paragraph_segments(&self, paragraph_id: Id) -> Result<Vec<Segment>> {
        let sql = format!(
            "SELECT {SEGMENT_COLS} FROM segment s WHERE s.paragraph_id = ?1 ORDER BY s.ord"
        );
        let mut st = self.conn.prepare(&sql)?;
        Ok(st
            .query_map([paragraph_id], row_segment)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn segment(&self, id: Id) -> Result<Segment> {
        let sql = format!("SELECT {SEGMENT_COLS} FROM segment s WHERE s.id = ?1");
        self.conn
            .query_row(&sql, [id], row_segment)
            .with_context(|| format!("segment {id}"))
    }

    /// The chapter a segment belongs to.
    pub fn segment_chapter_id(&self, segment_id: Id) -> Result<Id> {
        Ok(self.conn.query_row(
            "SELECT p.chapter_id FROM segment s JOIN paragraph p ON p.id = s.paragraph_id WHERE s.id = ?1",
            [segment_id],
            |r| r.get(0),
        )?)
    }

    /// Replace a paragraph's segments (used by the segmentation pass).
    /// Refuses to touch a paragraph that already has approved segments.
    pub fn replace_paragraph_segments(
        &mut self,
        paragraph_id: Id,
        segs: &[(SegmentKind, String, String)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let approved: i64 = tx.query_row(
            "SELECT COUNT(*) FROM segment WHERE paragraph_id = ?1 AND status = 'approved'",
            [paragraph_id],
            |r| r.get(0),
        )?;
        if approved > 0 {
            return Err(anyhow!(
                "paragraph has approved segments; unapprove them first"
            ));
        }
        tx.execute(
            "DELETE FROM segment WHERE paragraph_id = ?1",
            [paragraph_id],
        )?;
        for (i, (kind, speaker, text)) in segs.iter().enumerate() {
            tx.execute(
                "INSERT INTO segment (paragraph_id, ord, kind, speaker, text, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'draft')",
                params![paragraph_id, i as i64, kind.as_str(), speaker, text],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_segment_speaker(&self, id: Id, speaker: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE segment SET speaker = ?2, status = 'draft', current_take_id = NULL
             WHERE id = ?1 AND status != 'approved'",
            params![id, speaker],
        )?;
        Ok(())
    }

    /// Set (or clear) the tagged performance. Changing what is spoken drops
    /// the segment back to draft; earlier takes stay in the take list.
    pub fn set_segment_performance(&self, id: Id, performance: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE segment SET performance = ?2,
                    status = CASE WHEN performance IS ?2 THEN status ELSE 'draft' END,
                    current_take_id = CASE WHEN performance IS ?2 THEN current_take_id ELSE NULL END
             WHERE id = ?1 AND status != 'approved'",
            params![id, performance],
        )?;
        Ok(())
    }

    pub fn set_segment_kind(&self, id: Id, kind: SegmentKind) -> Result<()> {
        self.conn.execute(
            "UPDATE segment SET kind = ?2 WHERE id = ?1 AND status != 'approved'",
            params![id, kind.as_str()],
        )?;
        Ok(())
    }

    pub fn set_segment_status(&self, id: Id, status: SegmentStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE segment SET status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    pub fn set_segment_override(&self, id: Id, settings: Option<&VoiceSettings>) -> Result<()> {
        self.conn.execute(
            "UPDATE segment SET settings_override = ?2 WHERE id = ?1 AND status != 'approved'",
            params![id, settings.map(json)],
        )?;
        Ok(())
    }

    /// Make `take_id` the segment's current take. Picking a take also moves an
    /// unapproved segment to generated.
    pub fn set_current_take(&self, segment_id: Id, take_id: Id) -> Result<()> {
        self.conn.execute(
            "UPDATE segment SET current_take_id = ?2,
                    status = CASE WHEN status = 'approved' THEN status ELSE 'generated' END
             WHERE id = ?1",
            params![segment_id, take_id],
        )?;
        Ok(())
    }

    /// Merge segment `id` with the next segment in its paragraph. The merged
    /// text is cut from the paragraph so the original spacing survives.
    pub fn merge_with_next(&mut self, id: Id) -> Result<()> {
        let seg = self.segment(id)?;
        let para = self.paragraph(seg.paragraph_id)?;
        let segs = self.paragraph_segments(seg.paragraph_id)?;
        let i = segs
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| anyhow!("segment vanished"))?;
        let next = segs
            .get(i + 1)
            .ok_or_else(|| anyhow!("no next segment in this paragraph"))?;
        if seg.status == SegmentStatus::Approved || next.status == SegmentStatus::Approved {
            return Err(anyhow!("unapprove both segments before merging"));
        }
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        let merged = segment::merged_text(&para.text, &texts, i, i + 1);
        // Keep the audio tags of both halves when either had some.
        let performance = (seg.performance.is_some() || next.performance.is_some())
            .then(|| {
                let a = seg.performance.as_deref().unwrap_or(&seg.text);
                let b = next.performance.as_deref().unwrap_or(&next.text);
                crate::tags::validate(&merged, &format!("{a} {b}"))
                    .ok()
                    .flatten()
            })
            .flatten();
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE segment SET text = ?2, performance = ?3, status = 'draft', current_take_id = NULL
             WHERE id = ?1",
            params![id, merged, performance],
        )?;
        tx.execute("DELETE FROM segment WHERE id = ?1", [next.id])?;
        tx.commit()?;
        Ok(())
    }

    /// Split a segment into `parts` (validated by `segment::split_at_markers`).
    pub fn split_segment(&mut self, id: Id, parts: &[String]) -> Result<()> {
        let seg = self.segment(id)?;
        if seg.status == SegmentStatus::Approved {
            return Err(anyhow!("unapprove the segment before splitting"));
        }
        let tx = self.conn.transaction()?;
        let n = parts.len() as i64 - 1;
        tx.execute(
            "UPDATE segment SET ord = ord + ?3 WHERE paragraph_id = ?1 AND ord > ?2",
            params![seg.paragraph_id, seg.order, n],
        )?;
        tx.execute(
            "UPDATE segment SET text = ?2, performance = NULL, status = 'draft', current_take_id = NULL
             WHERE id = ?1",
            params![id, parts[0]],
        )?;
        for (i, part) in parts.iter().enumerate().skip(1) {
            tx.execute(
                "INSERT INTO segment (paragraph_id, ord, kind, speaker, text, settings_override, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'draft')",
                params![
                    seg.paragraph_id,
                    seg.order + i as i64,
                    seg.kind.as_str(),
                    seg.speaker,
                    part,
                    seg.settings_override.as_ref().map(json),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- takes ----------------------------------------------------------

    pub fn insert_take(&self, t: &NewTake) -> Result<Id> {
        self.conn.execute(
            "INSERT INTO take (segment_id, seed, request_id, audio_path, hash, credits_used, credits_exact)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                t.segment_id,
                t.seed as i64,
                t.request_id,
                t.audio_path,
                t.hash,
                t.credits_used,
                t.credits_exact as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn takes(&self, segment_id: Id) -> Result<Vec<Take>> {
        let sql = format!("SELECT {TAKE_COLS} FROM take WHERE segment_id = ?1 ORDER BY id");
        let mut st = self.conn.prepare(&sql)?;
        Ok(st
            .query_map([segment_id], row_take)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn take(&self, id: Id) -> Result<Take> {
        let sql = format!("SELECT {TAKE_COLS} FROM take WHERE id = ?1");
        self.conn
            .query_row(&sql, [id], row_take)
            .with_context(|| format!("take {id}"))
    }

    /// Number of takes per segment, for the review list.
    pub fn take_counts(&self, chapter_id: Id) -> Result<std::collections::HashMap<Id, usize>> {
        let mut st = self.conn.prepare(
            "SELECT t.segment_id, COUNT(*) FROM take t JOIN segment s ON s.id = t.segment_id
             JOIN paragraph p ON p.id = s.paragraph_id WHERE p.chapter_id = ?1 GROUP BY t.segment_id",
        )?;
        let rows = st.query_map([chapter_id], |r| {
            Ok((r.get::<_, Id>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Any existing take with this hash, from any segment.
    pub fn take_by_hash(&self, hash: &str) -> Result<Option<Take>> {
        let sql = format!("SELECT {TAKE_COLS} FROM take WHERE hash = ?1 ORDER BY id LIMIT 1");
        Ok(self.conn.query_row(&sql, [hash], row_take).optional()?)
    }

    pub fn chapter_stats(&self, chapter_id: Id, project: &Project) -> Result<ChapterStats> {
        let segs = self.chapter_segments(chapter_id)?;
        let mut s = ChapterStats {
            segments: segs.len(),
            ..Default::default()
        };
        for seg in &segs {
            match seg.status {
                SegmentStatus::Draft => s.draft += 1,
                SegmentStatus::Generated => s.generated += 1,
                SegmentStatus::Approved => s.approved += 1,
                SegmentStatus::Rejected => s.rejected += 1,
            }
            if project.voice_for(&seg.speaker).is_none() {
                s.unassigned += 1;
            }
        }
        let (credits, paid, segs_paid, estimated): (i64, i64, i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(t.credits_used), 0),
                    COALESCE(SUM(t.credits_used > 0), 0),
                    COUNT(DISTINCT CASE WHEN t.credits_used > 0 THEN t.segment_id END),
                    COALESCE(SUM(t.credits_used > 0 AND t.credits_exact = 0), 0)
             FROM take t JOIN segment s ON s.id = t.segment_id
             JOIN paragraph p ON p.id = s.paragraph_id WHERE p.chapter_id = ?1",
            [chapter_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        s.credits_used = credits;
        s.credits_estimated = estimated > 0;
        s.paid_takes = paid as usize;
        s.segments_with_paid_takes = segs_paid as usize;
        Ok(s)
    }
}

fn insert_paragraphs(
    tx: &rusqlite::Transaction,
    chapter_id: Id,
    ps: &[ParsedParagraph],
) -> Result<()> {
    for (i, p) in ps.iter().enumerate() {
        tx.execute(
            "INSERT INTO paragraph (chapter_id, ord, text, scene_break_before) VALUES (?1, ?2, ?3, ?4)",
            params![chapter_id, i as i64, p.text, p.scene_break_before as i64],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Db, Project, Id) {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db
            .import_chapter(
                pid,
                "One",
                "\"It is,\" started Vortram, \"truly magnificent!\"\n\nThe end.",
            )
            .unwrap();
        let project = db.project(pid).unwrap();
        (db, project, cid)
    }

    fn segment_paragraph(db: &mut Db, project: &Project, chapter_id: Id) {
        let lang = crate::lang::by_code(&project.language);
        for p in db.paragraphs(chapter_id).unwrap() {
            let segs: Vec<_> = segment::parse_paragraph(&p.text, lang)
                .into_iter()
                .map(|d| (d.kind, d.speaker.clone(), d.text(&p.text).to_string()))
                .collect();
            db.replace_paragraph_segments(p.id, &segs).unwrap();
        }
    }

    #[test]
    fn project_defaults_roundtrip() {
        let (db, mut project, _) = setup();
        assert_eq!(project.tts_model, "eleven_v4");
        assert!(project.voice_map.contains_key(NARRATOR));
        assert_eq!(
            project.voice_for(NARRATOR),
            None,
            "empty voice id counts as unassigned"
        );
        project.voice_map.insert(NARRATOR.into(), "voice123".into());
        project.settings.concurrency = 5;
        db.update_project(&project).unwrap();
        assert_eq!(db.project(project.id).unwrap(), project);
    }

    #[test]
    fn import_segment_merge_split() {
        let (mut db, project, cid) = setup();
        assert_eq!(db.paragraphs(cid).unwrap().len(), 2);
        segment_paragraph(&mut db, &project, cid);
        let segs = db.chapter_segments(cid).unwrap();
        assert_eq!(segs.len(), 4);

        // Merge the dialogue tag into the following quote, then split it back.
        db.merge_with_next(segs[1].id).unwrap();
        let merged = db.segment(segs[1].id).unwrap();
        assert_eq!(merged.text, "started Vortram, \"truly magnificent!\"");
        assert_eq!(db.chapter_segments(cid).unwrap().len(), 3);

        let parts =
            segment::split_at_markers(&merged.text, "started Vortram, | \"truly magnificent!\"")
                .unwrap();
        db.split_segment(merged.id, &parts).unwrap();
        let after = db.chapter_segments(cid).unwrap();
        let texts: Vec<_> = after.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "\"It is,\"",
                "started Vortram,",
                "\"truly magnificent!\"",
                "The end."
            ]
        );
        let para = db.paragraph(after[0].paragraph_id).unwrap();
        assert!(segment::locate(&para.text, &texts[..3]).is_ok());
    }

    #[test]
    fn takes_stats_and_locking() {
        let (mut db, mut project, cid) = setup();
        segment_paragraph(&mut db, &project, cid);
        let segs = db.chapter_segments(cid).unwrap();
        let s0 = segs[0].id;

        let t1 = db
            .insert_take(&NewTake {
                segment_id: s0,
                seed: 1,
                request_id: Some("r1"),
                audio_path: "a.wav",
                hash: "h1",
                credits_used: 8,
                credits_exact: true,
            })
            .unwrap();
        db.set_current_take(s0, t1).unwrap();
        let t2 = db
            .insert_take(&NewTake {
                segment_id: s0,
                seed: 2,
                request_id: Some("r2"),
                audio_path: "b.wav",
                hash: "h2",
                credits_used: 8,
                credits_exact: true,
            })
            .unwrap();
        db.set_current_take(s0, t2).unwrap();
        // A cache hit on another segment costs nothing.
        let t3 = db
            .insert_take(&NewTake {
                segment_id: segs[1].id,
                seed: 1,
                request_id: None,
                audio_path: "a.wav",
                hash: "h1",
                credits_used: 0,
                credits_exact: true,
            })
            .unwrap();
        db.set_current_take(segs[1].id, t3).unwrap();

        assert_eq!(db.take_by_hash("h1").unwrap().unwrap().id, t1);
        assert_eq!(db.takes(s0).unwrap().len(), 2);

        project.voice_map.insert(NARRATOR.into(), "v".into());
        let stats = db.chapter_stats(cid, &project).unwrap();
        assert_eq!(stats.segments, 4);
        assert_eq!(stats.generated, 2);
        assert_eq!(stats.draft, 2);
        assert_eq!(stats.credits_used, 16);
        assert_eq!(stats.paid_takes, 2);
        assert_eq!(stats.segments_with_paid_takes, 1);
        assert!((stats.reroll_rate() - 1.0).abs() < 1e-9);
        // Two dialogue segments have the unknown speaker.
        assert_eq!(stats.unassigned, 2);

        // Approved segments are locked against speaker edits.
        db.set_segment_status(s0, SegmentStatus::Approved).unwrap();
        db.set_segment_speaker(s0, "vortram").unwrap();
        let s = db.segment(s0).unwrap();
        assert_eq!(s.speaker, segment::UNKNOWN_SPEAKER);
        assert_eq!(s.current_take_id, Some(t2));
        assert!(db.merge_with_next(s0).is_err());

        // Renaming a speaker resets unapproved segments to draft.
        assert_eq!(
            db.rename_speaker(project.id, segment::UNKNOWN_SPEAKER, "vortram")
                .unwrap(),
            1
        );
    }

    #[test]
    fn performance_tags_follow_edits() {
        let (mut db, project, cid) = setup();
        segment_paragraph(&mut db, &project, cid);
        let segs = db.chapter_segments(cid).unwrap();
        let (tag, quote) = (segs[1].id, segs[2].id);

        // Setting tags on a generated segment drops it back to draft.
        let t = db
            .insert_take(&NewTake {
                segment_id: tag,
                seed: 1,
                request_id: None,
                audio_path: "a.wav",
                hash: "h",
                credits_used: 1,
                credits_exact: true,
            })
            .unwrap();
        db.set_current_take(tag, t).unwrap();
        db.set_segment_performance(tag, Some("[thoughtful] started Vortram,"))
            .unwrap();
        let s = db.segment(tag).unwrap();
        assert_eq!(
            s.performance.as_deref(),
            Some("[thoughtful] started Vortram,")
        );
        assert_eq!((s.status, s.current_take_id), (SegmentStatus::Draft, None));
        assert_eq!(s.spoken_text("eleven_v4"), "[thoughtful] started Vortram,");
        assert_eq!(s.spoken_text("eleven_multilingual_v2"), "started Vortram,");

        // Merging keeps both halves' tags.
        db.set_segment_performance(quote, Some("\"[excited] truly magnificent!\""))
            .unwrap();
        db.merge_with_next(tag).unwrap();
        let merged = db.segment(tag).unwrap();
        assert_eq!(
            merged.performance.as_deref(),
            Some("[thoughtful] started Vortram, \"[excited] truly magnificent!\"")
        );
        // Splitting clears them (the user re-tags each half).
        db.split_segment(
            tag,
            &["started Vortram,".into(), "\"truly magnificent!\"".into()],
        )
        .unwrap();
        assert_eq!(db.segment(tag).unwrap().performance, None);
    }

    #[test]
    fn chapter_text_can_change_until_audio_exists() {
        let (mut db, project, cid) = setup();
        segment_paragraph(&mut db, &project, cid);
        db.replace_chapter_text(cid, "One (rev)", "New first.\n\nNew second.\n\nThird.")
            .unwrap();
        let ch = db.chapter(cid).unwrap();
        assert_eq!(
            (ch.title.as_str(), ch.status),
            ("One (rev)", ChapterStatus::Imported)
        );
        assert_eq!(db.paragraphs(cid).unwrap().len(), 3);
        assert!(db.chapter_segments(cid).unwrap().is_empty());

        segment_paragraph(&mut db, &project, cid);
        let s = db.chapter_segments(cid).unwrap()[0].id;
        db.insert_take(&NewTake {
            segment_id: s,
            seed: 1,
            request_id: None,
            audio_path: "a.wav",
            hash: "h",
            credits_used: 1,
            credits_exact: true,
        })
        .unwrap();
        assert!(db.replace_chapter_text(cid, "x", "y").is_err());
    }

    #[test]
    fn old_libraries_gain_the_performance_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                &SCHEMA
                    .replace("    performance       TEXT,\n", "")
                    .replace("    credits_exact INTEGER NOT NULL DEFAULT 1,\n", ""),
            )
            .unwrap();
            let cols: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('segment') WHERE name = 'performance'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(cols, 0);
        }
        let mut db = Db::open(&path).unwrap();
        let has = |table: &str, col: &str| -> bool {
            db.conn
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .unwrap()
                .exists([])
                .unwrap()
        };
        assert!(has("segment", "performance") && has("take", "credits_exact"));
        let pid = db.create_project("Old", "en").unwrap();
        let cid = db.import_chapter(pid, "One", "Text.").unwrap();
        let project = db.project(pid).unwrap();
        segment_paragraph(&mut db, &project, cid);
        assert_eq!(db.chapter_segments(cid).unwrap()[0].performance, None);
        drop(db);
        Db::open(&path).unwrap();
    }

    #[test]
    fn character_registry_merges_aliases() {
        let (db, project, _) = setup();
        db.upsert_character(project.id, "vortram", "Vortram", &["the old man".into()])
            .unwrap();
        db.upsert_character(
            project.id,
            "vortram",
            "",
            &["The Old Man".into(), "Vort".into()],
        )
        .unwrap();
        let cs = db.characters(project.id).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].display_name, "Vortram");
        assert_eq!(
            cs[0].aliases,
            vec!["the old man".to_string(), "Vort".to_string()]
        );
    }
}
