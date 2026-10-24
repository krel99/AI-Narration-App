//! Retail export in the UI: the Publication section of Project settings
//! (metadata and cover checks), the ACX chip in the Review toolbar, the
//! "Retail & ACX" window with every chapter's measurements, and the retail
//! sample dialog.

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui::{self, Color32, RichText};

use super::{App, chip};
use crate::acx::{self, Check, FileReport, Verdict};
use crate::export;
use crate::model::*;
use crate::retail::{self, CoverInfo, Publication, RetailState, SampleSource, SampleStop};
use crate::theme::{self, Palette, primary_button};
use crate::worker::Command;

#[derive(Default)]
pub(super) struct RetailUi {
    /// The Retail & ACX window is open.
    open: bool,
    /// Stored checks of the current project.
    state: RetailState,
    state_for: Option<Id>,
    sample: Option<SampleDialog>,
    /// The last cover inspected, by resolved path (covers in the library
    /// are named by content, so a new picture gets a new path).
    cover: Option<(PathBuf, Result<CoverInfo, String>)>,
    /// Silence trims of the open chapter's takes, for its report's
    /// fingerprint (refreshed with the view and on every trim edit).
    pub(super) trims: HashMap<Id, (u32, u32)>,
}

struct SampleDialog {
    chapter_id: Id,
    chapter_title: String,
    /// The Review selection when the dialog opened: id, number, text.
    selected: Option<(Id, usize, String)>,
    from_selected: bool,
    minutes: f32,
    sources: Result<Vec<SampleSource>, String>,
}

/// Width of the chapter titles in the Retail & ACX grid.
const CHAPTER_COL_W: f32 = 190.0;

pub(super) fn verdict_color(p: &Palette, v: Verdict) -> Color32 {
    match v {
        Verdict::Pass => p.approved,
        Verdict::Warn => p.warning,
        Verdict::Fail => p.rejected,
    }
}

fn checks_tooltip(r: &FileReport, stale: bool) -> String {
    let mut s = format!(
        "{} · {}{}\n",
        acx::SPEC_NAME,
        r.file,
        if r.draft { " (draft export)" } else { "" }
    );
    for c in &r.checks {
        s.push_str(&format!(
            "\n{} {} {} — {}",
            c.verdict.symbol(),
            c.label,
            c.value,
            c.note
        ));
    }
    s.push_str(&format!(
        "\n\nMeasured {}.",
        r.checked_at.replace('T', " ").replace('Z', " UTC")
    ));
    if stale {
        s.push_str(" Takes or approvals changed since: export again to re-check.");
    }
    s.push_str("\nClick for every chapter.");
    s
}

impl App {
    fn cover_info(&mut self, publication: &Publication) -> Option<Result<CoverInfo, String>> {
        let path = retail::cover_file(&self.lib.root, &publication.cover_path)?;
        Some(cached_cover(&mut self.retail.cover, path))
    }

    /// Reload the stored checks (after exports, on project changes).
    pub(super) fn reload_retail(&mut self) {
        let Some(pid) = self.project.as_ref().map(|p| p.id) else {
            self.retail.state = RetailState::default();
            self.retail.state_for = None;
            return;
        };
        match RetailState::load(&self.db, pid) {
            Ok(s) => {
                self.retail.state = s;
                self.retail.state_for = Some(pid);
            }
            Err(e) => log::warn!("loading export reports: {e:#}"),
        }
    }

    /// The open chapter's take trims, for its report's fingerprint.
    pub(super) fn reload_trims(&mut self) {
        self.retail.trims = match self.view.chapter_id {
            Some(cid) => self.db.chapter_trims(cid).unwrap_or_else(|e| {
                log::warn!("loading trims: {e:#}");
                HashMap::new()
            }),
            None => HashMap::new(),
        };
    }

    /// A take's trim was edited in the waveform.
    pub(super) fn trim_changed(&mut self, take_id: Id, trim: Option<(u32, u32)>) {
        match trim {
            Some(t) => self.retail.trims.insert(take_id, t),
            None => self.retail.trims.remove(&take_id),
        };
    }

    /// The chapter's segments, takes, approvals or trims changed since its report.
    /// The open chapter is compared with the segments on screen, so the chip
    /// follows an approval right away; other chapters use the stored check.
    fn report_stale(&self, chapter_id: Id, r: &FileReport) -> bool {
        match &r.takes_key {
            Some(key) if self.view.chapter_id == Some(chapter_id) => {
                *key != retail::segments_key(&self.view.segments, &self.retail.trims)
            }
            _ => self.retail.state.stale.contains(&chapter_id),
        }
    }

    pub(super) fn retail_dialog_open(&self) -> bool {
        self.retail.sample.is_some()
    }

    pub(super) fn on_acx_checked(&mut self, summary: String, verdict: Verdict) {
        self.reload_retail();
        self.note(verdict == Verdict::Fail, summary);
    }

    /// Review toolbar: "ACX ✓" / "ACX ⚠ Noise floor -inf" / "ACX ✗ RMS -24.1"
    /// for the open chapter's latest export.
    pub(super) fn acx_chip(&mut self, ui: &mut egui::Ui) {
        let Some(cid) = self.view.chapter_id else {
            return;
        };
        if self.retail.state_for != self.project.as_ref().map(|p| p.id) {
            return;
        }
        let Some(r) = self.retail.state.reports.get(&cid) else {
            return;
        };
        let p = *theme::palette(ui);
        let stale = self.report_stale(cid, r);
        let mut text = r.chip_text();
        if r.draft {
            text.push_str(" · draft");
        }
        if stale {
            text.push_str(" · outdated");
        }
        let color = if stale {
            p.muted
        } else {
            verdict_color(&p, r.verdict())
        };
        let resp = chip(ui, text, color);
        let resp = ui
            .interact(resp.rect, resp.id.with("acx"), egui::Sense::click())
            .on_hover_text(checks_tooltip(r, stale));
        if resp.clicked() {
            self.open_retail();
        }
    }

    pub(super) fn open_retail(&mut self) {
        self.reload_retail();
        self.retail.open = true;
    }

    /// The ⋯ menu entry.
    pub(super) fn retail_menu_item(&mut self, ui: &mut egui::Ui) {
        if ui
            .button("Retail & ACX…")
            .on_hover_text(
                "ACX checks of every exported chapter, the retail sample, the export folder",
            )
            .clicked()
        {
            self.open_retail();
            ui.close();
        }
    }

    // ---- the window -------------------------------------------------------------

    pub(super) fn dialog_retail(&mut self, ctx: &egui::Context) {
        self.dialog_sample(ctx);
        if !self.retail.open {
            return;
        }
        let Some(project) = self.project.clone() else {
            self.retail.open = false;
            return;
        };
        if self.retail.state_for != Some(project.id) {
            self.reload_retail();
        }
        let cover = self.cover_info(&project.settings.publication);
        self.retail.state.cover = cover;
        let chapters = self.chapters.clone();
        let book_checks = self.retail.state.book_checks(&project, &chapters);
        let mut open = true;
        let mut open_folder = false;
        let mut sample = false;
        let can_sample =
            self.view.chapter_id.is_some() && !self.view.segments.is_empty() && !self.exporting;
        egui::Window::new("Retail & ACX")
            .open(&mut open)
            .default_width(1000.0)
            .default_pos([160.0, 90.0])
            .show(ctx, |ui| {
                let p = *theme::palette(ui);
                ui.label(
                    RichText::new(project.settings.publication.book_title(&project.name))
                        .strong()
                        .size(16.0),
                );
                ui.label(
                    RichText::new(format!(
                        "{}, measured on each chapter's exported MP3. Hover a value for its rule.",
                        acx::SPEC_NAME
                    ))
                    .color(p.muted),
                );
                ui.add_space(4.0);
                egui::Frame::new()
                    .fill(p.accent_soft)
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(acx::POLICY_NOTE).small()).wrap());
                    });
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| self.chapter_grid(ui, &chapters));
                ui.add_space(8.0);
                ui.label(RichText::new("BOOK").small().strong().color(p.muted));
                for c in &book_checks {
                    check_line(ui, &p, c);
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("RETAIL SAMPLE")
                        .small()
                        .strong()
                        .color(p.muted),
                );
                match &self.retail.state.sample {
                    None => {
                        ui.label(RichText::new("None yet.").color(p.muted));
                    }
                    Some((r, _)) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(&r.file).strong());
                            for key in ["length", "rms", "peak", "floor"] {
                                if let Some(c) = r.check(key) {
                                    ui.label(
                                        RichText::new(format!("{} {}", c.label, c.value))
                                            .color(verdict_color(&p, c.verdict)),
                                    )
                                    .on_hover_text(&c.note);
                                }
                            }
                            let v = r.verdict();
                            ui.label(
                                RichText::new(format!("{} {}", v.symbol(), v.word()))
                                    .color(verdict_color(&p, v)),
                            )
                            .on_hover_text(checks_tooltip(r, false));
                        });
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    open_folder = ui.button("Open export folder").clicked();
                    sample = ui
                        .add_enabled(can_sample, primary_button(ui, "Export retail sample…"))
                        .on_disabled_hover_text("Open a chapter with segments first")
                        .clicked();
                });
                ui.label(
                    RichText::new(format!(
                        "{} in the export folder is rewritten after every export.",
                        retail::REPORT_FILE
                    ))
                    .small()
                    .color(p.muted),
                );
            });
        self.retail.open = open;
        if open_folder {
            self.open_export_folder(&project);
        }
        if sample {
            self.open_sample_dialog();
        }
    }

    fn chapter_grid(&self, ui: &mut egui::Ui, chapters: &[Chapter]) {
        let p = *theme::palette(ui);
        egui::Grid::new("acx_grid")
            .striped(true)
            .num_columns(10)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                for h in [
                    "#", "Chapter", "Length", "RMS", "Peak", "Floor", "Head", "Tail", "Format",
                    "Result",
                ] {
                    ui.label(RichText::new(h).small().strong().color(p.muted));
                }
                ui.end_row();
                for (i, c) in chapters.iter().enumerate() {
                    ui.label(RichText::new((i + 1).to_string()).color(p.muted));
                    ui.allocate_ui_with_layout(
                        egui::Vec2::new(CHAPTER_COL_W, 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_min_width(CHAPTER_COL_W);
                            ui.add(egui::Label::new(&c.title).truncate())
                                .on_hover_text(&c.title);
                        },
                    );
                    let Some(r) = self.retail.state.reports.get(&c.id) else {
                        ui.label(RichText::new("not exported").color(p.muted));
                        for _ in 0..7 {
                            ui.label("");
                        }
                        ui.end_row();
                        continue;
                    };
                    for key in ["length", "rms", "peak", "floor", "head", "tail"] {
                        match r.check(key) {
                            Some(c) => {
                                ui.label(
                                    RichText::new(&c.value).color(verdict_color(&p, c.verdict)),
                                )
                                .on_hover_text(format!(
                                    "{} {}: {}",
                                    c.verdict.symbol(),
                                    c.label,
                                    c.note
                                ));
                            }
                            None => {
                                ui.label("–");
                            }
                        }
                    }
                    let (format, fv) = r.format_summary();
                    let format_notes: Vec<String> = r
                        .checks
                        .iter()
                        .filter(|c| {
                            ["codec", "rate", "bitrate", "cbr", "channels", "size"]
                                .contains(&c.key.as_str())
                        })
                        .map(|c| {
                            format!(
                                "{} {} {} — {}",
                                c.verdict.symbol(),
                                c.label,
                                c.value,
                                c.note
                            )
                        })
                        .collect();
                    ui.label(RichText::new(format).color(verdict_color(&p, fv)))
                        .on_hover_text(format_notes.join("\n"));
                    let stale = self.report_stale(c.id, r);
                    let v = r.verdict();
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{} {}", v.symbol(), v.word()))
                                .strong()
                                .color(verdict_color(&p, v)),
                        )
                        .on_hover_text(checks_tooltip(r, stale));
                        if r.draft {
                            ui.label(RichText::new("draft").small().color(p.muted));
                        }
                        if stale {
                            ui.label(RichText::new("outdated").small().color(p.warning))
                                .on_hover_text(
                                    "Takes or approvals changed after this export; export again to re-check.",
                                );
                        }
                    });
                    ui.end_row();
                }
            });
    }

    fn open_export_folder(&mut self, project: &Project) {
        let dir = match export::export_dir(&self.lib, project) {
            Ok(d) => d,
            Err(e) => return self.note(true, format!("{e:#}")),
        };
        match std::process::Command::new("xdg-open").arg(&dir).spawn() {
            // Reap it off the UI thread, or every click leaves a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => self.note(true, format!("Could not open {} ({e}).", dir.display())),
        }
    }

    // ---- retail sample ------------------------------------------------------------

    fn open_sample_dialog(&mut self) {
        let Some(cid) = self.view.chapter_id else {
            return;
        };
        let selected = self.selected.and_then(|i| {
            self.view.segments.get(i).map(|s| {
                let text: String = s.text.chars().take(60).collect();
                let more = if s.text.chars().count() > 60 {
                    "…"
                } else {
                    ""
                };
                (s.id, i + 1, format!("{text}{more}"))
            })
        });
        let sources =
            export::sample_sources(&self.db, &self.lib, cid).map_err(|e| format!("{e:#}"));
        self.retail.sample = Some(SampleDialog {
            chapter_id: cid,
            chapter_title: self.chapter_title(cid),
            from_selected: selected.is_some(),
            selected,
            minutes: (retail::SAMPLE_DEFAULT_MS / 60_000) as f32,
            sources,
        });
    }

    fn dialog_sample(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.retail.sample else {
            return;
        };
        let Some(project) = self.project.clone() else {
            self.retail.sample = None;
            return;
        };
        let mut go = false;
        let mut cancel = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let exporting = self.exporting;
        egui::Window::new("Retail sample")
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 110.0])
            .show(ctx, |ui| {
                let p = *theme::palette(ui);
                ui.set_max_width(460.0);
                ui.label(format!("From \"{}\"", d.chapter_title));
                ui.add_space(4.0);
                match &d.selected {
                    Some((_, n, text)) => {
                        ui.radio_value(&mut d.from_selected, true, format!("Selected segment {n}: {text}"));
                    }
                    None => {
                        ui.add_enabled(false, egui::RadioButton::new(false, "Selected segment (none selected in Review)"));
                    }
                }
                ui.radio_value(&mut d.from_selected, false, "Chapter start");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Length up to");
                    ui.add(
                        egui::Slider::new(&mut d.minutes, 1.0..=5.0)
                            .step_by(0.5)
                            .fixed_decimals(1)
                            .suffix(" min"),
                    );
                });
                ui.add_space(4.0);
                let start_id = d.selected.as_ref().filter(|_| d.from_selected).map(|s| s.0);
                let max_ms = (d.minutes as f64 * 60_000.0) as u64;
                let plan = match &d.sources {
                    Err(e) => Err(e.clone()),
                    Ok(src) => {
                        let start = start_id
                            .and_then(|id| src.iter().position(|s| s.segment_id == id))
                            .unwrap_or(0);
                        retail::plan_sample(src, start, max_ms, &project.settings.silence)
                            .map(|pl| {
                                // Review numbers segments only, not placeholders.
                                let n = |id| {
                                    src.iter()
                                        .filter(|s| !s.unsegmented)
                                        .position(|s| s.segment_id == id)
                                        .map_or(0, |i| i + 1)
                                };
                                let why = match pl.stop {
                                    SampleStop::Unapproved(id) => Some(format!(
                                        "⚠ Segment {} is not approved yet: the sample stops before it (approved takes only).",
                                        n(id)
                                    )),
                                    SampleStop::Unsegmented(k) => Some(format!(
                                        "⚠ Paragraph {k} has no segments yet: the sample stops before it (approved takes only)."
                                    )),
                                    _ => None,
                                };
                                (pl, why)
                            })
                    }
                };
                match &plan {
                    Ok((pl, why)) => {
                        ui.label(RichText::new(format!("≈ {}", pl.describe())).strong());
                        if let Some(why) = why {
                            ui.add(
                                egui::Label::new(RichText::new(why).color(p.warning)).wrap(),
                            );
                        }
                        if pl.total_ms() < retail::SAMPLE_MIN_MS {
                            ui.label(
                                RichText::new("⚠ Shorter than a minute.").color(p.warning),
                            );
                        }
                    }
                    Err(e) => {
                        ui.add(egui::Label::new(RichText::new(format!("⚠ {e}")).color(p.warning)).wrap());
                    }
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "Approved takes only, fade in 0.5 s and out 3 s, mastered like the chapters; MP3 192 kbps with tags: \"{}\" in the export folder.",
                            export::sample_file_name(&project)
                        ))
                        .small()
                        .color(p.muted),
                    )
                    .wrap(),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "ACX: up to 5 minutes from the audiobook itself (not the credits), nothing explicit. ACX recommends the opening, since most listeners hear the beginning anyway.",
                        )
                        .small()
                        .color(p.muted),
                    )
                    .wrap(),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    go = ui
                        .add_enabled(plan.is_ok() && !exporting, primary_button(ui, "Export sample"))
                        .clicked();
                    cancel |= ui.button("Cancel").clicked();
                });
            });
        if go {
            let d = self.retail.sample.take().expect("dialog open");
            let start_segment = d.selected.filter(|_| d.from_selected).map(|s| s.0);
            self.exporting = true;
            self.worker.send(Command::ExportSample {
                chapter_id: d.chapter_id,
                start_segment,
                max_ms: (d.minutes as f64 * 60_000.0) as u64,
            });
        } else if cancel {
            self.retail.sample = None;
        }
    }

    // ---- project settings -----------------------------------------------------------

    /// Copy a newly chosen cover into the library before saving the project.
    pub(super) fn store_cover(&mut self, project: &mut Project) {
        let path = project.settings.publication.cover_path.trim().to_string();
        if path.is_empty() || !std::path::Path::new(&path).is_absolute() {
            return;
        }
        match retail::import_cover(&self.lib.root, project.id, std::path::Path::new(&path)) {
            Ok(rel) => project.settings.publication.cover_path = rel,
            Err(e) => self.note(
                true,
                format!("Could not copy the cover into the library ({e:#}); keeping {path}."),
            ),
        }
    }
}

fn cached_cover(
    cache: &mut Option<(PathBuf, Result<CoverInfo, String>)>,
    path: PathBuf,
) -> Result<CoverInfo, String> {
    if let Some((p, r)) = cache
        && *p == path
    {
        return r.clone();
    }
    let r = retail::inspect_cover(&path).map_err(|e| format!("{e:#}"));
    *cache = Some((path, r.clone()));
    r
}

fn check_line(ui: &mut egui::Ui, p: &Palette, c: &Check) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(c.verdict.symbol()).color(verdict_color(p, c.verdict)));
        ui.label(RichText::new(&c.label).strong());
        ui.label(&c.value);
        ui.label(RichText::new(&c.note).small().color(p.muted));
    });
}

/// The Publication section of Project settings.
pub(super) fn publication_ui(
    ui: &mut egui::Ui,
    project: &mut Project,
    library_root: &std::path::Path,
    state: &mut RetailUi,
) {
    let cover_cache = &mut state.cover;
    let p = *theme::palette(ui);
    ui.add_space(8.0);
    ui.label(RichText::new("PUBLICATION").small().strong().color(p.muted));
    let name = project.name.clone();
    let pb = &mut project.settings.publication;
    egui::Grid::new("publication")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (label, value, hint) in [
                ("Title", &mut pb.title, name.as_str()),
                ("Subtitle", &mut pb.subtitle, ""),
                ("Author", &mut pb.author, "Jane Author"),
                ("Narrator", &mut pb.narrator, "as credited"),
                ("Publisher", &mut pb.publisher, ""),
                ("Year", &mut pb.year, "2026"),
                ("Genre", &mut pb.genre, "Audiobook"),
                ("Copyright", &mut pb.copyright, "© 2026 Jane Author"),
            ] {
                ui.label(label);
                ui.add(
                    egui::TextEdit::singleline(value)
                        .hint_text(hint)
                        .desired_width(300.0),
                );
                ui.end_row();
            }
            ui.label("Description");
            ui.add(
                egui::TextEdit::multiline(&mut pb.description)
                    .desired_rows(2)
                    .desired_width(300.0),
            );
            ui.end_row();
            ui.label("Cover");
            ui.horizontal(|ui| {
                let shown = if pb.cover_path.trim().is_empty() {
                    "none".to_string()
                } else {
                    std::path::Path::new(pb.cover_path.trim())
                        .file_name()
                        .map_or(pb.cover_path.clone(), |n| n.to_string_lossy().to_string())
                };
                ui.add(egui::Label::new(RichText::new(shown).color(p.muted)).truncate())
                    .on_hover_text(&pb.cover_path);
                if ui.button("Choose…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "Cover art (JPG, PNG)",
                            &["jpg", "jpeg", "png", "JPG", "JPEG", "PNG"],
                        )
                        .pick_file()
                {
                    pb.cover_path = path.display().to_string();
                }
                if !pb.cover_path.is_empty() && ui.button("Remove").clicked() {
                    pb.cover_path.clear();
                }
            });
            ui.end_row();
        });
    if let Some(path) = retail::cover_file(library_root, &pb.cover_path) {
        let text = match cached_cover(cover_cache, path) {
            Ok(info) => {
                let problems = info.problems();
                if problems.is_empty() {
                    RichText::new(format!("✓ {}: meets ACX's cover rules", info.describe()))
                        .color(p.approved)
                } else {
                    RichText::new(format!(
                        "⚠ {}: {} (ACX wants a square JPG/PNG, at least 2400×2400, RGB, at most 8 MB)",
                        info.describe(),
                        problems.join(", ")
                    ))
                    .color(p.warning)
                }
            }
            Err(e) => RichText::new(format!("⚠ {e}")).color(p.warning),
        };
        ui.add(egui::Label::new(text.small()).wrap());
    }
    ui.checkbox(&mut pb.cover_in_mp3, "Embed the cover in the chapter MP3s")
        .on_hover_text("The FLAC masters and the M4B always carry it. Whether ACX's and Findaway's uploaders accept embedded art is not documented; turn this off if one complains.");
    ui.checkbox(
        &mut pb.room_tone,
        "Room tone: a pink-noise bed at about −73 dB instead of digital silence",
    )
    .on_hover_text("ACX Check flags pauses below −90 dB as dead silence.");
    for h in pb.hints() {
        ui.label(RichText::new(format!("⚠ {h}")).small().color(p.warning));
    }
    ui.add(
        egui::Label::new(
            RichText::new(
                "Exports are mastered to −20 dB RMS under a −4 dB limiter and checked against the ACX/Findaway spec (Retail & ACX in the ⋯ menu). MP3s get the chapter title, book title, author, narrator, track and cover; the M4B adds the description.",
            )
            .small()
            .color(p.muted),
        )
        .wrap(),
    );
}
