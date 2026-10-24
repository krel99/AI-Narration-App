//! Choosing voices from the user's own ElevenLabs account. The list loads
//! when the Voices dialog opens and stays in memory; "Choose…" opens a
//! picker that searches and filters it; the Voices grid shows each assigned
//! voice's name; previews play through the app's player.

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, Key, RichText, Sense, UiBuilder, Vec2};

use super::{App, VoiceEditor, VoiceRow, chip};
use crate::audio::Pcm;
use crate::model::{Id, NARRATOR};
use crate::theme::{self, primary_button};
use crate::tts::VoiceInfo;
use crate::tts::voices::plausible_id;
use crate::voices::{self, Facet};
use crate::worker::Command;

/// Player queue id of a voice preview; segment ids are positive.
pub(super) const PREVIEW_ID: Id = -1;
/// Height of a picker row: name and chips, two description lines and
/// "Used by" must fit inside it (see the layout test).
const ROW_H: f32 = 84.0;
/// Vertical padding inside a picker row.
const ROW_PAD_Y: f32 = 4.0;
const MAX_LABEL_CHIPS: usize = 5;
/// Label keys shown as chips, in this order (any others follow).
const LABEL_ORDER: [&str; 6] = [
    "gender",
    "age",
    "accent",
    "descriptive",
    "description",
    "use_case",
];

#[derive(Default)]
pub(super) struct VoiceUi {
    /// The API key the list was loaded with; another key loads it again.
    loaded_key: Option<String>,
    /// The API key of the load in flight.
    requested_key: Option<String>,
    loading: bool,
    error: Option<String>,
    /// Assigned voices outside the account list, found by id.
    others: HashMap<String, VoiceInfo>,
    /// Assigned ids ElevenLabs has no voice for.
    unknown: HashSet<String>,
    /// Ids already looked up (or being looked up) since the last load.
    asked: HashSet<String>,
    /// Lookup requests not answered yet.
    lookups_pending: usize,
    picker: Option<Picker>,
    preview_loading: Option<String>,
    /// The voice whose preview was last handed to the player.
    preview_playing: Option<String>,
    preview_error: Option<(String, String)>,
}

impl VoiceUi {
    /// Bookkeeping for a finished load. The list belongs to the key it was
    /// requested with, not to whatever key is set now; returns true when the
    /// key changed meanwhile, so the list should be loaded again.
    fn load_finished(&mut self, outcome: Result<(), String>, current_key: &str) -> bool {
        self.loading = false;
        let requested = self.requested_key.take();
        match outcome {
            Ok(()) => {
                self.loaded_key = Some(requested.clone().unwrap_or_else(|| current_key.into()));
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        !current_key.is_empty() && requested.is_some_and(|k| k != current_key)
    }
}

struct Picker {
    /// The Voices editor row being assigned.
    key: String,
    speaker: String,
    filter: voices::Filter,
    focus_search: bool,
    /// Frames left in which the window is raised again: opened by a click
    /// in the Voices dialog, both windows ask to be on top in that frame and
    /// egui then keeps their old order, which can hide the picker.
    raise: u8,
}

/// What the preview buttons show this frame.
#[derive(Default, Clone)]
pub(super) struct PreviewNow {
    loading: Option<String>,
    playing: Option<String>,
    error: Option<(String, String)>,
}

impl PreviewNow {
    fn error_for(&self, voice_id: &str) -> Option<&str> {
        self.error
            .as_ref()
            .filter(|(id, _)| id == voice_id)
            .map(|(_, e)| e.as_str())
    }
}

/// What a click in the Voices grid asks for.
pub(super) enum CellAction {
    Choose(String),
    Preview(Box<VoiceInfo>),
}

/// Set the voice of the editor row `key`. Returns false if there is no
/// such row.
pub(super) fn assign_voice(ed: &mut VoiceEditor, key: &str, voice_id: &str) -> bool {
    match ed.rows.iter_mut().find(|r| r.key == key) {
        Some(row) => {
            row.voice_id = voice_id.to_string();
            true
        }
        None => false,
    }
}

/// A voice by id: from the account list (`true`) or from a lookup.
fn resolve<'a>(
    voices: &'a [VoiceInfo],
    state: &'a VoiceUi,
    id: &str,
) -> Option<(&'a VoiceInfo, bool)> {
    if id.is_empty() {
        return None;
    }
    voices
        .iter()
        .find(|v| v.voice_id == id)
        .map(|v| (v, true))
        .or_else(|| state.others.get(id).map(|v| (v, false)))
}

fn voices_n(n: usize) -> String {
    if n == 1 {
        "1 voice".into()
    } else {
        format!("{n} voices")
    }
}

fn count_text(n: usize) -> String {
    if n == 0 {
        "No voices in your account yet".into()
    } else {
        format!("{} in your account", voices_n(n))
    }
}

/// "cloned · female · young · british", for hover texts.
fn summary(v: &VoiceInfo) -> String {
    let mut parts: Vec<String> = v.category.iter().cloned().collect();
    for k in ["gender", "age", "accent"] {
        if let Some(l) = v.labels.get(k) {
            parts.push(l.clone());
        }
    }
    let mut s = v.name.clone();
    if !parts.is_empty() {
        s.push_str(&format!("\n{}", parts.join(" · ").replace('_', " ")));
    }
    s.push_str(&format!("\nvoice id {}", v.voice_id));
    s
}

/// The label values shown as chips, in a stable order.
fn label_chips(v: &VoiceInfo) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: &String| {
        let s = s.replace('_', " ");
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&s)) {
            out.push(s);
        }
    };
    for k in LABEL_ORDER {
        if let Some(l) = v.labels.get(k) {
            add(l);
        }
    }
    for (k, l) in &v.labels {
        if !LABEL_ORDER.contains(&k.as_str()) && k != "language" {
            add(l);
        }
    }
    out.truncate(MAX_LABEL_CHIPS);
    out
}

/// ▶ / ■, or a spinner while the preview downloads. True when clicked.
fn preview_button(ui: &mut egui::Ui, v: &VoiceInfo, now: &PreviewNow) -> bool {
    let p = *theme::palette(ui);
    let id = v.voice_id.as_str();
    if now.loading.as_deref() == Some(id) {
        ui.add_sized([26.0, 22.0], egui::Spinner::new().size(14.0))
            .on_hover_text("Downloading the preview…");
        return false;
    }
    let playing = now.playing.as_deref() == Some(id);
    let available = !voices::preview_sources(v, "").is_empty();
    let clicked = ui
        .add_enabled_ui(available, |ui| {
            ui.add_sized(
                [26.0, 22.0],
                egui::Button::new(if playing { "■" } else { "▶" }),
            )
        })
        .inner
        .on_hover_text(if playing {
            "Stop the preview"
        } else {
            "Play the voice's preview (downloaded once, then cached)"
        })
        .on_disabled_hover_text("This voice has no preview")
        .clicked();
    if let Some(e) = now.error_for(id) {
        ui.label(RichText::new("⚠").color(p.warning))
            .on_hover_text(e);
    }
    clicked
}

/// The Voices grid cell beside the voice-id field: the assigned voice's
/// name, a preview button and "Choose…".
pub(super) fn voice_cell(
    ui: &mut egui::Ui,
    row: &VoiceRow,
    voices: &[VoiceInfo],
    state: &VoiceUi,
    now: &PreviewNow,
) -> Option<CellAction> {
    let p = *theme::palette(ui);
    let id = row.voice_id.trim();
    let mut action = None;
    let resolved = resolve(voices, state, id);
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(150.0, 22.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(150.0);
                ui.set_max_width(150.0);
                let muted = |t: &str| RichText::new(t).small().color(p.muted);
                match resolved {
                    Some((v, true)) => {
                        ui.add(egui::Label::new(RichText::new(&v.name).strong()).truncate())
                            .on_hover_text(summary(v));
                    }
                    Some((v, false)) => {
                        ui.add(egui::Label::new(RichText::new(&v.name).strong()).truncate())
                            .on_hover_text(format!(
                                "{}\n\nNot in My Voices (an ElevenLabs default voice, or one you removed). It still works.",
                                summary(v)
                            ));
                    }
                    None if id.is_empty() => {
                        ui.label(muted("no voice"));
                    }
                    None if state.unknown.contains(id) => {
                        ui.label(RichText::new("⚠ unknown id").small().color(p.warning))
                            .on_hover_text("ElevenLabs has no voice with this id for your API key. It is kept as typed.");
                    }
                    None if state.loading
                        || (state.lookups_pending > 0 && state.asked.contains(id)) =>
                    {
                        ui.label(muted("…"));
                    }
                    None if state.loaded_key.is_none() => {
                        ui.label(muted("—")).on_hover_text("Your voices are not loaded");
                    }
                    None => {
                        ui.label(muted("not in your voices")).on_hover_text(
                            "This id is not among your loaded voices. It is kept as typed.",
                        );
                    }
                }
            },
        );
        match resolved {
            Some((v, _)) => {
                if preview_button(ui, v, now) {
                    action = Some(CellAction::Preview(Box::new(v.clone())));
                }
            }
            None => {
                ui.add_enabled_ui(false, |ui| {
                    ui.add_sized([26.0, 22.0], egui::Button::new("▶"))
                })
                .inner
                .on_disabled_hover_text("No known voice to preview");
            }
        }
        if ui
            .button("Choose…")
            .on_hover_text("Pick from the voices in your ElevenLabs account")
            .clicked()
        {
            action = Some(CellAction::Choose(row.key.clone()));
        }
    });
    action
}

/// The line above the Voices grid: the list's state and a refresh button.
/// True when refresh was clicked.
pub(super) fn list_status(ui: &mut egui::Ui, count: usize, state: &VoiceUi) -> bool {
    let p = *theme::palette(ui);
    let mut refresh = false;
    ui.horizontal(|ui| {
        refresh = ui
            .add_enabled(!state.loading, egui::Button::new("⟳ Refresh").small())
            .on_hover_text("Load the voice list from ElevenLabs again")
            .clicked();
        if state.loading {
            ui.spinner();
            ui.label(
                RichText::new("Loading your ElevenLabs voices…")
                    .small()
                    .color(p.muted),
            );
        } else if let Some(e) = &state.error {
            ui.add(egui::Label::new(RichText::new(e).small().color(p.rejected)).wrap());
        } else if state.loaded_key.is_some() {
            ui.label(RichText::new(count_text(count)).small().color(p.muted));
        } else {
            ui.label(
                RichText::new(
                    "Your ElevenLabs voices load here once an API key is set (Settings).",
                )
                .small()
                .color(p.muted),
            );
        }
    });
    refresh
}

/// What a click in a picker row asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    Assign,
    Preview,
}

/// What every picker row needs besides its voice.
struct RowCtx<'a> {
    now: &'a PreviewNow,
    /// The open dialog's (speaker key, voice id) rows, for "Used by".
    assignments: &'a [(String, String)],
    /// The speaker being assigned: its key and how it is named.
    key: &'a str,
    speaker: &'a str,
    /// Its voice id in the dialog now.
    current: &'a str,
}

struct RowOut {
    action: Option<RowAction>,
    /// The row's padded inside and what its text column took; the text
    /// must fit, or the last line is cut off (checked by the layout test).
    #[cfg_attr(not(test), allow(dead_code))]
    inner: egui::Rect,
    #[cfg_attr(not(test), allow(dead_code))]
    text: egui::Rect,
    /// ▶ and Assign, which must stay inside the row.
    #[cfg_attr(not(test), allow(dead_code))]
    buttons: egui::Rect,
}

/// One voice of the picker list. It takes exactly `ROW_H` (plus the usual
/// item spacing), which is what `show_rows` assumes when it places the
/// visible rows: the contents go into a child `Ui` that does not move the
/// list's cursor again.
fn picker_row(ui: &mut egui::Ui, v: &VoiceInfo, c: &RowCtx) -> RowOut {
    let p = *theme::palette(ui);
    let is_current = !c.current.is_empty() && v.voice_id == c.current;
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::hover());
    let fill = if is_current {
        p.accent_soft
    } else if resp.hovered() {
        ui.visuals().faint_bg_color
    } else {
        p.card
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    ui.painter().rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(Vec2::new(10.0, ROW_PAD_Y));
    let mut row = ui.new_child(
        UiBuilder::new()
            .scope_id(egui::Id::unique(("voice_picker_row", &v.voice_id)))
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let mut action = None;
    let buttons_w = 120.0;
    let text_w = (inner.width() - buttons_w).max(160.0);
    let text = row
        .allocate_ui_with_layout(
            Vec2::new(text_w, inner.height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(text_w);
                let clip = ui.max_rect().intersect(ui.clip_rect());
                ui.set_clip_rect(clip);
                ui.spacing_mut().item_spacing.y = 2.0;
                // Name and chips on one line of a fixed size: chips that do
                // not fit are cut off rather than widening the column (which
                // would re-wrap the description wider than the row and push
                // the buttons out of the window).
                let (line, _) = ui.allocate_exact_size(
                    Vec2::new(text_w, ui.spacing().interact_size.y),
                    Sense::hover(),
                );
                let mut l = ui.new_child(
                    UiBuilder::new()
                        .max_rect(line)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                l.set_clip_rect(line.intersect(clip));
                l.spacing_mut().item_spacing.x = 4.0;
                l.add(egui::Label::new(RichText::new(&v.name).strong().size(14.5)).truncate())
                    .on_hover_text(summary(v));
                if is_current {
                    chip(&mut l, "current", p.approved);
                }
                if let Some(cat) = &v.category {
                    chip(&mut l, cat.replace('_', " "), p.accent);
                }
                for label in label_chips(v) {
                    chip(&mut l, label, p.muted);
                }
                let (text, color) = match &v.description {
                    Some(d) => (d.clone(), ui.visuals().text_color()),
                    None => ("No description".to_string(), p.muted),
                };
                let mut job = egui::text::LayoutJob::simple(
                    text,
                    egui::FontId::proportional(12.5),
                    color,
                    text_w,
                );
                job.wrap.max_rows = 2;
                job.wrap.overflow_character = Some('…');
                ui.add(egui::Label::new(job).selectable(false));
                let users = voices::used_by(
                    c.assignments.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                    &v.voice_id,
                    c.key,
                );
                if !users.is_empty() {
                    let text = format!("Used by: {}", users.join(", "));
                    ui.add(
                        egui::Label::new(RichText::new(&text).small().color(p.warning)).truncate(),
                    )
                    .on_hover_text(text);
                }
            },
        )
        .response
        .rect;
    let buttons = row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui
            .add(primary_button(ui, "Assign"))
            .on_hover_text(format!("Use {} for {}", v.name, c.speaker))
            .clicked()
        {
            action = Some(RowAction::Assign);
        }
        if preview_button(ui, v, c.now) {
            action = Some(RowAction::Preview);
        }
    });
    RowOut {
        action,
        inner,
        text,
        buttons: buttons.response.rect,
    }
}

impl App {
    /// Hook: the Voices dialog opened. Loads the list once per API key
    /// (again after an error); otherwise the list in memory is used.
    pub(super) fn voices_dialog_opened(&mut self) {
        let key = self.cfg.api_keys().elevenlabs;
        if key.is_empty() {
            return;
        }
        if self.voice_ui.loaded_key.as_deref() != Some(key.as_str())
            || self.voice_ui.error.is_some()
        {
            self.refresh_voices();
        }
    }

    pub(super) fn refresh_voices(&mut self) {
        if self.voice_ui.loading {
            return;
        }
        let key = self.cfg.api_keys().elevenlabs;
        if key.is_empty() {
            self.voice_ui.error = Some("Set the ElevenLabs API key in Settings first.".into());
            return;
        }
        let ui = &mut self.voice_ui;
        ui.requested_key = Some(key);
        ui.loading = true;
        ui.error = None;
        ui.others.clear();
        ui.unknown.clear();
        ui.asked.clear();
        ui.lookups_pending = 0;
        self.worker.send(Command::FetchVoices);
    }

    pub(super) fn on_voices(&mut self, result: Result<Vec<VoiceInfo>, String>) {
        let current = self.cfg.api_keys().elevenlabs;
        let outcome = match result {
            Ok(list) => {
                self.note(
                    false,
                    format!(
                        "Loaded {} from your ElevenLabs account.",
                        voices_n(list.len())
                    ),
                );
                self.voices = list;
                Ok(())
            }
            Err(e) => {
                self.note(true, e.clone());
                Err(e)
            }
        };
        if self.voice_ui.load_finished(outcome, &current) && self.dialogs.voices.is_some() {
            self.refresh_voices();
        }
    }

    pub(super) fn on_voices_looked_up(&mut self, found: Vec<VoiceInfo>, missing: Vec<String>) {
        self.voice_ui.lookups_pending = self.voice_ui.lookups_pending.saturating_sub(1);
        for v in found {
            self.voice_ui.others.insert(v.voice_id.clone(), v);
        }
        self.voice_ui.unknown.extend(missing);
    }

    pub(super) fn on_voice_preview(&mut self, voice_id: String, result: Result<Pcm, String>) {
        // Cancelled or replaced meanwhile: the file is cached for next time.
        if self.voice_ui.preview_loading.as_deref() != Some(voice_id.as_str()) {
            return;
        }
        self.voice_ui.preview_loading = None;
        match result {
            Ok(pcm) => {
                self.player.play(vec![(PREVIEW_ID, pcm)], 0);
                self.voice_ui.preview_playing = Some(voice_id);
            }
            Err(e) => {
                self.note(true, format!("Voice preview: {e}"));
                self.voice_ui.preview_error = Some((voice_id, e));
            }
        }
    }

    pub(super) fn preview_now(&self) -> PreviewNow {
        let ui = &self.voice_ui;
        PreviewNow {
            loading: ui.preview_loading.clone(),
            playing: ui
                .preview_playing
                .clone()
                .filter(|_| self.player.current() == Some(PREVIEW_ID)),
            error: ui.preview_error.clone(),
        }
    }

    /// ▶ starts a voice's preview; a second click stops it (or abandons the
    /// download).
    fn toggle_preview(&mut self, voice: VoiceInfo) {
        let now = self.preview_now();
        if now.playing.as_deref() == Some(voice.voice_id.as_str()) {
            self.player.stop();
            self.voice_ui.preview_playing = None;
            return;
        }
        if now.loading.as_deref() == Some(voice.voice_id.as_str()) {
            self.voice_ui.preview_loading = None;
            return;
        }
        if now.playing.is_some() {
            self.player.stop();
            self.voice_ui.preview_playing = None;
        }
        self.voice_ui.preview_error = None;
        self.voice_ui.preview_loading = Some(voice.voice_id.clone());
        let language = self
            .project
            .as_ref()
            .map(|p| p.language.clone())
            .unwrap_or_default();
        self.worker.send(Command::PreviewVoice {
            voice: Box::new(voice),
            language,
        });
    }

    /// Hook: a click in the Voices grid.
    pub(super) fn voice_cell_action(&mut self, action: CellAction) {
        match action {
            CellAction::Preview(v) => self.toggle_preview(*v),
            CellAction::Choose(key) => {
                let speaker = self
                    .dialogs
                    .voices
                    .as_ref()
                    .and_then(|ed| ed.rows.iter().find(|r| r.key == key))
                    .map(|r| {
                        if r.key == NARRATOR {
                            "the narrator".to_string()
                        } else if r.display_name.trim().is_empty() {
                            r.key.clone()
                        } else {
                            r.display_name.trim().to_string()
                        }
                    })
                    .unwrap_or_else(|| key.clone());
                self.voice_ui.picker = Some(Picker {
                    key,
                    speaker,
                    filter: Default::default(),
                    focus_search: true,
                    raise: 2,
                });
                if self.voice_ui.loaded_key.is_none() && !self.voice_ui.loading {
                    self.refresh_voices();
                }
            }
        }
    }

    /// Look up assigned ids that are neither in the list nor looked up yet,
    /// once the list is in and no text field is being typed into.
    fn lookup_unresolved(&mut self, ctx: &egui::Context) {
        let state = &self.voice_ui;
        if state.loading || state.error.is_some() || state.loaded_key.is_none() {
            return;
        }
        if ctx.text_edit_focused() {
            return;
        }
        let Some(ed) = &self.dialogs.voices else {
            return;
        };
        let mut ids: Vec<String> = Vec::new();
        for row in &ed.rows {
            let id = row.voice_id.trim();
            // Voice ids are 20 characters; shorter text is still being typed.
            if id.len() >= 8
                && plausible_id(id)
                && !state.asked.contains(id)
                && !ids.iter().any(|i| i == id)
                && resolve(&self.voices, state, id).is_none()
            {
                ids.push(id.to_string());
            }
        }
        if ids.is_empty() {
            return;
        }
        self.voice_ui.asked.extend(ids.iter().cloned());
        self.voice_ui.lookups_pending += 1;
        self.worker.send(Command::LookupVoices { ids });
    }

    /// The "Choose a voice for …" window (and cleanup once the Voices
    /// dialog is closed).
    pub(super) fn dialog_voice_picker(&mut self, ctx: &egui::Context) {
        if self.dialogs.voices.is_none() {
            self.voice_ui.picker = None;
            self.voice_ui.preview_loading = None;
            if self.voice_ui.preview_playing.take().is_some()
                && self.player.current() == Some(PREVIEW_ID)
            {
                self.player.stop();
            }
            return;
        }
        self.lookup_unresolved(ctx);
        if self.voice_ui.picker.is_none() {
            return;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.voice_ui.picker = None;
            return;
        }
        let now = self.preview_now();
        let free_plan = self
            .account_tier()
            .is_some_and(|t| t.eq_ignore_ascii_case("free"));
        let assignments: Vec<(String, String)> = self
            .dialogs
            .voices
            .as_ref()
            .map(|ed| {
                ed.rows
                    .iter()
                    .filter(|r| !r.remove)
                    .map(|r| (r.key.clone(), r.voice_id.trim().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let voices = &self.voices;
        let VoiceUi {
            picker,
            loading,
            error,
            loaded_key,
            ..
        } = &mut self.voice_ui;
        let Some(picker) = picker.as_mut() else {
            return;
        };
        let loading = *loading;
        let loaded = loaded_key.is_some();
        let current = assignments
            .iter()
            .find(|(k, _)| *k == picker.key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let mut open = true;
        let mut refresh = false;
        let mut assign: Option<String> = None;
        let mut preview: Option<VoiceInfo> = None;
        let window = egui::Window::new(format!("Choose a voice for {}", picker.speaker))
            .id(egui::Id::unique("voice_picker"))
            .open(&mut open)
            .collapsible(false)
            .default_size([700.0, 560.0])
            .min_width(520.0)
            .show(ctx, |ui| {
                let p = *theme::palette(ui);
                ui.horizontal(|ui| {
                    let search = ui.add(
                        egui::TextEdit::singleline(&mut picker.filter.query)
                            .hint_text("Search name, labels, description")
                            .desired_width((ui.available_width() - 290.0).clamp(150.0, 280.0)),
                    );
                    if picker.focus_search {
                        search.request_focus();
                        picker.focus_search = false;
                    }
                    if loading {
                        ui.spinner();
                        ui.label(RichText::new("Loading…").small().color(p.muted));
                    } else if loaded {
                        let shown = voices::filtered(voices, &picker.filter).len();
                        let text = if picker.filter.is_active() {
                            format!("{shown} of {}", count_text(voices.len()))
                        } else {
                            count_text(voices.len())
                        };
                        ui.label(RichText::new(text).small().color(p.muted));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        refresh = ui
                            .add_enabled(!loading, egui::Button::new("⟳ Refresh"))
                            .on_hover_text("Load the voice list from ElevenLabs again")
                            .clicked();
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    for facet in Facet::ALL {
                        let options = voices::facet_options(voices, facet);
                        if options.is_empty() {
                            continue;
                        }
                        let chosen = picker.filter.get(facet).map(str::to_string);
                        let text = match &chosen {
                            Some(v) => format!("{}: {}", facet.label(), voices::facet_display(facet, v)),
                            None => format!("{}: any", facet.label()),
                        };
                        let mut pick: Option<Option<String>> = None;
                        egui::ComboBox::from_id_salt(("voice_facet", facet.label()))
                            .selected_text(text)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(chosen.is_none(), "any").clicked() {
                                    pick = Some(None);
                                }
                                for o in &options {
                                    let label = voices::facet_display(facet, o);
                                    if ui.selectable_label(chosen.as_deref() == Some(o), label).clicked() {
                                        pick = Some(Some(o.clone()));
                                    }
                                }
                            });
                        if let Some(v) = pick {
                            picker.filter.set(facet, v);
                        }
                    }
                    if picker.filter.is_active()
                        && ui
                            .add(egui::Button::new(RichText::new("✕ Clear filters").color(p.accent)).frame(false))
                            .clicked()
                    {
                        picker.filter.clear();
                    }
                });
                if let Some(e) = error.as_deref() {
                    ui.add(egui::Label::new(RichText::new(e).color(p.rejected)).wrap());
                }
                if free_plan {
                    ui.add(
                        egui::Label::new(
                            RichText::new("Free plan: ElevenLabs may refuse voices added from the Voice Library for API use.")
                                .small()
                                .color(p.warning),
                        )
                        .wrap(),
                    );
                }
                ui.separator();
                let shown = voices::filtered(voices, &picker.filter);
                if voices.is_empty() {
                    if loaded && !loading {
                        ui.add_space(12.0);
                        ui.label(RichText::new("No voices in your account yet.").strong());
                        ui.label(
                            RichText::new("Create, clone or add voices on elevenlabs.io (My Voices), then press Refresh. A voice id can also be pasted into the Voices grid.")
                                .small()
                                .color(p.muted),
                        );
                    }
                    return;
                }
                if shown.is_empty() {
                    ui.add_space(12.0);
                    ui.label(RichText::new("No voices match.").color(p.muted));
                    return;
                }
                let row = RowCtx {
                    now: &now,
                    assignments: &assignments,
                    key: &picker.key,
                    speaker: &picker.speaker,
                    current: &current,
                };
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, ROW_H, shown.len(), |ui, range| {
                        for &i in &shown[range] {
                            match picker_row(ui, &voices[i], &row).action {
                                Some(RowAction::Assign) => assign = Some(voices[i].voice_id.clone()),
                                Some(RowAction::Preview) => preview = Some(voices[i].clone()),
                                None => {}
                            }
                        }
                    });
            });
        if picker.raise > 0 {
            picker.raise -= 1;
            if let Some(r) = &window {
                ctx.move_to_top(r.response.layer_id);
            }
        }
        let key = picker.key.clone();
        if !open {
            self.voice_ui.picker = None;
        }
        if refresh {
            self.refresh_voices();
        }
        if let Some(v) = preview {
            self.toggle_preview(v);
        }
        if let Some(id) = assign {
            if let Some(ed) = &mut self.dialogs.voices {
                assign_voice(ed, &key, &id);
            }
            self.voice_ui.picker = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, voice: &str) -> VoiceRow {
        VoiceRow {
            key: key.into(),
            display_name: String::new(),
            aliases: String::new(),
            voice_id: voice.into(),
            character_id: None,
            remove: false,
        }
    }

    #[test]
    fn assigning_updates_the_editor_row() {
        let mut ed = VoiceEditor {
            rows: vec![row(NARRATOR, "old"), row("vale", "")],
            new_key: String::new(),
            rename_from: String::new(),
            rename_to: String::new(),
        };
        assert!(assign_voice(&mut ed, "vale", "v42"));
        assert_eq!(ed.rows[1].voice_id, "v42");
        assert_eq!(ed.rows[0].voice_id, "old", "other rows untouched");
        assert!(!assign_voice(&mut ed, "nobody", "v1"));
    }

    #[test]
    fn names_resolve_from_the_list_then_lookups() {
        let voices = vec![VoiceInfo {
            voice_id: "a1".into(),
            name: "Vale".into(),
            ..Default::default()
        }];
        let mut state = VoiceUi::default();
        state.others.insert(
            "p1".into(),
            VoiceInfo {
                voice_id: "p1".into(),
                name: "Rachel".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            resolve(&voices, &state, "a1").map(|(v, l)| (v.name.as_str(), l)),
            Some(("Vale", true))
        );
        assert_eq!(
            resolve(&voices, &state, "p1").map(|(v, l)| (v.name.as_str(), l)),
            Some(("Rachel", false))
        );
        assert!(resolve(&voices, &state, "zz").is_none());
        assert!(resolve(&voices, &state, "").is_none());
    }

    fn labelled(id: &str) -> VoiceInfo {
        VoiceInfo {
            voice_id: id.into(),
            name: "Bradford - Expressive and Articulate Storyteller".into(),
            category: Some("professional".into()),
            labels: [
                ("gender", "male"),
                ("age", "middle_aged"),
                ("accent", "british-essex"),
                ("descriptive", "professional"),
                ("use_case", "informative_educational"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            description: Some("Clipped, authoritative officer with a dry edge. ".repeat(12)),
            ..Default::default()
        }
    }

    /// `show_rows` places the visible rows assuming each takes `ROW_H` plus
    /// the item spacing; a row that moves the cursor by anything else makes
    /// rows touch, leaves a gap at the end of the list and jumps while
    /// scrolling. Its text (name, two description lines, "Used by") must
    /// fit inside it, and a long name with many chips must not widen the
    /// text and push ▶ / Assign out of the row, at the picker's narrowest
    /// and at a wide size.
    #[test]
    fn picker_rows_take_their_height_and_their_text_fits() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let now = PreviewNow::default();
        let assignments: Vec<(String, String)> =
            [("narrator", "v1"), ("vale", "v2"), ("voss", "v3")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let row = RowCtx {
            now: &now,
            assignments: &assignments,
            key: "mara",
            speaker: "Mara",
            current: "v2",
        };
        let voices: Vec<VoiceInfo> = (1..=3).map(|i| labelled(&format!("v{i}"))).collect();
        for width in [480.0, 1000.0] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(width, 700.0),
                )),
                ..Default::default()
            };
            let mut rows = 0;
            let mut output = ctx.run_ui(input, |ui| {
                let spacing = ui.spacing().item_spacing.y;
                let top = ui.cursor().min.y;
                for (n, v) in voices.iter().enumerate() {
                    let out = picker_row(ui, v, &row);
                    assert_eq!(out.action, None);
                    let moved = ui.cursor().min.y - top;
                    assert!(
                        (moved - (n + 1) as f32 * (ROW_H + spacing)).abs() < 0.01,
                        "row {n} at width {width}: cursor moved {moved}"
                    );
                    assert!(
                        out.text.max.y <= out.inner.max.y + 0.5,
                        "width {width}: text {:?} overflows the row {:?}",
                        out.text,
                        out.inner
                    );
                    assert!(
                        out.text.height() > 55.0,
                        "name, two description lines and Used by: {:?}",
                        out.text
                    );
                    assert!(
                        out.text.max.x <= out.buttons.min.x + 0.5,
                        "width {width}: text {:?} runs into the buttons {:?}",
                        out.text,
                        out.buttons
                    );
                    assert!(
                        out.buttons.max.x <= out.inner.max.x + 0.5 && out.buttons.width() > 60.0,
                        "width {width}: buttons {:?} outside the row {:?}",
                        out.buttons,
                        out.inner
                    );
                    rows += 1;
                }
            });
            output.textures_delta.clear();
            assert!(rows >= 3);
        }
    }

    #[test]
    fn a_list_belongs_to_the_key_it_was_requested_with() {
        let mut state = VoiceUi {
            requested_key: Some("old".into()),
            loading: true,
            ..Default::default()
        };
        // The key changed in Settings while the list was loading.
        assert!(state.load_finished(Ok(()), "new"), "load again");
        assert_eq!(state.loaded_key.as_deref(), Some("old"));
        assert!(!state.loading && state.error.is_none());

        state.requested_key = Some("new".into());
        assert!(!state.load_finished(Ok(()), "new"));
        assert_eq!(state.loaded_key.as_deref(), Some("new"));

        // An error for the current key is shown, not retried in a loop.
        state.requested_key = Some("new".into());
        assert!(!state.load_finished(Err("HTTP 500".into()), "new"));
        assert_eq!(state.error.as_deref(), Some("HTTP 500"));
        // An error for a key that is no longer set is retried with the new one.
        state.requested_key = Some("typo".into());
        assert!(state.load_finished(Err("rejected".into()), "fixed"));
        // The key was removed meanwhile: nothing to load.
        state.requested_key = Some("gone".into());
        assert!(!state.load_finished(Ok(()), ""));
    }

    #[test]
    fn chips_and_counts() {
        let v = VoiceInfo {
            voice_id: "a".into(),
            name: "A".into(),
            labels: [
                ("use_case", "social_media"),
                ("gender", "female"),
                ("language", "en"),
                ("zz", "Female"),
                ("accent", "british"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            ..Default::default()
        };
        assert_eq!(label_chips(&v), ["female", "british", "social media"]);
        assert_eq!(count_text(0), "No voices in your account yet");
        assert_eq!(count_text(1), "1 voice in your account");
        assert_eq!(count_text(38), "38 voices in your account");
    }
}
