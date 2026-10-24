//! Cost tracking: Claude usage per call (`llm_usage`), the model and spoken
//! characters of each paid take (`take_cost`, so the account's real credit
//! rate can be learned from `character-cost`), and ElevenLabs' per-model
//! list rates (`model_rate`, so estimates work offline too).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::costs::{self, CreditRate, LearnedSample};
use crate::llm::CallUsage;
use crate::model::Id;

/// Claude usage of a chapter for one pass and backend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PassUsage {
    /// `attribution` or `tags`.
    pub pass: String,
    /// `api` or `claude_code`.
    pub backend: String,
    /// Recorded rows (one per call and model).
    pub calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// API-equivalent list price of the priced calls.
    pub cost_usd: f64,
    /// Rows without a cost (a model missing from the price table).
    pub unpriced: usize,
}

impl PassUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

impl Db {
    pub fn record_llm_usage(
        &self,
        project_id: Option<Id>,
        chapter_id: Option<Id>,
        pass: &str,
        backend: &str,
        u: &CallUsage,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO llm_usage (project_id, chapter_id, pass, backend, model, input_tokens,
                    output_tokens, cache_read_tokens, cache_write_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                project_id,
                chapter_id,
                pass,
                backend,
                u.model,
                u.input_tokens as i64,
                u.output_tokens as i64,
                u.cache_read_tokens as i64,
                u.cache_write_tokens as i64,
                u.cost_usd,
            ],
        )?;
        Ok(())
    }

    /// The chapter's Claude usage, per pass and backend.
    pub fn chapter_llm_usage(&self, chapter_id: Id) -> Result<Vec<PassUsage>> {
        let mut st = self.conn.prepare(
            "SELECT pass, backend, COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_read_tokens), SUM(cache_write_tokens), COALESCE(SUM(cost_usd), 0),
                    SUM(cost_usd IS NULL)
             FROM llm_usage WHERE chapter_id = ?1 GROUP BY pass, backend ORDER BY pass, backend",
        )?;
        let rows = st.query_map([chapter_id], |r| {
            Ok(PassUsage {
                pass: r.get(0)?,
                backend: r.get(1)?,
                calls: r.get::<_, i64>(2)? as usize,
                input_tokens: r.get::<_, i64>(3)? as u64,
                output_tokens: r.get::<_, i64>(4)? as u64,
                cache_read_tokens: r.get::<_, i64>(5)? as u64,
                cache_write_tokens: r.get::<_, i64>(6)? as u64,
                cost_usd: r.get(7)?,
                unpriced: r.get::<_, i64>(8)? as usize,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Remember the model and spoken characters of a paid take.
    pub fn record_take_cost(&self, take_id: Id, model: &str, chars: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO take_cost (take_id, model, chars) VALUES (?1, ?2, ?3)",
            params![take_id, model, chars],
        )?;
        Ok(())
    }

    /// Credits and characters of the most recent exact, paid takes of
    /// `model` in this library (cache hits and estimates excluded).
    pub fn learned_rate(&self, model: &str) -> Result<Option<LearnedSample>> {
        let (n, credits, chars): (i64, i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(credits_used), 0), COALESCE(SUM(chars), 0) FROM (
                SELECT t.credits_used, c.chars FROM take t JOIN take_cost c ON c.take_id = t.id
                WHERE c.model = ?1 AND t.credits_exact = 1 AND t.credits_used > 0 AND c.chars > 0
                ORDER BY t.id DESC LIMIT ?2)",
            params![model, costs::LEARN_FROM_TAKES as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok((n > 0).then_some(LearnedSample {
            takes: n as usize,
            credits,
            chars,
        }))
    }

    /// Store the list rates fetched from /v1/models.
    pub fn set_model_rates(&self, rates: &[(String, f64)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (model, multiplier) in rates {
            tx.execute(
                "INSERT INTO model_rate (model_id, multiplier) VALUES (?1, ?2)
                 ON CONFLICT(model_id) DO UPDATE SET multiplier = excluded.multiplier,
                    fetched_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
                params![model, multiplier],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn model_rate(&self, model: &str) -> Result<Option<f64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT multiplier FROM model_rate WHERE model_id = ?1",
                [model],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Credits per character for `model`, with where the number comes from.
    /// Never fails: on a database error it falls back to the default table.
    pub fn credit_rate(&self, model: &str) -> CreditRate {
        let learned = self.learned_rate(model).unwrap_or_else(|e| {
            log::warn!("learned rate: {e:#}");
            None
        });
        let list = self.model_rate(model).ok().flatten();
        costs::credit_rate(model, learned, list)
    }
}

/// An llm_usage row in tests: (pass, backend, model, output tokens, cost).
#[cfg(test)]
pub type UsageRow = (String, String, String, i64, Option<f64>);

#[cfg(test)]
impl Db {
    pub fn all_llm_usage(&self) -> Result<Vec<UsageRow>> {
        let mut st = self.conn.prepare(
            "SELECT pass, backend, model, output_tokens, cost_usd FROM llm_usage ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The take_cost row of a take: (model, chars).
    pub fn take_cost(&self, take_id: Id) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT model, chars FROM take_cost WHERE take_id = ?1",
                [take_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{CallUsage, CreditRate};
    use crate::costs::RateBasis;

    fn usage(model: &str, input: u64, output: u64, cost: Option<f64>) -> CallUsage {
        CallUsage {
            model: model.into(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 10,
            cache_write_tokens: 20,
            cost_usd: cost,
        }
    }

    #[test]
    fn llm_usage_is_summed_per_pass_and_survives_chapter_deletion() {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db.import_chapter(pid, "One", "Text.").unwrap();
        let other = db.import_chapter(pid, "Two", "Text.").unwrap();
        let rec = |pass: &str, backend: &str, u: CallUsage, chapter: Id| {
            db.record_llm_usage(Some(pid), Some(chapter), pass, backend, &u)
                .unwrap()
        };
        rec(
            "attribution",
            "claude_code",
            usage("claude-opus-5-5", 100, 50, Some(0.5)),
            cid,
        );
        rec(
            "attribution",
            "claude_code",
            usage("claude-opus-5-5", 200, 70, Some(0.25)),
            cid,
        );
        rec("tags", "claude_code", usage("mystery", 10, 5, None), cid);
        rec(
            "attribution",
            "api",
            usage("claude-opus-5-5", 1, 1, Some(9.0)),
            other,
        );

        let u = db.chapter_llm_usage(cid).unwrap();
        assert_eq!(u.len(), 2);
        assert_eq!(
            (u[0].pass.as_str(), u[0].backend.as_str(), u[0].calls),
            ("attribution", "claude_code", 2)
        );
        assert_eq!((u[0].input_tokens, u[0].output_tokens), (300, 120));
        assert_eq!(u[0].total_tokens(), 300 + 120 + 20 + 40);
        assert!((u[0].cost_usd - 0.75).abs() < 1e-12);
        assert_eq!((u[1].pass.as_str(), u[1].unpriced), ("tags", 1));

        // Deleting the chapter keeps the spend on record for the project.
        db.delete_chapter(cid).unwrap();
        let left: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM llm_usage WHERE project_id = ?1",
                [pid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 4);
        assert!(db.chapter_llm_usage(cid).unwrap().is_empty());
    }

    #[test]
    fn credit_rate_is_learned_from_exact_paid_takes() {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db.create_project("Book", "en").unwrap();
        let cid = db.import_chapter(pid, "One", "Text.").unwrap();
        let p = db.paragraphs(cid).unwrap()[0].clone();
        db.replace_paragraph_segments(
            p.id,
            &[(SegmentKind::Narration, NARRATOR.into(), "Text.".into())],
        )
        .unwrap();
        let seg = db.chapter_segments(cid).unwrap()[0].id;
        let take = |db: &Db, n: usize, credits: i64, exact: bool| -> Id {
            db.insert_take(&NewTake {
                segment_id: seg,
                seed: n as u32,
                request_id: None,
                audio_path: "a.wav",
                hash: &format!("h{n}"),
                credits_used: credits,
                credits_exact: exact,
            })
            .unwrap()
        };

        // Nothing learned yet: the default table.
        assert_eq!(db.credit_rate("eleven_v4").basis, RateBasis::Default);
        db.set_model_rates(&[("eleven_v4".into(), 1.0), ("eleven_flash_v2_5".into(), 0.5)])
            .unwrap();
        db.set_model_rates(&[("eleven_v4".into(), 0.9)]).unwrap();
        assert_eq!(db.model_rate("eleven_v4").unwrap(), Some(0.9));
        assert_eq!(
            db.credit_rate("eleven_v4"),
            CreditRate {
                per_char: 0.9,
                basis: RateBasis::ListRate
            }
        );

        // 60 exact takes: only the latest 50 count. Old ones at 1.0/char,
        // recent ones at 0.44/char.
        for n in 0..60 {
            let credits = if n < 10 { 100 } else { 44 };
            let t = take(&db, n, credits, true);
            db.record_take_cost(t, "eleven_v4", 100).unwrap();
        }
        // Estimates, cache hits and other models are ignored.
        let est = take(&db, 100, 500, false);
        db.record_take_cost(est, "eleven_v4", 100).unwrap();
        let cached = take(&db, 101, 0, true);
        db.record_take_cost(cached, "eleven_v4", 100).unwrap();
        let flash = take(&db, 102, 10, true);
        db.record_take_cost(flash, "eleven_flash_v2_5", 100)
            .unwrap();

        let sample = db.learned_rate("eleven_v4").unwrap().unwrap();
        assert_eq!(sample.takes, 50);
        assert_eq!((sample.credits, sample.chars), (50 * 44, 50 * 100));
        let rate = db.credit_rate("eleven_v4");
        assert!((rate.per_char - 0.44).abs() < 1e-12);
        assert_eq!(rate.basis, RateBasis::Learned { takes: 50 });
        assert_eq!(db.learned_rate("eleven_v3").unwrap(), None);
    }

    #[test]
    fn old_libraries_gain_the_cost_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            let cut = SCHEMA.find("CREATE TABLE IF NOT EXISTS llm_usage").unwrap();
            conn.execute_batch(&SCHEMA[..cut]).unwrap();
        }
        let db = Db::open(&path).unwrap();
        for table in ["llm_usage", "take_cost", "model_rate"] {
            let n: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table}");
        }
        db.record_llm_usage(None, None, "tags", "api", &CallUsage::default())
            .unwrap();
    }
}
