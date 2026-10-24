//! Silence trimming in the UI: the take waveform in the Review detail panel
//! (trimmed regions shaded, draggable start/end handles, playhead), the
//! "Auto-trim chapter" toolbar button and the project settings section.
//!
//! A trim is metadata on the take (see `crate::trim`): editing it never
//! touches the WAV and costs nothing.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
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
pub(super) struct WaveState {
    /// Waveforms per audio file. A take's WAV never changes, so each file
    /// is read once (takes that share a cached file share the entry).
    cache: HashMap<String, Rc<Wave>>,
    /// Audio files being read on a background thread: the UI thread never
    /// reads a WAV to draw it.
    loading: HashSet<String>,
    loaded_tx: Sender<(String, Wave)>,
    loaded_rx: Receiver<(String, Wave)>,
    drag: Option<Drag>,
    /// The take queued by the last single-take play; `None` after a
    /// paragraph play, which plays each segment's current take.
    pub(super) playing_take: Option<Id>,
}

impl Default for WaveState {
    fn default() -> Self {
        let (loaded_tx, loaded_rx) = channel();
        Self {
            cache: HashMap::new(),
            loading: HashSet::new(),
            loaded_tx,
            loaded_rx,
            drag: None,
            playing_take: None,
        }
    }
}

struct Wave {
    peaks: Vec<(i16, i16)>,
    duration_ms: u32,
    /// The detector's input, so Auto-trim does not read the file again.
    frames: Option<trim::Frames>,
    error: Option<String>,
}

impl Wave {
    /// Read a take's WAV and prepare it for drawing (blocking).
    fn load(path: &Path) -> Self {
        match audio::read_wav(path) {
            Ok(pcm) => Wave {
                peaks: trim::peaks(&pcm, PEAK_BUCKETS),
                duration_ms: trim::duration_ms(&pcm),
                frames: Some(trim::Frames::of(&pcm)),
                error: None,
            },
            Err(e) => Wave {
                peaks: Vec::new(),
                duration_ms: 0,
                frames: None,
                error: Some(format!("{e:#}")),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    take_id: Id,
    /// Dragging the end handle (otherwise the start handle).
    end: bool,
    start_ms: u32,
    end_ms: u32,
}

impl Drag {
    /// Move the dragged handle to `ms`, keeping at least `MIN_KEEP_MS`
    /// between the handles and both inside the take.
    fn moved(self, ms: u32, dur: u32) -> Self {
        let ms = ms.min(dur);
        if self.end {
            Self {
                end_ms: ms.clamp((self.start_ms + MIN_KEEP_MS).min(dur), dur),
                ..self
            }
        } else {
            Self {
                start_ms: ms.min(self.end_ms.saturating_sub(MIN_KEEP_MS)),
                ..self
            }
        }
    }

    /// The trim to store on release; the whole take is stored as untrimmed.
    fn trim(&self, dur: u32) -> Option<(u32, u32)> {
        let whole = self.start_ms == 0 && self.end_ms >= dur;
        (!whole).then_some((self.start_ms, self.end_ms))
    }
}

/// Which handle a press at `x` grabs: the end handle when the press is at
/// or right of it, the start handle when at or left of it, otherwise the
/// nearer one. Handles that touch can still be pulled apart either way.
fn grabs_end(x: f32, xs: f32, xe: f32) -> bool {
    if x >= xe {
        true
    } else if x <= xs {
        false
    } else {
        (x - xe).abs() < (x - xs).abs()
    }
}

impl App {
    /// A take's audio as it is played and exported: trimmed when it has a trim.
    pub(super) fn take_audio(&self, take: &Take) -> anyhow::Result<Pcm> {
        let default = TrimSettings::default();
        let settings = self.project.as_ref().map_or(&default, |p| &p.settings.trim);
        trim::load_take(&self.lib, take, settings)
    }

    /// The waveform of a take's audio file: from the cache, or `None` while a
    /// background thread reads it (it asks for a repaint when done).
    fn wave_for(&mut self, ctx: &egui::Context, take: &Take) -> Option<Rc<Wave>> {
        while let Ok((key, wave)) = self.wave.loaded_rx.try_recv() {
            self.wave.loading.remove(&key);
            if self.wave.cache.len() >= CACHE_LIMIT {
                self.wave.cache.clear();
            }
            self.wave.cache.insert(key, Rc::new(wave));
        }
        if let Some(w) = self.wave.cache.get(&take.audio_path) {
            return Some(w.clone());
        }
        if self.wave.loading.insert(take.audio_path.clone()) {
            let key = take.audio_path.clone();
            let path = self.lib.audio_path(&take.audio_path);
            let (tx, ctx) = (self.wave.loaded_tx.clone(), ctx.clone());
            let spawned = std::thread::Builder::new()
                .name("waveform".into())
                .spawn(move || {
                    let _ = tx.send((key, Wave::load(&path)));
                    ctx.request_repaint();
                });
            if let Err(e) = spawned {
                self.wave.loading.remove(&take.audio_path);
                log::warn!("waveform thread: {e}");
            }
        }
        None
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
        let editable = !locked;
        // A drag belongs to one editable take; anything else ends it unsaved.
        if self.wave.drag.is_some_and(|d| d.take_id != take.id) || !editable {
            self.wave.drag = None;
        }
        ui.add_space(6.0);

        // Stay clear of the panel's (floating) scroll bar at the right edge,
        // or grabbing the end handle would grab the scroll bar.
        let bar = {
            let s = &ui.spacing().scroll;
            s.bar_inner_margin + s.bar_width + s.bar_outer_margin
        };
        let width = (ui.available_width() - bar).max(120.0);
        let Some(wave) = self.wave_for(ui.ctx(), &take) else {
            // Same footprint as the waveform, so nothing jumps when it arrives.
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, WAVE_H), Sense::hover());
            ui.painter()
                .rect_filled(rect, 6.0, ui.visuals().faint_bg_color);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Reading the take…",
                FontId::proportional(11.5),
                p.muted,
            );
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled(false, egui::Button::new("✂ Auto-trim").small());
                ui.add_enabled(false, egui::Button::new("Reset trim").small());
            });
            return;
        };
        if let Some(e) = &wave.error {
            ui.colored_label(p.rejected, format!("Waveform unavailable: {e}"));
            return;
        }
        let dur = wave.duration_ms.max(1);
        let sense = if editable {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        };
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, WAVE_H), Sense::hover());
        // A stable id, so a drag survives layout changes above the waveform.
        let resp = ui.interact(
            rect,
            ui.make_persistent_id(("trim_waveform", take.id)),
            sense,
        );
        let x_of = |ms: u32| rect.left() + rect.width() * (ms.min(dur) as f32 / dur as f32);
        let ms_of = |x: f32| {
            (((x - rect.left()) / rect.width()).clamp(0.0, 1.0) * dur as f32).round() as u32
        };

        // Dragging a handle: the one on the pressed side moves, committed on
        // release.
        let (stored_start, stored_end) = take
            .trim_ms
            .map_or((0, dur), |(s, e)| (s.min(dur), e.min(dur)));
        if editable {
            if resp.drag_started()
                && let Some(pos) = ui
                    .input(|i| i.pointer.press_origin())
                    .or(resp.interact_pointer_pos())
            {
                self.wave.drag = Some(Drag {
                    take_id: take.id,
                    end: grabs_end(pos.x, x_of(stored_start), x_of(stored_end)),
                    start_ms: stored_start,
                    end_ms: stored_end,
                });
            }
            if let Some(mut d) = self.wave.drag {
                if (resp.dragged() || resp.drag_stopped())
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    d = d.moved(ms_of(pos.x), dur);
                }
                if resp.drag_stopped() {
                    self.wave.drag = None;
                    resp.surrender_focus();
                    let new = d.trim(dur);
                    if new != take.trim_ms {
                        self.store_trim(ui.ctx(), take.id, new);
                    }
                } else if resp.dragged() {
                    self.wave.drag = Some(d);
                } else {
                    // The pointer was released without this waveform seeing
                    // it: drop the drag rather than show a range never saved.
                    self.wave.drag = None;
                }
            }
        }
        let (start, end) = match self.wave.drag {
            Some(d) => (d.start_ms, d.end_ms),
            None => (stored_start, stored_end),
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
            // Same detector as the worker, on the frames read with the
            // waveform: no file read on the UI thread.
            match wave
                .frames
                .as_ref()
                .map(|f| trim::detect_frames(f, &settings))
            {
                Some(Some(range)) => self.store_trim(ui.ctx(), take.id, Some(range)),
                Some(None) => self.note(
                    false,
                    format!(
                        "Nothing in this take is above {} dB; left as it is.",
                        settings.threshold_db
                    ),
                ),
                None => self.note(true, "Auto-trim: the take could not be read.".into()),
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
            for (label, v, max, hint) in [
                (
                    "Keep before speech",
                    &mut t.keep_head_ms,
                    2000,
                    "Silence left before the first sound (used by detection)",
                ),
                (
                    "Keep after speech",
                    &mut t.keep_tail_ms,
                    3000,
                    "Silence left after the last sound (used by detection)",
                ),
                (
                    "Fade in/out",
                    &mut t.fade_ms,
                    200,
                    "Applied at the cut points whenever a trimmed take is played or exported, approved takes included",
                ),
            ] {
                ui.label(label);
                ui.add(
                    egui::DragValue::new(v)
                        .range(0..=max)
                        .speed(5)
                        .suffix(" ms"),
                )
                .on_hover_text(hint);
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drag(end: bool, start_ms: u32, end_ms: u32) -> Drag {
        Drag {
            take_id: 1,
            end,
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn handles_keep_their_order_and_a_minimum_length() {
        let dur = 3000;
        // The start handle cannot pass the end handle (minus 20 ms).
        let d = drag(false, 0, 1000).moved(2500, dur);
        assert_eq!((d.start_ms, d.end_ms), (980, 1000));
        // The end handle cannot pass the start handle, nor leave the take.
        let d = drag(true, 500, 3000).moved(100, dur);
        assert_eq!((d.start_ms, d.end_ms), (500, 520));
        let d = drag(true, 500, 1000).moved(9999, dur);
        assert_eq!(d.end_ms, 3000);
        // A take shorter than the minimum keeps all of it.
        let d = drag(true, 0, 10).moved(0, 10);
        assert_eq!((d.start_ms, d.end_ms), (0, 10));
        let d = drag(false, 0, 10).moved(8, 10);
        assert_eq!((d.start_ms, d.end_ms), (0, 10));
    }

    #[test]
    fn releasing_on_the_whole_take_stores_untrimmed() {
        assert_eq!(drag(false, 0, 3000).trim(3000), None);
        assert_eq!(drag(true, 0, 2999).trim(3000), Some((0, 2999)));
        assert_eq!(drag(false, 1, 3000).trim(3000), Some((1, 3000)));
    }

    #[test]
    fn a_press_grabs_the_handle_on_its_side() {
        // Between the handles: the nearer one.
        assert!(!grabs_end(110.0, 100.0, 300.0));
        assert!(grabs_end(290.0, 100.0, 300.0));
        // Outside them: the one on that side, even when they touch.
        assert!(grabs_end(305.0, 300.0, 300.5));
        assert!(!grabs_end(295.0, 300.0, 300.5));
        assert!(grabs_end(400.0, 100.0, 300.0));
        assert!(!grabs_end(50.0, 100.0, 300.0));
    }
}
