//! Take check in the UI: the ⚠ marker on segment rows, the "N flagged" chip
//! and "Check chapter" in the Review toolbar, the Check section of the
//! detail panel (the transcript against the text, with differences
//! highlighted), and the "Take checking" section of Settings.
//!
//! A check only flags: nothing here blocks approval or any other action.

use std::collections::HashMap;
use std::rc::Rc;

use eframe::egui::{self, Color32, RichText, Stroke, text::LayoutJob};

use super::App;
use crate::check::{
    BUILT_WITH_WHISPER, CheckCommand, CheckConfig, CheckStatus, IssueKind, TakeCheck, models, text,
};
use crate::db::Library;
use crate::lang;
use crate::model::*;
use crate::theme::{self, Palette};
use crate::worker::{Command, Event};

/// Take-check state kept on the App.
#[derive(Default)]
pub(super) struct CheckState {
    /// Results for the open chapter's current takes, by segment id.
    by_segment: HashMap<Id, TakeCheck>,
    /// The detail panel's alignment, for (take id, transcript).
    diff: Option<((Id, String), Rc<text::Check>)>,
    /// Checker queue: done of total; 0 of 0 when idle.
    progress: (usize, usize),
    download: Option<Download>,
}

struct Download {
    model: String,
    received: u64,
    total: Option<u64>,
    running: bool,
    error: Option<String>,
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes >> 20)
}

impl App {
    pub(super) fn reload_checks(&mut self) {
        self.check.by_segment = match self.view.chapter_id {
            Some(cid) => match self.db.chapter_checks(cid) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("take checks: {e:#}");
                    HashMap::new()
                }
            },
            None => HashMap::new(),
        };
    }

    pub(super) fn on_check_event(&mut self, ev: Event) {
        match ev {
            Event::TakeChecked { segment_id } => {
                if self.view.segments.iter().any(|s| s.id == segment_id) {
                    self.reload_checks();
                }
            }
            Event::CheckProgress { done, total } => self.check.progress = (done, total),
            Event::ModelDownload {
                model,
                received,
                total,
            } => {
                self.check.download = Some(Download {
                    model,
                    received,
                    total,
                    running: true,
                    error: None,
                });
            }
            Event::ModelDownloadFinished { model, error } => {
                if let Some(e) = &error {
                    log::warn!("downloading the Whisper model {model}: {e}");
                    self.note(
                        true,
                        format!("Downloading the Whisper model {model} failed; see Settings → Take checking."),
                    );
                }
                self.check.download = Some(Download {
                    model,
                    received: 0,
                    total: None,
                    running: false,
                    error,
                });
            }
            _ => {}
        }
    }

    /// The current take's check, if it has one.
    fn current_check(&self, seg: &Segment) -> Option<&TakeCheck> {
        self.check
            .by_segment
            .get(&seg.id)
            .filter(|c| Some(c.take_id) == seg.current_take_id)
    }

    /// Segment rows: an amber ⚠ after the status icon when the current take
    /// is flagged. The cell is always there, so the columns stay aligned.
    pub(super) fn check_marker(&self, ui: &mut egui::Ui, seg: &Segment) {
        let p = theme::palette(ui);
        let flagged = self.current_check(seg).filter(|c| c.is_flagged());
        let r = super::left_aligned(ui, 14.0, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(if flagged.is_some() { "⚠" } else { "" }).color(p.warning),
                )
                .selectable(false),
            )
        });
        if let Some(c) = flagged {
            r.on_hover_text(format!(
                "Take check: {}",
                c.reason().unwrap_or("flagged for a listen")
            ));
        }
    }

    fn flagged_segments(&self) -> Vec<usize> {
        self.view
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| self.current_check(s).is_some_and(|c| c.is_flagged()))
            .map(|(i, _)| i)
            .collect()
    }

    /// Review toolbar, first row: "⚠ N flagged"; a click selects the next one.
    pub(super) fn check_chip(&mut self, ui: &mut egui::Ui) {
        let flagged = self.flagged_segments();
        if flagged.is_empty() {
            return;
        }
        let p = *theme::palette(ui);
        let chip = egui::Button::new(
            RichText::new(format!("⚠ {} flagged", flagged.len()))
                .small()
                .color(p.warning),
        )
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, p.warning.gamma_multiply(0.6)))
        .corner_radius(10);
        if ui
            .add(chip)
            .on_hover_text("The take check heard something unexpected. Click to select the next flagged segment.")
            .clicked()
        {
            let cur = self.selected;
            let next = flagged
                .iter()
                .copied()
                .find(|&i| cur.is_none_or(|c| i > c))
                .unwrap_or(flagged[0]);
            self.select(next);
        }
    }

    /// Review toolbar, actions row: "Check chapter", or progress and Stop.
    pub(super) fn check_toolbar(&mut self, ui: &mut egui::Ui, chapter_id: Id) {
        let (done, total) = self.check.progress;
        if total > 0 {
            ui.add(egui::Spinner::new());
            ui.label(format!("Checking takes {done}/{total}"))
                .on_hover_text("Transcribing on this computer at low priority");
            if ui.small_button("Stop").clicked() {
                self.worker.send(Command::Check(CheckCommand::Stop));
            }
            return;
        }
        let model = &self.cfg.check.model;
        let words = BUILT_WITH_WHISPER
            && models::info(model)
                .is_some_and(|m| models::installed(&self.lib, m) != models::Installed::No);
        let hover = if words {
            format!(
                "Transcribe the current takes that have no result yet with Whisper ({model}) on this computer, and flag differences from the text. Free; runs in the background."
            )
        } else {
            "No Whisper model is installed (Settings → Take checking), so only the free length check runs.".to_string()
        };
        if ui
            .add_enabled(self.view.has_audio(), egui::Button::new("Check chapter"))
            .on_hover_text(hover)
            .clicked()
        {
            self.worker
                .send(Command::Check(CheckCommand::Chapter { chapter_id }));
        }
    }

    fn diff_for(
        &mut self,
        seg: &Segment,
        c: &TakeCheck,
        tts_model: &str,
        language: &str,
    ) -> Option<Rc<text::Check>> {
        let heard = c.transcript.as_deref()?;
        let key = (c.take_id, heard.to_string());
        if let Some((k, d)) = &self.check.diff
            && *k == key
        {
            return Some(d.clone());
        }
        let l = text::Lang::from_code(language);
        let tags = seg
            .performance
            .as_deref()
            .filter(|_| lang::model_supports_tags(tts_model))
            .map(|p| text::tag_phrases(p, l))
            .unwrap_or_default();
        let d = Rc::new(text::check(
            &seg.text,
            heard,
            l,
            &tags,
            &text::Thresholds::default(),
        ));
        self.check.diff = Some((key, d.clone()));
        Some(d)
    }

    /// Detail panel: the current take's check.
    pub(super) fn check_section(
        &mut self,
        ui: &mut egui::Ui,
        seg: &Segment,
        locked: bool,
        i: usize,
    ) {
        if seg.current_take_id.is_none() {
            return;
        }
        let p = *theme::palette(ui);
        let Some(project) = self.project.clone() else {
            return;
        };
        let result = self.current_check(seg).cloned();
        ui.add_space(8.0);
        let mut recheck = false;
        let mut dismiss = false;
        let mut reroll = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("TAKE CHECK").small().strong().color(p.muted));
            match &result {
                None => {
                    ui.label(RichText::new("Not checked yet.").color(p.muted));
                }
                Some(c) => {
                    let (label, color) = match c.status {
                        CheckStatus::Flagged => ("⚠ Flagged for a listen", p.warning),
                        CheckStatus::Ok if c.wer.is_some_and(|w| w > 0.0) => {
                            ("✓ Minor differences", p.approved)
                        }
                        CheckStatus::Ok => ("✓ Sounds like the text", p.approved),
                        CheckStatus::Dismissed => ("Flag dismissed", p.muted),
                        CheckStatus::Error => ("Check failed", p.rejected),
                        CheckStatus::Skipped => ("Length checked only", p.muted),
                    };
                    super::chip(ui, label, color);
                    let mut meta = Vec::new();
                    if c.model.is_empty() {
                        meta.push("no transcription".to_string());
                    } else {
                        meta.push(format!("Whisper {}", c.model));
                    }
                    if let Some(w) = c.wer {
                        meta.push(format!("WER {:.0}%", w * 100.0));
                    }
                    meta.push(
                        c.checked_at
                            .replace('T', " ")
                            .trim_end_matches('Z')
                            .to_string(),
                    );
                    ui.label(RichText::new(meta.join(" · ")).small().color(p.muted));
                }
            }
        });
        if let Some(c) = &result {
            let mut differs = false;
            if let Some(d) = self.diff_for(seg, c, &project.tts_model, &project.language) {
                let heard = c.transcript.as_deref().unwrap_or_default();
                // An exact match would only repeat the text above.
                differs = d.ops.iter().any(|o| !matches!(o, text::Op::Match(..)));
                if differs {
                    let mut job = diff_job(ui, &seg.text, heard, &d, &p);
                    job.wrap.max_width = ui.available_width();
                    ui.add(egui::Label::new(job).wrap());
                }
                let shown = if heard.trim().is_empty() {
                    "Whisper heard nothing.".to_string()
                } else {
                    format!("Whisper heard: “{}”", heard.trim())
                };
                ui.add(egui::Label::new(RichText::new(shown).small().color(p.muted)).wrap());
                if d.ops.iter().any(|o| o.is_error()) {
                    ui.label(
                        RichText::new("Struck through: in the text but not heard. Highlighted: heard but not in the text.")
                            .small()
                            .color(p.muted),
                    );
                }
            }
            // A dismissed flag keeps its findings, without the warning.
            let dismissed = c.status == CheckStatus::Dismissed;
            for issue in &c.issues {
                if issue.kind == IssueKind::Words && !issue.flag && !differs {
                    continue; // "the words match": the chip says it
                }
                let flag = issue.flag && !dismissed;
                let color = if flag {
                    p.warning
                } else if issue.kind == IssueKind::Error {
                    p.rejected
                } else {
                    p.muted
                };
                let mark = if flag { "⚠ " } else { "· " };
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{mark}{}", issue.message))
                            .small()
                            .color(color),
                    )
                    .wrap(),
                );
            }
        }
        ui.horizontal_wrapped(|ui| {
            recheck = ui
                .add(egui::Button::new(if result.is_some() { "Re-check" } else { "Check now" }).small())
                .on_hover_text("Check this take again, ahead of the queue (free)")
                .clicked();
            if result.as_ref().is_some_and(|c| c.is_flagged()) {
                dismiss = ui
                    .add(egui::Button::new("Dismiss flag").small())
                    .on_hover_text("The take is fine: remove the flag")
                    .clicked();
            }
            reroll = ui
                .add_enabled(
                    !locked && !self.in_flight.contains(&seg.id),
                    egui::Button::new("↻ Re-roll").small(),
                )
                .on_hover_text("Reject this take and generate a new one with a new seed (costs credits, as R does)")
                .clicked();
            let hint = if locked {
                "Flags never block approval. Unapprove to re-roll."
            } else {
                "Flags never block approval."
            };
            ui.label(RichText::new(hint).small().color(p.muted));
        });
        if recheck {
            self.worker
                .send(Command::Check(CheckCommand::Segment { segment_id: seg.id }));
            self.note(false, "Take check queued.".into());
        }
        if dismiss && let Some(c) = &result {
            let r = self.db.dismiss_take_check(c.take_id);
            self.try_db(r);
            self.reload_checks();
        }
        if reroll {
            self.reject(i);
        }
    }
}

/// The segment's text with the differences marked: words not heard are
/// struck through in red, words heard but not in the text are inserted in
/// the tag colour (fillers in muted italics).
fn diff_job(ui: &egui::Ui, expected: &str, heard: &str, c: &text::Check, p: &Palette) -> LayoutJob {
    let size = 15.0;
    let normal = egui::TextFormat {
        font_id: egui::FontId::proportional(size),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let missing = egui::TextFormat {
        color: p.rejected,
        strikethrough: Stroke::new(1.5, p.rejected),
        ..normal.clone()
    };
    let near = egui::TextFormat {
        underline: Stroke::new(1.0, p.muted),
        ..normal.clone()
    };
    let extra = egui::TextFormat {
        color: p.tag,
        background: p.tag_bg,
        ..normal.clone()
    };
    let filler = egui::TextFormat {
        color: p.muted,
        italics: true,
        ..normal.clone()
    };
    let mut job = LayoutJob::default();
    let mut cursor = 0usize;
    let span = |i: usize| c.expected[i].span;
    let heard_word = |j: usize| {
        let (a, b) = c.heard[j].span;
        heard.get(a..b).unwrap_or(&c.heard[j].norm).to_string()
    };
    // Text up to the next expected token (or the end) from op `k` on.
    let next_start = |k: usize| {
        c.ops[k..]
            .iter()
            .find_map(|o| match *o {
                text::Op::Match(i, _)
                | text::Op::Near(i, _)
                | text::Op::MergeRef(i, _)
                | text::Op::MergeHyp(i, _)
                | text::Op::Sub(i, _)
                | text::Op::Del(i) => Some(span(i).0),
                text::Op::Ins { .. } => None,
            })
            .unwrap_or(expected.len())
    };
    let push = |job: &mut LayoutJob, s: &str, f: &egui::TextFormat| {
        if !s.is_empty() {
            job.append(s, 0.0, f.clone());
        }
    };
    for (k, op) in c.ops.iter().enumerate() {
        let word = |job: &mut LayoutJob, cursor: &mut usize, i: usize, f: &egui::TextFormat| {
            let (a, b) = span(i);
            if a >= *cursor {
                push(job, &expected[*cursor..a], &normal);
                push(job, &expected[a..b], f);
                *cursor = b;
            }
        };
        match *op {
            text::Op::Match(i, _) | text::Op::MergeHyp(i, _) => {
                word(&mut job, &mut cursor, i, &normal)
            }
            text::Op::Near(i, _) => word(&mut job, &mut cursor, i, &near),
            text::Op::MergeRef(i, _) => {
                word(&mut job, &mut cursor, i, &normal);
                word(&mut job, &mut cursor, i + 1, &normal);
            }
            text::Op::Del(i) => word(&mut job, &mut cursor, i, &missing),
            text::Op::Sub(i, j) => {
                word(&mut job, &mut cursor, i, &missing);
                push(&mut job, " ", &normal);
                push(&mut job, &heard_word(j), &extra);
            }
            text::Op::Ins {
                j,
                filler: is_filler,
            } => {
                let to = next_start(k).max(cursor);
                push(&mut job, &expected[cursor..to], &normal);
                cursor = to;
                // A space before the word, except right after an opening quote.
                let mut rev = job.text.chars().rev();
                let (last, before) = (rev.next(), rev.next());
                let opening = last.is_some_and(|ch| "\"'“„«‘‚(".contains(ch))
                    && before.is_none_or(char::is_whitespace);
                if last.is_some_and(|ch| !ch.is_whitespace()) && !opening {
                    push(&mut job, " ", &normal);
                }
                push(
                    &mut job,
                    &heard_word(j),
                    if is_filler { &filler } else { &extra },
                );
                if cursor < expected.len() {
                    push(&mut job, " ", &normal);
                }
            }
        }
    }
    push(&mut job, &expected[cursor.min(expected.len())..], &normal);
    job
}

/// Settings: the "Take checking" section. Returns a command to send now
/// (download, cancel, delete); the settings themselves apply on Save.
pub(super) fn settings_section(
    ui: &mut egui::Ui,
    cfg: &mut CheckConfig,
    state: &CheckState,
    lib: &Library,
) -> Option<CheckCommand> {
    let p = *theme::palette(ui);
    let mut cmd = None;
    ui.add_space(10.0);
    ui.label(
        RichText::new("TAKE CHECKING")
            .small()
            .strong()
            .color(p.muted),
    );
    ui.add(
        egui::Label::new(
            RichText::new(
                "Transcribes each take on this computer with Whisper and flags takes whose words differ from the text or whose length is unusual. It only flags: approving is never blocked, and it never costs credits. It runs in the background at low priority, one take at a time.",
            )
            .small()
            .color(p.muted),
        )
        .wrap(),
    );
    if !BUILT_WITH_WHISPER {
        ui.colored_label(
            p.warning,
            "Built without Whisper (the `whisper` feature): only the free length check runs.",
        );
    }
    ui.checkbox(&mut cfg.enabled, "Check new takes automatically");
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    egui::Grid::new("take_check").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Model");
        let selected = models::info(&cfg.model)
            .map(|m| format!("{} — {}", m.name, m.label))
            .unwrap_or_else(|| cfg.model.clone());
        egui::ComboBox::from_id_salt("check_model")
            .selected_text(selected)
            .width(260.0)
            .height(400.0)
            .show_ui(ui, |ui| {
                for m in models::MODELS {
                    let installed = models::installed(lib, m) != models::Installed::No;
                    let mark = if installed { "  ✓ installed" } else { "" };
                    ui.selectable_value(
                        &mut cfg.model,
                        m.name.to_string(),
                        format!("{} — {}{mark}\n{}", m.name, m.label, m.hint()),
                    )
                    .on_hover_text(m.note);
                }
            });
        ui.end_row();

        ui.label("");
        ui.vertical(|ui| {
            let Some(m) = models::info(&cfg.model) else {
                ui.colored_label(p.rejected, "Unknown model");
                return;
            };
            ui.label(RichText::new(format!("{} · {}", m.hint(), m.note)).small().color(p.muted));
            let dl = state.download.as_ref().filter(|d| d.model == m.name);
            let busy = state.download.as_ref().is_some_and(|d| d.running);
            ui.horizontal(|ui| {
                if let Some(d) = dl.filter(|d| d.running) {
                    let frac = d.total.map_or(0.0, |t| d.received as f32 / t.max(1) as f32);
                    let text = match d.total {
                        Some(t) => format!("{} of {}", mib(d.received), mib(t)),
                        None => mib(d.received),
                    };
                    ui.add(egui::ProgressBar::new(frac).desired_width(200.0).text(text));
                    if ui.button("Cancel").clicked() {
                        cmd = Some(CheckCommand::CancelDownload);
                    }
                    return;
                }
                match models::installed(lib, m) {
                    models::Installed::No => {
                        ui.label(RichText::new("Not installed").color(p.muted));
                        let label = format!(
                            "Download ({}{} MiB)",
                            if m.size_approx { "≈" } else { "" },
                            m.size_mib
                        );
                        if ui
                            .add_enabled(BUILT_WITH_WHISPER && !busy, theme::primary_button(ui, &label))
                            .on_hover_text("From huggingface.co/ggerganov/whisper.cpp; the checksum is verified before the file is kept. Set HF_TOKEN to send a Hugging Face token.")
                            .clicked()
                        {
                            cmd = Some(CheckCommand::Download {
                                model: m.name.to_string(),
                            });
                        }
                    }
                    installed => {
                        let text = if installed == models::Installed::Verified {
                            "✓ Installed, checksum verified"
                        } else if m.sha1.is_some() {
                            "✓ Installed (verified when first loaded)"
                        } else {
                            "✓ Installed (no published checksum)"
                        };
                        ui.label(RichText::new(text).color(p.approved));
                        if ui.add_enabled(!busy, egui::Button::new("Delete")).clicked() {
                            cmd = Some(CheckCommand::Delete {
                                model: m.name.to_string(),
                            });
                        }
                    }
                }
            });
            if let Some(e) = dl.and_then(|d| d.error.as_ref()) {
                ui.add(egui::Label::new(RichText::new(format!("Download failed: {e}")).small().color(p.rejected)).wrap());
            }
        });
        ui.end_row();

        ui.label("Threads");
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut cfg.threads).range(1..=cores.max(1)));
            ui.label(
                RichText::new(format!("of {cores} cores (default: half)"))
                    .small()
                    .color(p.muted),
            );
        });
        ui.end_row();
    });
    ui.label(
        RichText::new(format!(
            "Models are stored in {}.",
            models::models_dir(lib).display()
        ))
        .small()
        .color(p.muted),
    );
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_text(expected: &str, heard: &str) -> (String, Vec<(String, bool, bool)>) {
        let c = text::check(
            expected,
            heard,
            text::Lang::En,
            &[],
            &text::Thresholds::default(),
        );
        let ctx = egui::Context::default();
        let mut out = (String::new(), Vec::new());
        let mut full = ctx.run_ui(Default::default(), |ui| {
            let job = diff_job(ui, expected, heard, &c, &theme::LIGHT);
            out.0 = job.text.clone();
            out.1 = job
                .sections
                .iter()
                .map(|s| {
                    (
                        job.text[s.byte_range.start.0..s.byte_range.end.0].to_string(),
                        s.format.strikethrough.width > 0.0,
                        s.format.color == theme::LIGHT.tag,
                    )
                })
                .collect();
        });
        full.textures_delta.clear();
        out
    }

    #[test]
    fn diff_marks_missing_and_extra_words() {
        let (text, sections) = job_text(
            "Close the old door, please.",
            " Close the door now, please.",
        );
        assert_eq!(text, "Close the old door, now please.");
        let struck: Vec<&str> = sections
            .iter()
            .filter(|s| s.1)
            .map(|s| s.0.as_str())
            .collect();
        let extra: Vec<&str> = sections
            .iter()
            .filter(|s| s.2)
            .map(|s| s.0.as_str())
            .collect();
        assert_eq!((struck, extra), (vec!["old"], vec!["now"]));
        // A wrong word shows both; an extra word at the end goes after it.
        let (text, _) = job_text("The river rose.", " The liver rose again.");
        assert_eq!(text, "The river liver rose. again");
        // Extra words at the start go inside the opening quote, unspaced.
        let (text, _) = job_text("\"Go now.\"", " Well, go now.");
        assert_eq!(text, "\"Well Go now.\"");
        // A repeated phrase is marked on its second copy.
        let (text, _) = job_text(
            "\"Then we lose the signal.\"",
            " Then we lose the signal. Then we lose the signal.",
        );
        assert_eq!(text, "\"Then we lose the signal.\" Then we lose the signal");
        // A perfect match is the text itself.
        assert_eq!(
            job_text("“Fine,” he said.", " Fine, he said.").0,
            "“Fine,” he said."
        );
    }
}
