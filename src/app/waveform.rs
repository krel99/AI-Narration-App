//! Silence trimming in the UI: the take waveform in the Review detail panel
//! (trimmed regions shaded, draggable start/end handles, playhead), the
//! "Auto-trim chapter" toolbar button and the project settings section.
//!
//! A trim is metadata on the take (see `crate::trim`): editing it never
//! touches the WAV and costs nothing.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
};

use super::App;
use crate::audio::{self, Pcm};
use crate::model::*;
use crate::theme;
use crate::trim::{self, TrimSettings};
use crate::worker::Command;

const WAVE_H: f32 = 70.0;
/// Peaks computed per take; columns are folded from these when drawing.
const PEAK_BUCKETS: usize = 2048;
/// Shortest range a manual trim may keep.
const MIN_KEEP_MS: u32 = 20;
/// Cached waveforms before the cache starts over.
const CACHE_LIMIT: usize = 256;

/// Waveform state kept on the App.
#[derive(Default)]
pub(super) struct WaveState {
    /// Peaks per audio file. A take's WAV never changes, so they are
    /// computed once (takes that share a cached file share the entry).
    cache: HashMap<String, Rc<Wave>>,
    drag: Option<Drag>,
    /// The take queued by the last single-take play; `None` after a
    /// paragraph play, which plays each segment's current take.
    pub(super) playing_take: Option<Id>,
}

struct Wave {
    peaks: Vec<(i16, i16)>,
    duration_ms: u32,
    error: Option<String>,
}

#[derive(Clone, Copy)]
struct Drag {
    take_id: Id,
    /// Dragging the end handle (otherwise the start handle).
    end: bool,
    start_ms: u32,
    end_ms: u32,
}

impl App {
    /// A take's audio as it is played and exported: trimmed when it has a trim.
    pub(super) fn take_audio(&self, take: &Take) -> anyhow::Result<Pcm> {
        let default = TrimSettings::default();
        let settings = self.project.as_ref().map_or(&default, |p| &p.settings.trim);
        trim::load_take(&self.lib, take, settings)
    }

    fn wave_for(&mut self, take: &Take) -> Rc<Wave> {
        if let Some(w) = self.wave.cache.get(&take.audio_path) {
            return w.clone();
        }
        let wave = match audio::read_wav(&self.lib.audio_path(&take.audio_path)) {
            Ok(pcm) => Wave {
                peaks: trim::peaks(&pcm, PEAK_BUCKETS),
                duration_ms: trim::duration_ms(&pcm),
                error: None,
            },
            Err(e) => Wave {
                peaks: Vec::new(),
                duration_ms: 0,
                error: Some(format!("{e:#}")),
            },
        };
        if self.wave.cache.len() >= CACHE_LIMIT {
            self.wave.cache.clear();
        }
        let wave = Rc::new(wave);
        self.wave
            .cache
            .insert(take.audio_path.clone(), wave.clone());
        wave
    }

    /// Save a take's trim and show it right away (the take list is the
    /// selected segment's, so it is updated in place instead of reloaded).
    fn store_trim(&mut self, ctx: &egui::Context, take_id: Id, trim: Option<(u32, u32)>) {
        let r = self.db.set_take_trim(take_id, trim);
        if self.try_db(r).is_some()
            && let Some(t) = self.takes.iter_mut().find(|t| t.id == take_id)
        {
            t.trim_ms = trim;
        }
        ctx.request_repaint();
    }

    /// The current take's waveform with its trim, under the segment's actions.
    pub(super) fn waveform_section(&mut self, ui: &mut egui::Ui, seg: &Segment, locked: bool) {
        let Some(take) = seg
            .current_take_id
            .and_then(|id| self.takes.iter().find(|t| t.id == id))
            .cloned()
        else {
            return;
        };
        let p = *theme::palette(ui);
        let wave = self.wave_for(&take);
        ui.add_space(6.0);
        if let Some(e) = &wave.error {
            ui.colored_label(p.rejected, format!("Waveform unavailable: {e}"));
            return;
        }
        let dur = wave.duration_ms.max(1);
        let editable = !locked;
        if self.wave.drag.is_some_and(|d| d.take_id != take.id) {
            self.wave.drag = None;
        }

        // Stay clear of the panel's (floating) scroll bar at the right edge,
        // or grabbing the end handle would grab the scroll bar.
        let bar = {
            let s = &ui.spacing().scroll;
            s.bar_inner_margin + s.bar_width + s.bar_outer_margin
        };
        let width = (ui.available_width() - bar).max(120.0);
        let sense = if editable {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, WAVE_H), sense);
        let x_of = |ms: u32| rect.left() + rect.width() * (ms.min(dur) as f32 / dur as f32);
        let ms_of = |x: f32| {
            (((x - rect.left()) / rect.width()).clamp(0.0, 1.0) * dur as f32).round() as u32
        };

        // Dragging a handle: the nearest one moves, committed on release.
        let (stored_start, stored_end) = take.trim_ms.unwrap_or((0, dur));
        if editable {
            if resp.drag_started()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                let end = (pos.x - x_of(stored_end)).abs() < (pos.x - x_of(stored_start)).abs();
                self.wave.drag = Some(Drag {
                    take_id: take.id,
                    end,
                    start_ms: stored_start,
                    end_ms: stored_end.min(dur),
                });
            }
            if let Some(d) = &mut self.wave.drag {
                if resp.dragged()
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let ms = ms_of(pos.x);
                    if d.end {
                        d.end_ms = ms.clamp((d.start_ms + MIN_KEEP_MS).min(dur), dur);
                    } else {
                        d.start_ms = ms.min(d.end_ms.saturating_sub(MIN_KEEP_MS));
                    }
                }
                if resp.drag_stopped() {
                    let d = *d;
                    self.wave.drag = None;
                    resp.surrender_focus();
                    let whole = d.start_ms == 0 && d.end_ms >= dur;
                    let new = (!whole).then_some((d.start_ms, d.end_ms));
                    if new != take.trim_ms {
                        self.store_trim(ui.ctx(), take.id, new);
                    }
                }
            }
        }
        let (start, end) = match self.wave.drag {
            Some(d) => (d.start_ms, d.end_ms),
            None => (stored_start, stored_end.min(dur)),
        };
        let (xs, xe) = (x_of(start), x_of(end));

        // Background and centre line.
        let dark = ui.visuals().dark_mode;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 6.0, ui.visuals().faint_bg_color);
        let mid = rect.center().y;
        let half = rect.height() / 2.0 - 4.0;
        painter.hline(rect.x_range(), mid, Stroke::new(1.0, p.border));

        // One vertical stroke per pixel column, folded from the peaks.
        let cols = rect.width().round().max(1.0) as usize;
        let n = wave.peaks.len();
        if n > 0 {
            for c in 0..cols {
                let b0 = c * n / cols;
                let b1 = ((c + 1) * n / cols).clamp(b0 + 1, n);
                let (lo, hi) = wave.peaks[b0..b1]
                    .iter()
                    .fold((i16::MAX, i16::MIN), |(lo, hi), &(a, b)| {
                        (lo.min(a), hi.max(b))
                    });
                let x = rect.left() + c as f32 + 0.5;
                let top = mid - hi as f32 / 32768.0 * half;
                let bottom = (mid - lo as f32 / 32768.0 * half).max(top + 1.0);
                let kept = x >= xs && x <= xe;
                let color = if kept { p.generated } else { p.draft };
                painter.vline(x, top..=bottom, Stroke::new(1.0, color));
            }
        }

        // Shade what is cut off, then the handles.
        let shade = if dark {
            Color32::from_black_alpha(110)
        } else {
            Color32::from_black_alpha(26)
        };
        if xs > rect.left() {
            painter.rect_filled(
                Rect::from_x_y_ranges(rect.left()..=xs, rect.y_range()),
                egui::CornerRadius {
                    nw: 6,
                    sw: 6,
                    ..Default::default()
                },
                shade,
            );
        }
        if xe < rect.right() {
            painter.rect_filled(
                Rect::from_x_y_ranges(xe..=rect.right(), rect.y_range()),
                egui::CornerRadius {
                    ne: 6,
                    se: 6,
                    ..Default::default()
                },
                shade,
            );
        }
        let handle_color = if editable { p.accent } else { p.muted };
        for x in [xs, xe] {
            let x = x.clamp(rect.left() + 1.0, rect.right() - 1.0);
            painter.vline(x, rect.y_range(), Stroke::new(2.0, handle_color));
            // The grab tab stays inside the waveform, even at its edges.
            let tab_x = x.clamp(rect.left() + 4.0, rect.right() - 4.0);
            let tab =
                Rect::from_center_size(egui::pos2(tab_x, rect.top() + 8.0), Vec2::new(8.0, 16.0));
            painter.rect_filled(tab, 2.0, handle_color);
        }
        painter.rect_stroke(rect, 6.0, Stroke::new(1.0, p.border), StrokeKind::Inside);

        // Time labels in the corners; the dragged handle shows its time.
        let small = FontId::proportional(10.5);
        painter.text(
            rect.left_bottom() + Vec2::new(5.0, -3.0),
            Align2::LEFT_BOTTOM,
            "0 s",
            small.clone(),
            p.muted,
        );
        painter.text(
            rect.right_bottom() + Vec2::new(-5.0, -3.0),
            Align2::RIGHT_BOTTOM,
            trim::secs(dur),
            small.clone(),
            p.muted,
        );
        if let Some(d) = self.wave.drag {
            let (x, ms) = if d.end {
                (xe, d.end_ms)
            } else {
                (xs, d.start_ms)
            };
            let anchor = if d.end {
                Align2::RIGHT_TOP
            } else {
                Align2::LEFT_TOP
            };
            let dx = if d.end { -8.0 } else { 8.0 };
            painter.text(
                egui::pos2(x + dx, rect.top() + 3.0),
                anchor,
                trim::secs(ms),
                FontId::proportional(11.5),
                p.accent,
            );
        }

        // Playhead while this take is audible (playback is already trimmed).
        let playing_this = self.player.current() == Some(seg.id)
            && self.wave.playing_take.is_none_or(|t| t == take.id);
        if playing_this {
            let (from, to) = take.trim_ms.unwrap_or((0, dur));
            let pos = self.player.position().as_millis().min(u32::MAX as u128) as u32;
            let x = x_of(from.saturating_add(pos).min(to));
            painter.vline(x, rect.y_range(), Stroke::new(1.5, p.rejected));
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }

        if editable
            && self.wave.drag.is_none()
            && let Some(pos) = resp.hover_pos()
        {
            let near = (pos.x - xs).abs().min((pos.x - xe).abs()) < 10.0;
            if near {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }
        if self.wave.drag.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }

        // Actions and a summary of the trim.
        let mut auto = false;
        let mut reset = false;
        ui.horizontal_wrapped(|ui| {
            auto = ui
                .add_enabled(editable, egui::Button::new("✂ Auto-trim").small())
                .on_hover_text(
                    "Detect this take's leading and trailing silence with the project's trim settings (works even when automatic trimming is off)",
                )
                .clicked();
            reset = ui
                .add_enabled(
                    editable && take.trim_ms.is_some(),
                    egui::Button::new("Reset trim").small(),
                )
                .on_hover_text("Play and export the whole take again")
                .clicked();
            let summary = match take.trim_ms {
                Some((s, e)) => format!(
                    "Trimmed: {} – {}, keeps {} of {}",
                    trim::secs(s),
                    trim::secs(e.min(dur)),
                    trim::secs(e.min(dur).saturating_sub(s)),
                    trim::secs(dur)
                ),
                None => format!("Untrimmed · {}", trim::secs(dur)),
            };
            ui.label(RichText::new(summary).small().color(p.muted));
            let hint = if locked {
                "Approved segments are locked; unapprove to change the trim."
            } else {
                "Drag the handles to trim by hand. The audio file is never changed."
            };
            ui.label(RichText::new(hint).small().color(p.muted));
        });
        if auto {
            let settings = self
                .project
                .as_ref()
                .map(|p| p.settings.trim.clone())
                .unwrap_or_default();
            match trim::detect_take(&self.lib, &take, &settings) {
                Ok(Some(range)) => self.store_trim(ui.ctx(), take.id, Some(range)),
                Ok(None) => self.note(
                    false,
                    format!(
                        "Nothing in this take is above {} dB; left as it is.",
                        settings.threshold_db
                    ),
                ),
                Err(e) => self.note(true, format!("Auto-trim: {e:#}")),
            }
        }
        if reset {
            self.store_trim(ui.ctx(), take.id, None);
        }
    }

    /// Review toolbar: re-detect the trims of the whole chapter. Shown only
    /// when automatic trimming is on for the project.
    pub(super) fn auto_trim_chapter_button(&mut self, ui: &mut egui::Ui, chapter_id: Id) {
        if !self
            .project
            .as_ref()
            .is_some_and(|p| p.settings.trim.enabled)
        {
            return;
        }
        if ui
            .add_enabled(
                self.view.has_audio(),
                egui::Button::new("✂ Auto-trim chapter"),
            )
            .on_hover_text(
                "Detect leading and trailing silence for the current take of every unapproved segment, replacing earlier trims. Metadata only: no credits, audio files unchanged.",
            )
            .clicked()
        {
            self.worker.send(Command::AutoTrimChapter { chapter_id });
        }
    }
}

/// Project settings: the "Silence trimming" section.
pub(super) fn trim_settings_ui(ui: &mut egui::Ui, t: &mut TrimSettings) {
    let muted = theme::palette(ui).muted;
    ui.label(
        RichText::new(
            "Off by default. Trims each new take's leading/trailing silence; never touches the middle of a take. The audio files are not changed: every take's trim can be adjusted or reset on its waveform in Review.",
        )
        .small()
        .color(muted),
    );
    ui.checkbox(&mut t.enabled, "Trim new takes automatically");
    egui::Grid::new("trim_settings")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Silence threshold");
            ui.add(
                egui::DragValue::new(&mut t.threshold_db)
                    .range(-90.0..=-20.0)
                    .speed(0.5)
                    .suffix(" dBFS"),
            )
            .on_hover_text("10 ms frames quieter than this count as silence");
            ui.end_row();
            for (label, v, max) in [
                ("Keep before speech", &mut t.keep_head_ms, 2000),
                ("Keep after speech", &mut t.keep_tail_ms, 3000),
                ("Fade in/out", &mut t.fade_ms, 200),
            ] {
                ui.label(label);
                ui.add(
                    egui::DragValue::new(v)
                        .range(0..=max)
                        .speed(5)
                        .suffix(" ms"),
                );
                ui.end_row();
            }
        });
}
