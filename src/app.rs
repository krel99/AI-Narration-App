//! The egui UI. One chapter is worked on at a time, in two tabs: Text (the
//! chapter as plain prose) and Review (the keyboard-first segment list with
//! its detail panel).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use eframe::egui::{self, Color32, Key, RichText, Sense, Stroke, UiBuilder, Vec2};

use crate::config::AppConfig;
use crate::db::{ChapterStats, Db, Library};
use crate::export;
use crate::lang;
use crate::model::*;
use crate::player::Player;
use crate::segment::{self, UNKNOWN_SPEAKER};
use crate::tags;
use crate::theme::{self, Palette, primary_button, speaker_color};
use crate::tts::VoiceInfo;
use crate::worker::{self, Command, Event, WorkerHandle};

mod check;
mod dialogue;
mod waveform;

const ROW_H: f32 = 46.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Text,
    Review,
}

#[derive(Default)]
struct ChapterView {
    chapter_id: Option<Id>,
    paragraphs: Vec<Paragraph>,
    /// All segments in reading order.
    segments: Vec<Segment>,
    /// paragraph id → index into `paragraphs`.
    para_index: HashMap<Id, usize>,
    take_counts: HashMap<Id, usize>,
}

impl ChapterView {
    fn is_first_in_paragraph(&self, i: usize) -> bool {
        i == 0 || self.segments[i - 1].paragraph_id != self.segments[i].paragraph_id
    }

    fn paragraph_of(&self, i: usize) -> Option<&Paragraph> {
        self.para_index
            .get(&self.segments[i].paragraph_id)
            .map(|&p| &self.paragraphs[p])
    }

    fn paragraph_range(&self, i: usize) -> std::ops::Range<usize> {
        let pid = self.segments[i].paragraph_id;
        let start = (0..=i)
            .rev()
            .take_while(|&j| self.segments[j].paragraph_id == pid)
            .last()
            .unwrap_or(i);
        let end = (i..self.segments.len())
            .take_while(|&j| self.segments[j].paragraph_id == pid)
            .last()
            .unwrap_or(i)
            + 1;
        start..end
    }

    fn has_audio(&self) -> bool {
        !self.take_counts.is_empty()
    }
}

/// The Text tab's editable copy of the chapter.
#[derive(Default)]
struct TextDraft {
    /// `None` while writing a new chapter that has not been saved yet.
    chapter_id: Option<Id>,
    title: String,
    text: String,
    saved_title: String,
    saved_text: String,
}

impl TextDraft {
    fn text_dirty(&self) -> bool {
        self.text != self.saved_text
    }

    fn dirty(&self) -> bool {
        self.text_dirty() || self.title != self.saved_title
    }
}

struct GeneratePlan {
    title: String,
    ids: Vec<Id>,
    reroll: bool,
    chars: usize,
    unassigned: usize,
}

struct VoiceRow {
    key: String,
    display_name: String,
    aliases: String,
    voice_id: String,
    character_id: Option<Id>,
    remove: bool,
}

struct VoiceEditor {
    rows: Vec<VoiceRow>,
    new_key: String,
    rename_from: String,
    rename_to: String,
}

#[derive(Default)]
struct Dialogs {
    new_project: Option<(String, String)>,
    settings: Option<AppConfig>,
    project_settings: Option<Project>,
    voices: Option<VoiceEditor>,
    split: Option<(Id, String, Option<String>)>,
    generate: Option<GeneratePlan>,
    delete_chapter: Option<Id>,
    delete_project: Option<Id>,
    resegment: Option<Id>,
    resplit: bool,
    log: bool,
}

/// The audio-tag editor in the detail panel.
struct TagEdit {
    segment_id: Id,
    text: String,
    error: Option<String>,
}

pub struct App {
    cfg: AppConfig,
    lib: Library,
    db: Db,
    worker: WorkerHandle,
    player: Player,

    tab: Tab,
    projects: Vec<Project>,
    project: Option<Project>,
    characters: Vec<Character>,
    chapters: Vec<Chapter>,
    stats: HashMap<Id, ChapterStats>,
    view: ChapterView,
    draft: TextDraft,
    selected: Option<usize>,
    scroll_to_selected: bool,
    scroll_offset: f32,
    viewport_h: f32,
    takes: Vec<Take>,
    takes_for: Option<Id>,
    tag_edit: Option<TagEdit>,
    wave: waveform::WaveState,
    check: check::CheckState,

    in_flight: HashSet<Id>,
    failures: HashMap<Id, String>,
    segmenting: Option<(Id, usize, usize)>,
    directing: Option<(Id, usize, usize)>,
    generating: Option<(usize, usize)>,
    exporting: bool,
    log: Vec<(bool, String)>,
    status: String,
    voices: Vec<VoiceInfo>,
    dirty: bool,
    dialogs: Dialogs,
}

fn status_icon(s: SegmentStatus, p: &Palette) -> (&'static str, Color32) {
    match s {
        SegmentStatus::Draft => ("○", p.draft),
        SegmentStatus::Generated => ("●", p.generated),
        SegmentStatus::Approved => ("✔", p.approved),
        SegmentStatus::Rejected => ("✖", p.rejected),
    }
}

/// Segment text as a layout job, with audio tags drawn as soft chips.
fn tagged_job(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    strong: bool,
    italic: bool,
) -> egui::text::LayoutJob {
    let p = theme::palette(ui);
    let base = if strong {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    let mut job = egui::text::LayoutJob::default();
    for (is_tag, piece) in tags::pieces(text) {
        let format = if is_tag {
            egui::TextFormat {
                font_id: egui::FontId::proportional(size - 1.5),
                color: p.tag,
                background: p.tag_bg,
                italics: true,
                ..Default::default()
            }
        } else {
            egui::TextFormat {
                font_id: egui::FontId::proportional(size),
                color: base,
                italics: italic,
                ..Default::default()
            }
        };
        job.append(piece, 0.0, format);
    }
    job
}

/// A small rounded label with a coloured outline.
fn chip(ui: &mut egui::Ui, text: impl Into<String>, color: Color32) -> egui::Response {
    egui::Frame::new()
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).color(color).small())
        })
        .response
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        let cfg = AppConfig::load();
        theme::apply(&cc.egui_ctx, cfg.dark_mode);
        let lib = Library::new(cfg.library_dir());
        let db = lib.open()?;
        let ctx = cc.egui_ctx.clone();
        let worker = worker::spawn(lib.clone(), cfg.api_keys(), move || ctx.request_repaint())?;
        let mut app = Self {
            cfg,
            lib,
            db,
            worker,
            player: Player::new(),
            tab: Tab::Text,
            projects: vec![],
            project: None,
            characters: vec![],
            chapters: vec![],
            stats: HashMap::new(),
            view: ChapterView::default(),
            draft: TextDraft::default(),
            selected: None,
            scroll_to_selected: false,
            scroll_offset: 0.0,
            viewport_h: 400.0,
            takes: vec![],
            takes_for: None,
            tag_edit: None,
            wave: Default::default(),
            check: Default::default(),
            in_flight: HashSet::new(),
            failures: HashMap::new(),
            segmenting: None,
            directing: None,
            generating: None,
            exporting: false,
            log: vec![],
            status: String::new(),
            voices: vec![],
            dirty: false,
            dialogs: Dialogs::default(),
        };
        app.worker
            .send(Command::Check(crate::check::CheckCommand::Config(
                app.cfg.check.clone(),
            )));
        if let Some(e) = app.player.error.clone() {
            app.note(true, e);
        }
        if !export::ffmpeg_available() {
            app.note(
                true,
                "ffmpeg not found on PATH; export will not work.".into(),
            );
        }
        app.reload_projects();
        let last = app
            .cfg
            .last_project
            .filter(|id| app.projects.iter().any(|p| p.id == *id));
        if let Some(id) = last.or_else(|| app.projects.first().map(|p| p.id)) {
            app.select_project(id);
        }
        Ok(app)
    }

    fn note(&mut self, error: bool, msg: String) {
        if error {
            log::warn!("{msg}");
        } else {
            log::info!("{msg}");
        }
        self.status = msg.clone();
        self.log.push((error, msg));
        if self.log.len() > 500 {
            self.log.remove(0);
        }
    }

    fn try_db<T>(&mut self, r: anyhow::Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.note(true, format!("{e:#}"));
                None
            }
        }
    }

    fn chapter_title(&self, id: Id) -> String {
        self.chapters
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.title.clone())
            .unwrap_or_default()
    }

    fn tags_active(&self) -> bool {
        self.project
            .as_ref()
            .is_some_and(|p| lang::model_supports_tags(&p.tts_model))
    }

    // ---- loading ----------------------------------------------------------

    fn reload_projects(&mut self) {
        let r = self.db.list_projects();
        self.projects = self.try_db(r).unwrap_or_default();
    }

    fn select_project(&mut self, id: Id) {
        let r = self.db.project(id);
        self.project = self.try_db(r);
        self.cfg.last_project = Some(id);
        let _ = self.cfg.save();
        self.view = ChapterView::default();
        self.selected = None;
        self.reload_project_data();
        match self.chapters.last().map(|c| c.id) {
            Some(latest) => self.open_chapter(latest),
            None => self.new_chapter(),
        }
    }

    fn reload_project_data(&mut self) {
        let Some(pid) = self.project.as_ref().map(|p| p.id) else {
            return;
        };
        if let Ok(p) = self.db.project(pid) {
            self.project = Some(p);
        }
        let r = self.db.characters(pid);
        self.characters = self.try_db(r).unwrap_or_default();
        let r = self.db.chapters(pid);
        self.chapters = self.try_db(r).unwrap_or_default();
        self.reload_stats();
    }

    fn reload_stats(&mut self) {
        let Some(project) = self.project.clone() else {
            return;
        };
        self.stats.clear();
        for c in &self.chapters {
            if let Ok(s) = self.db.chapter_stats(c.id, &project) {
                self.stats.insert(c.id, s);
            }
        }
    }

    fn open_chapter(&mut self, chapter_id: Id) {
        self.view.chapter_id = Some(chapter_id);
        self.selected = None;
        self.scroll_offset = 0.0;
        self.tag_edit = None;
        self.reload_view();
        if let Some(c) = self.chapters.iter().find(|c| c.id == chapter_id) {
            self.draft = TextDraft {
                chapter_id: Some(c.id),
                title: c.title.clone(),
                text: c.source_text.clone(),
                saved_title: c.title.clone(),
                saved_text: c.source_text.clone(),
            };
        }
        if !self.view.segments.is_empty() {
            self.selected = Some(0);
            self.tab = Tab::Review;
        } else {
            self.tab = Tab::Text;
        }
    }

    fn new_chapter(&mut self) {
        self.view = ChapterView::default();
        self.selected = None;
        self.tag_edit = None;
        let title = format!("Chapter {}", self.chapters.len() + 1);
        self.draft = TextDraft {
            chapter_id: None,
            title: title.clone(),
            saved_title: title,
            ..Default::default()
        };
        self.tab = Tab::Text;
    }

    fn reload_view(&mut self) {
        let Some(cid) = self.view.chapter_id else {
            return;
        };
        let selected_id = self.selected_segment().map(|s| s.id);
        let paragraphs = self.db.paragraphs(cid);
        let segments = self.db.chapter_segments(cid);
        let counts = self.db.take_counts(cid);
        let (Some(paragraphs), Some(segments), Some(counts)) = (
            self.try_db(paragraphs),
            self.try_db(segments),
            self.try_db(counts),
        ) else {
            return;
        };
        self.view.para_index = paragraphs
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id, i))
            .collect();
        self.view.paragraphs = paragraphs;
        self.view.segments = segments;
        self.view.take_counts = counts;
        // Keep the selection on the same segment, or the nearest row.
        self.selected = match selected_id {
            Some(id) => self.view.segments.iter().position(|s| s.id == id).or(self
                .selected
                .map(|i| i.min(self.view.segments.len().saturating_sub(1)))),
            None => self.selected,
        }
        .filter(|_| !self.view.segments.is_empty());
        self.takes_for = None;
        self.reload_checks();
    }

    fn selected_segment(&self) -> Option<&Segment> {
        self.selected.and_then(|i| self.view.segments.get(i))
    }

    fn ensure_takes(&mut self) {
        let id = self.selected_segment().map(|s| s.id);
        if self.takes_for != id {
            self.takes = id.and_then(|id| self.db.takes(id).ok()).unwrap_or_default();
            self.takes_for = id;
        }
    }

    // ---- worker events ------------------------------------------------------

    fn handle_events(&mut self) {
        while let Ok(ev) = self.worker.events.try_recv() {
            match ev {
                Event::Info(m) => self.note(false, m),
                Event::Error(m) => {
                    self.exporting = false;
                    self.note(true, m)
                }
                Event::SegmentationProgress {
                    chapter_id,
                    done,
                    total,
                } => {
                    self.segmenting = Some((chapter_id, done, total));
                }
                Event::SegmentationFinished {
                    chapter_id,
                    llm_paragraphs,
                    fallback_paragraphs,
                    input_tokens,
                    output_tokens,
                } => {
                    self.segmenting = None;
                    self.dirty = true;
                    let title = self.chapter_title(chapter_id);
                    let tokens = format!("{input_tokens} input / {output_tokens} output tokens");
                    let msg = if fallback_paragraphs > 0 {
                        format!(
                            "{title}: {llm_paragraphs} dialogue paragraphs attributed, {fallback_paragraphs} fell back to the parser (check 'unknown' speakers); {tokens}."
                        )
                    } else {
                        format!(
                            "{title}: {llm_paragraphs} dialogue paragraphs attributed; {tokens}."
                        )
                    };
                    self.note(fallback_paragraphs > 0, msg);
                }
                Event::DirectionProgress {
                    chapter_id,
                    done,
                    total,
                } => {
                    self.directing = Some((chapter_id, done, total));
                }
                Event::DirectionFinished {
                    chapter_id,
                    tagged,
                    rejected,
                    input_tokens,
                    output_tokens,
                } => {
                    self.directing = None;
                    self.dirty = true;
                    let rejected = if rejected > 0 {
                        format!(", {rejected} suggestions rejected for changing the text")
                    } else {
                        String::new()
                    };
                    self.note(
                        false,
                        format!(
                            "{}: audio tags added to {tagged} segments{rejected}; {input_tokens} input / {output_tokens} output tokens.",
                            self.chapter_title(chapter_id)
                        ),
                    );
                }
                Event::SegmentsChanged { chapter_id } => {
                    if self.view.chapter_id == Some(chapter_id) {
                        self.dirty = true;
                    } else {
                        self.reload_stats();
                    }
                }
                Event::GenerationStarted { segment_ids } => {
                    let (done, total) = self.generating.unwrap_or((0, 0));
                    self.generating = Some((done, total + segment_ids.len()));
                    for id in segment_ids {
                        self.failures.remove(&id);
                        self.in_flight.insert(id);
                    }
                }
                Event::SegmentGenerated { segment_id } => {
                    self.in_flight.remove(&segment_id);
                    if let Some((d, _)) = &mut self.generating {
                        *d += 1;
                    }
                }
                Event::GenerationFailed { segment_id, error } => {
                    self.in_flight.remove(&segment_id);
                    if let Some((d, _)) = &mut self.generating {
                        *d += 1;
                    }
                    self.failures.insert(segment_id, error);
                }
                Event::GenerationFinished {
                    generated,
                    cached,
                    failed,
                    credits,
                } => {
                    if self.in_flight.is_empty() {
                        self.generating = None;
                    }
                    self.note(
                        failed > 0,
                        format!(
                            "Generation finished: {generated} takes ({cached} from cache), {failed} failed, {credits} credits used."
                        ),
                    );
                }
                Event::ExportFinished {
                    files,
                    gaps,
                    duration_ms,
                } => {
                    self.exporting = false;
                    let list: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
                    let gaps = if gaps > 0 {
                        format!(", {gaps} gaps filled with silence")
                    } else {
                        String::new()
                    };
                    let secs = duration_ms / 1000;
                    let length = format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
                    self.note(
                        false,
                        format!("Exported ({length}{gaps}): {}", list.join(", ")),
                    );
                    self.dirty = true;
                }
                Event::Voices(v) => {
                    self.note(false, format!("Loaded {} voices from ElevenLabs.", v.len()));
                    self.voices = v;
                }
                ev @ (Event::TakeChecked { .. }
                | Event::CheckProgress { .. }
                | Event::ModelDownload { .. }
                | Event::ModelDownloadFinished { .. }) => self.on_check_event(ev),
            }
        }
        if self.dirty {
            self.dirty = false;
            self.reload_project_data();
            self.reload_view();
        }
    }

    // ---- actions ------------------------------------------------------------

    fn play_segment(&mut self, i: usize) {
        let Some(seg) = self.view.segments.get(i).cloned() else {
            return;
        };
        let Some(take_id) = seg.current_take_id else {
            self.note(true, "This segment has no audio yet.".into());
            return;
        };
        self.play_take(seg.id, take_id);
    }

    fn play_take(&mut self, segment_id: Id, take_id: Id) {
        let pcm = self.db.take(take_id).and_then(|t| self.take_audio(&t));
        if let Some(pcm) = self.try_db(pcm) {
            self.player.play(vec![(segment_id, pcm)], 0);
            self.wave.playing_take = Some(take_id);
        }
    }

    fn play_paragraph(&mut self, i: usize) {
        if i >= self.view.segments.len() {
            return;
        }
        let gap = self
            .project
            .as_ref()
            .map_or(0, |p| p.settings.silence.within_paragraph_ms);
        let mut items = Vec::new();
        for j in self.view.paragraph_range(i) {
            let seg = &self.view.segments[j];
            if let Some(t) = seg.current_take_id {
                match self.db.take(t).and_then(|t| self.take_audio(&t)) {
                    Ok(pcm) => items.push((seg.id, pcm)),
                    Err(e) => log::warn!("{e:#}"),
                }
            }
        }
        if items.is_empty() {
            self.note(true, "No audio in this paragraph yet.".into());
        } else {
            self.player.play(items, gap);
            self.wave.playing_take = None;
        }
    }

    fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.player.stop();
        } else if let Some(i) = self.selected {
            self.play_segment(i);
        }
    }

    fn approve(&mut self, i: usize) {
        let Some(seg) = self.view.segments.get(i).cloned() else {
            return;
        };
        if seg.current_take_id.is_none() {
            self.note(true, "Generate audio before approving.".into());
            return;
        }
        let r = self.db.set_segment_status(seg.id, SegmentStatus::Approved);
        self.try_db(r);
        self.reload_view();
        self.reload_stats();
        self.select(i + 1);
    }

    fn unapprove(&mut self, i: usize) {
        let Some(seg) = self.view.segments.get(i).cloned() else {
            return;
        };
        let status = if seg.current_take_id.is_some() {
            SegmentStatus::Generated
        } else {
            SegmentStatus::Draft
        };
        let r = self.db.set_segment_status(seg.id, status);
        self.try_db(r);
        self.dirty = true;
    }

    /// R: reject the current take and (by default) re-roll it with a new seed.
    fn reject(&mut self, i: usize) {
        let Some(seg) = self.view.segments.get(i).cloned() else {
            return;
        };
        if seg.status == SegmentStatus::Approved {
            self.note(
                true,
                "Approved segments are locked; unapprove first.".into(),
            );
            return;
        }
        if self.in_flight.contains(&seg.id) {
            return;
        }
        if seg.current_take_id.is_none() {
            self.queue_generate(vec![seg.id], false);
            return;
        }
        let r = self.db.set_segment_status(seg.id, SegmentStatus::Rejected);
        self.try_db(r);
        if self
            .project
            .as_ref()
            .is_some_and(|p| p.settings.reroll_on_reject)
        {
            self.queue_generate(vec![seg.id], true);
        }
        self.dirty = true;
    }

    fn queue_generate(&mut self, ids: Vec<Id>, reroll: bool) {
        if self.cfg.api_keys().elevenlabs.is_empty() {
            self.note(true, "Set the ElevenLabs API key in Settings first.".into());
            return;
        }
        self.worker.send(Command::Generate {
            segment_ids: ids,
            reroll,
        });
    }

    fn plan_generation(&mut self, reroll: bool) {
        let Some(project) = &self.project else { return };
        let wanted = if reroll {
            SegmentStatus::Rejected
        } else {
            SegmentStatus::Draft
        };
        let mut plan = GeneratePlan {
            title: if reroll {
                "Re-roll rejected segments".into()
            } else {
                "Generate draft segments".into()
            },
            ids: vec![],
            reroll,
            chars: 0,
            unassigned: 0,
        };
        for s in &self.view.segments {
            if s.status != wanted || self.in_flight.contains(&s.id) {
                continue;
            }
            if project.voice_for(&s.speaker).is_none() {
                plan.unassigned += 1;
                continue;
            }
            plan.chars += s.spoken_text(&project.tts_model).chars().count();
            plan.ids.push(s.id);
        }
        self.dialogs.generate = Some(plan);
    }

    fn segment_chapter(&mut self, chapter_id: Id) {
        if self.cfg.api_keys().anthropic.is_empty() {
            self.note(
                true,
                "No Anthropic API key: dialogue will be split by the parser only, with speaker 'unknown'.".into(),
            );
        }
        self.worker.send(Command::SegmentChapter { chapter_id });
        self.tab = Tab::Review;
    }

    fn select(&mut self, i: usize) {
        if self.view.segments.is_empty() {
            self.selected = None;
            return;
        }
        self.selected = Some(i.min(self.view.segments.len() - 1));
        self.scroll_to_selected = true;
    }

    fn select_paragraph(&mut self, forward: bool) {
        let Some(i) = self.selected else {
            return self.select(0);
        };
        let range = self.view.paragraph_range(i);
        if forward {
            if range.end < self.view.segments.len() {
                self.select(range.end);
            }
        } else if range.start > 0 {
            let prev = self.view.paragraph_range(range.start - 1);
            self.select(prev.start);
        } else {
            self.select(0);
        }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if self.tab != Tab::Review || ctx.text_edit_focused() || self.any_dialog_open() {
            return;
        }
        let (space, a, r, p, up, down, left, right) = ctx.input(|i| {
            (
                i.key_pressed(Key::Space),
                i.key_pressed(Key::A),
                i.key_pressed(Key::R),
                i.key_pressed(Key::P),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
            )
        });
        let cur = self.selected.unwrap_or(0);
        if space {
            self.toggle_play();
        }
        if p {
            self.play_paragraph(cur);
        }
        if a {
            self.approve(cur);
        }
        if r {
            self.reject(cur);
        }
        if up {
            self.select(cur.saturating_sub(1));
        }
        if down {
            self.select(if self.selected.is_some() { cur + 1 } else { 0 });
        }
        if left {
            self.select_paragraph(false);
        }
        if right {
            self.select_paragraph(true);
        }
    }

    fn any_dialog_open(&self) -> bool {
        let d = &self.dialogs;
        d.new_project.is_some()
            || d.settings.is_some()
            || d.project_settings.is_some()
            || d.voices.is_some()
            || d.split.is_some()
            || d.generate.is_some()
            || d.delete_chapter.is_some()
            || d.delete_project.is_some()
            || d.resegment.is_some()
            || d.resplit
    }

    /// Save the Text tab. Returns false if nothing could be saved yet.
    fn save_text(&mut self, confirmed: bool) -> bool {
        let Some(project) = self.project.clone() else {
            return false;
        };
        let title = if self.draft.title.trim().is_empty() {
            format!("Chapter {}", self.chapters.len() + 1)
        } else {
            self.draft.title.trim().to_string()
        };
        match self.draft.chapter_id {
            None => {
                let r = self.db.import_chapter(project.id, &title, &self.draft.text);
                let Some(id) = self.try_db(r) else {
                    return false;
                };
                self.reload_project_data();
                self.open_chapter(id);
                self.tab = Tab::Text;
                true
            }
            Some(id) => {
                if self.draft.text_dirty() {
                    if !self.view.segments.is_empty() && !confirmed {
                        self.dialogs.resplit = true;
                        return false;
                    }
                    let r = self.db.replace_chapter_text(id, &title, &self.draft.text);
                    if self.try_db(r).is_none() {
                        return false;
                    }
                } else if self.draft.title != self.draft.saved_title {
                    let r = self.db.rename_chapter(id, &title);
                    self.try_db(r);
                }
                self.reload_project_data();
                self.open_chapter(id);
                self.tab = Tab::Text;
                true
            }
        }
    }

    fn open_text_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Text", &["txt", "md"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                if self.draft.chapter_id.is_some() && self.view.has_audio() {
                    self.new_chapter();
                }
                if let Some(stem) = path.file_stem() {
                    self.draft.title = stem.to_string_lossy().to_string();
                }
                self.draft.text = text;
            }
            Err(e) => self.note(true, format!("{}: {e}", path.display())),
        }
    }

    fn open_voice_editor(&mut self) {
        let Some(project) = &self.project else { return };
        let mut rows: Vec<VoiceRow> = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |rows: &mut Vec<VoiceRow>,
                        key: &str,
                        name: &str,
                        aliases: &[String],
                        cid: Option<Id>| {
            if seen.insert(key.to_string()) {
                rows.push(VoiceRow {
                    key: key.into(),
                    display_name: name.into(),
                    aliases: aliases.join(", "),
                    voice_id: project.voice_map.get(key).cloned().unwrap_or_default(),
                    character_id: cid,
                    remove: false,
                });
            }
        };
        push(&mut rows, NARRATOR, "Narrator", &[], None);
        for c in &self.characters {
            push(&mut rows, &c.key, &c.display_name, &c.aliases, Some(c.id));
        }
        for s in self.db.speakers_in_use(project.id).unwrap_or_default() {
            push(&mut rows, &s, "", &[], None);
        }
        for k in project.voice_map.keys() {
            push(&mut rows, k, "", &[], None);
        }
        self.dialogs.voices = Some(VoiceEditor {
            rows,
            new_key: String::new(),
            rename_from: String::new(),
            rename_to: String::new(),
        });
    }

    // ---- top bar --------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = *theme::palette(ui);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("◖ Narration Studio")
                    .strong()
                    .size(16.0)
                    .color(p.accent),
            );
            ui.add_space(14.0);

            let current = self
                .project
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "No project".into());
            let mut pick = None;
            egui::ComboBox::from_id_salt("project")
                .selected_text(current)
                .width(170.0)
                .show_ui(ui, |ui| {
                    for p in &self.projects {
                        let sel = self.project.as_ref().is_some_and(|c| c.id == p.id);
                        if ui.selectable_label(sel, &p.name).clicked() {
                            pick = Some(p.id);
                        }
                    }
                    ui.separator();
                    if ui.button("＋ New project…").clicked() {
                        self.dialogs.new_project = Some((String::new(), "en".into()));
                    }
                });
            if let Some(id) = pick {
                self.select_project(id);
            }

            if self.project.is_some() {
                ui.label(RichText::new("›").color(p.muted));
                let chapter_label = match self.draft.chapter_id {
                    Some(id) => self
                        .chapters
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| c.title.clone())
                        .unwrap_or_default(),
                    None => "New chapter".into(),
                };
                let mut open = None;
                let mut new = false;
                egui::ComboBox::from_id_salt("chapter")
                    .selected_text(chapter_label)
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for c in self.chapters.iter().rev() {
                            let st = self.stats.get(&c.id).cloned().unwrap_or_default();
                            let progress = if st.segments == 0 {
                                "text only".to_string()
                            } else {
                                format!("{}/{} approved", st.approved, st.segments)
                            };
                            let sel = self.draft.chapter_id == Some(c.id);
                            if ui
                                .selectable_label(sel, format!("{}  ·  {progress}", c.title))
                                .clicked()
                            {
                                open = Some(c.id);
                            }
                        }
                        ui.separator();
                        if ui.button("＋ New chapter").clicked() {
                            new = true;
                        }
                    });
                if let Some(id) = open {
                    self.open_chapter(id);
                }
                if new {
                    self.new_chapter();
                }
                ui.menu_button("⋯", |ui| {
                    let ready = self
                        .stats
                        .values()
                        .any(|s| s.segments > 0 && s.approved == s.segments);
                    if ui
                        .add_enabled(
                            ready && !self.exporting,
                            egui::Button::new("Export book (M4B)"),
                        )
                        .on_hover_text(
                            "All fully approved chapters as one M4B with chapter markers",
                        )
                        .clicked()
                    {
                        if let Some(p) = &self.project {
                            self.exporting = true;
                            self.worker.send(Command::ExportBook { project_id: p.id });
                        }
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.draft.chapter_id.is_some(),
                            egui::Button::new("Delete chapter…"),
                        )
                        .clicked()
                    {
                        self.dialogs.delete_chapter = self.draft.chapter_id;
                        ui.close();
                    }
                });

                ui.add_space(18.0);
                let has_segments = !self.view.segments.is_empty();
                for (tab, label) in [(Tab::Text, "1  Text"), (Tab::Review, "2  Review")] {
                    let enabled = tab == Tab::Text || has_segments || self.segmenting.is_some();
                    let selected = self.tab == tab;
                    let text = if selected {
                        RichText::new(label).strong().color(p.accent)
                    } else {
                        RichText::new(label)
                    };
                    if ui
                        .add_enabled(enabled, egui::Button::selectable(selected, text))
                        .clicked()
                    {
                        self.tab = tab;
                    }
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    self.dialogs.settings = Some(self.cfg.clone());
                }
                let has_project = self.project.is_some();
                if ui
                    .add_enabled(has_project, egui::Button::new("Project"))
                    .clicked()
                {
                    self.dialogs.project_settings = self.project.clone();
                }
                if ui
                    .add_enabled(has_project, egui::Button::new("Voices"))
                    .clicked()
                {
                    self.open_voice_editor();
                }
            });
        });
        ui.add_space(6.0);
    }

    // ---- Text tab ---------------------------------------------------------------

    fn text_tab(&mut self, ui: &mut egui::Ui) {
        let p = *theme::palette(ui);
        if self.project.is_none() {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("Welcome");
                ui.label("Create a project to start narrating. A project holds the language, voices and settings.");
                ui.add_space(10.0);
                if ui.add(primary_button(ui, "Create project")).clicked() {
                    self.dialogs.new_project = Some((String::new(), "en".into()));
                }
            });
            return;
        }
        let locked = self.draft.chapter_id.is_some() && self.view.has_audio();
        let width = ui.available_width().min(920.0);
        let full = ui.available_rect_before_wrap();
        let column = egui::Rect::from_center_size(
            egui::pos2(full.center().x, full.center().y),
            Vec2::new(width, full.height()),
        );
        ui.scope_builder(UiBuilder::new().max_rect(column), |ui| {
            {
                ui.add_space(14.0);
                ui.label(RichText::new("CHAPTER TITLE").small().color(p.muted));
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.title)
                        .font(egui::FontId::proportional(20.0))
                        .desired_width(width)
                        .margin(egui::Margin::symmetric(10, 6)),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("TEXT").small().color(p.muted));
                    ui.label(
                        RichText::new("Paste the chapter as plain prose. Blank lines separate paragraphs; a line of *** marks a scene break.")
                            .small()
                            .color(p.muted),
                    );
                });
                if locked {
                    ui.label(
                        RichText::new("This chapter has audio, so its text is locked. Start a new chapter from the chapter menu to narrate different text.")
                            .small()
                            .color(p.warning),
                    );
                }
                let footer_h = 56.0;
                let editor_h = (ui.available_height() - footer_h).max(160.0);
                egui::Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::same(4))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("text_editor")
                            .max_height(editor_h)
                            .min_scrolled_height(editor_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let rows = (editor_h / 22.0) as usize;
                                ui.add_enabled(
                                    !locked,
                                    egui::TextEdit::multiline(&mut self.draft.text)
                                        .font(egui::FontId::proportional(16.0))
                                        .frame(egui::Frame::NONE)
                                        .margin(egui::Margin::symmetric(14, 12))
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(rows)
                                        .hint_text("Paste or type the chapter text here…"),
                                );
                            });
                    });
                ui.add_space(8.0);
                let paragraphs = segment::split_paragraphs(&self.draft.text);
                let lang = lang::by_code(self.project.as_ref().map_or("en", |p| &p.language));
                let with_dialogue = paragraphs.iter().filter(|p| segment::has_dialogue(&p.text, lang)).count();
                let chars: usize = paragraphs.iter().map(|p| p.text.chars().count()).sum();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} paragraphs · {} with dialogue · {} characters",
                            paragraphs.len(),
                            with_dialogue,
                            chars
                        ))
                        .color(p.muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_segment = !paragraphs.is_empty() && !locked && self.segmenting.is_none();
                        let label = if self.tags_active()
                            && self.project.as_ref().is_some_and(|p| p.settings.audio_tags.enabled)
                        {
                            "Segment & add audio tags  →"
                        } else {
                            "Segment speakers  →"
                        };
                        if ui
                            .add_enabled(can_segment, primary_button(ui, label))
                            .on_hover_text("Saves the text, splits it into narration and dialogue, and attributes speakers with Claude")
                            .clicked()
                            && (!self.draft.dirty() || self.save_text(false))
                            && let Some(id) = self.draft.chapter_id {
                                self.segment_chapter(id);
                            }
                        if ui
                            .add_enabled(self.draft.dirty() && !paragraphs.is_empty(), egui::Button::new("Save"))
                            .clicked()
                        {
                            self.save_text(false);
                        }
                        if ui.add_enabled(!locked, egui::Button::new("Open text file…")).clicked() {
                            self.open_text_file();
                        }
                    });
                });
            }
        });
    }

    // ---- Review tab -------------------------------------------------------------

    fn review_toolbar(&mut self, ui: &mut egui::Ui) {
        let Some(cid) = self.view.chapter_id else {
            return;
        };
        let p = *theme::palette(ui);
        let st = self.stats.get(&cid).cloned().unwrap_or_default();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading(&self.draft.saved_title);
            ui.add_space(10.0);
            if st.segments > 0 {
                let frac = st.approved as f32 / st.segments as f32;
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(150.0)
                        .desired_height(8.0)
                        .fill(p.approved)
                        .corner_radius(4),
                );
                ui.label(
                    RichText::new(format!("{} / {} approved", st.approved, st.segments)).strong(),
                );
                if st.rejected > 0 {
                    chip(ui, format!("{} rejected", st.rejected), p.rejected);
                }
                self.check_chip(ui);
                if st.unassigned > 0 {
                    chip(
                        ui,
                        format!("⚠ {} unassigned voice", st.unassigned),
                        p.warning,
                    )
                    .on_hover_text("Map these speakers in Voices before generating");
                }
                chip(
                    ui,
                    format!(
                        "{}{} credits · re-roll {:.0}%",
                        if st.credits_estimated { "≈" } else { "" },
                        st.credits_used,
                        st.reroll_rate() * 100.0
                    ),
                    p.muted,
                )
                .on_hover_text(if st.credits_estimated {
                    "Some takes came back without a credit count; their cost is estimated."
                } else {
                    "Credits as reported by ElevenLabs."
                });
                if !self.tags_active() {
                    chip(ui, "model ignores audio tags", p.muted).on_hover_text(
                        "Choose Eleven v4 or v3 in Project settings to perform [tags]",
                    );
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            match self.segmenting.filter(|(c, _, _)| *c == cid) {
                Some((_, d, t)) => {
                    ui.add(egui::Spinner::new());
                    ui.label(format!("Segmenting {d}/{t} dialogue paragraphs…"));
                }
                None => {
                    if ui
                        .button("Re-segment drafts")
                        .on_hover_text("Re-run speaker attribution for paragraphs that have no audio yet")
                        .clicked()
                    {
                        self.dialogs.resegment = Some(cid);
                    }
                }
            }
            match self.directing.filter(|(c, _, _)| *c == cid) {
                Some((_, d, t)) => {
                    ui.add(egui::Spinner::new());
                    ui.label(format!("Adding audio tags {d}/{t}…"));
                }
                None => {
                    if ui
                        .add_enabled(st.draft > 0 && self.segmenting.is_none(), egui::Button::new("✦ Audio tags"))
                        .on_hover_text("Let Claude (re)write [whispers], [long pause]… into draft segments, narration included")
                        .clicked()
                    {
                        self.worker.send(Command::DirectChapter { chapter_id: cid });
                    }
                }
            }
            ui.separator();
            if ui
                .add_enabled(st.draft > 0, primary_button(ui, &format!("Generate {} drafts", st.draft)))
                .clicked()
            {
                self.plan_generation(false);
            }
            if ui
                .add_enabled(st.rejected > 0, egui::Button::new(format!("Re-roll {} rejected", st.rejected)))
                .clicked()
            {
                self.plan_generation(true);
            }
            if let Some((d, t)) = self.generating {
                ui.add(egui::Spinner::new());
                ui.label(format!("{d}/{t}"));
                if ui.small_button("Cancel").clicked() {
                    self.worker.send(Command::Cancel);
                }
            }
            self.auto_trim_chapter_button(ui, cid);
            self.check_toolbar(ui, cid);
            ui.separator();
            let complete = st.segments > 0 && st.approved == st.segments;
            if ui
                .add_enabled(complete && !self.exporting, egui::Button::new("Export chapter"))
                .on_hover_text("MP3 192 kbps (ACX) + FLAC master, loudness-normalised")
                .clicked()
            {
                self.exporting = true;
                self.worker.send(Command::ExportChapter { chapter_id: cid, draft: false });
            }
            if ui
                .add_enabled(st.segments > 0 && !self.exporting, egui::Button::new("Draft export"))
                .on_hover_text("Uses current takes even if unapproved; gaps become silence")
                .clicked()
            {
                self.exporting = true;
                self.worker.send(Command::ExportChapter { chapter_id: cid, draft: true });
            }
            if self.exporting {
                ui.add(egui::Spinner::new());
            }
        });
        ui.add_space(6.0);
    }

    fn segment_list(&mut self, ui: &mut egui::Ui) {
        let p = *theme::palette(ui);
        let n = self.view.segments.len();
        if n == 0 {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                if self.segmenting.is_some() {
                    ui.add(egui::Spinner::new().size(28.0));
                    ui.label("Splitting the text into segments and attributing speakers…");
                } else {
                    ui.label(format!(
                        "{} paragraphs, not segmented yet.",
                        self.view.paragraphs.len()
                    ));
                    if let Some(cid) = self.view.chapter_id
                        && ui.add(primary_button(ui, "Segment speakers")).clicked()
                    {
                        self.segment_chapter(cid);
                    }
                }
            });
            return;
        }
        let spacing = ui.spacing().item_spacing.y;
        let row_full = ROW_H + spacing;
        let mut area = egui::ScrollArea::vertical()
            .id_salt("segments")
            .auto_shrink([false, false]);
        if self.scroll_to_selected {
            self.scroll_to_selected = false;
            if let Some(i) = self.selected {
                let top = i as f32 * row_full;
                let mut off = self.scroll_offset;
                if top < off {
                    off = top;
                } else if top + row_full > off + self.viewport_h {
                    off = top + row_full - self.viewport_h;
                }
                area = area.vertical_scroll_offset(off.max(0.0));
            }
        }
        let playing = self.player.current();
        let project = self.project.clone();
        let tags_active = self.tags_active();
        let dark = ui.visuals().dark_mode;
        let mut clicked: Option<usize> = None;
        let mut act: Option<(usize, &'static str)> = None;
        let out = area.show_rows(ui, ROW_H, n, |ui, range| {
            for i in range {
                let seg = &self.view.segments[i];
                let w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click());
                if resp.clicked() {
                    clicked = Some(i);
                    // Keep Space/A/R for the review shortcuts, not the row widget.
                    resp.surrender_focus();
                }
                let first = self.view.is_first_in_paragraph(i);
                let selected = self.selected == Some(i);
                let fill = if Some(seg.id) == playing {
                    p.playing
                } else if selected {
                    p.accent_soft
                } else if resp.hovered() {
                    ui.visuals().faint_bg_color
                } else {
                    p.card
                };
                ui.painter().rect_filled(rect, 6.0, fill);
                if selected {
                    let bar = egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height()));
                    ui.painter().rect_filled(bar, 2.0, p.accent);
                }
                if first && i > 0 {
                    let y = rect.top() - spacing / 2.0;
                    ui.painter()
                        .hline(rect.x_range(), y, Stroke::new(1.0, p.border));
                }
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.add_space(8.0);
                        let para = self.view.paragraph_of(i);
                        let ptext = match (first, para) {
                            (true, Some(p)) if p.scene_break_before => format!("✱ {}", p.order + 1),
                            (true, Some(p)) => format!("¶ {}", p.order + 1),
                            _ => String::new(),
                        };
                        left_aligned(ui, 40.0, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(ptext).small().color(p.muted))
                                    .selectable(false),
                            )
                        });
                        let (icon, color) = if self.in_flight.contains(&seg.id) {
                            ("◌", p.working)
                        } else {
                            status_icon(seg.status, &p)
                        };
                        let status_label = left_aligned(ui, 18.0, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(icon).color(color))
                                    .selectable(false),
                            )
                        });
                        if let Some(err) = self.failures.get(&seg.id) {
                            status_label.on_hover_text(format!("Last generation failed: {err}"));
                        }
                        self.check_marker(ui, seg);
                        let unassigned = project
                            .as_ref()
                            .is_none_or(|p| p.voice_for(&seg.speaker).is_none());
                        let badge = if unassigned {
                            RichText::new(format!("⚠ {}", seg.speaker)).color(p.warning)
                        } else {
                            RichText::new(&seg.speaker).color(speaker_color(&seg.speaker, dark))
                        };
                        let badge_resp = left_aligned(ui, 112.0, |ui| {
                            ui.add(
                                egui::Label::new(badge.strong())
                                    .truncate()
                                    .selectable(false),
                            )
                        });
                        if unassigned {
                            badge_resp.on_hover_text(
                                "Unassigned voice: map this speaker in Voices before generating",
                            );
                        }
                        let buttons_w = 150.0;
                        let text_w = (ui.available_width() - buttons_w).max(80.0);
                        let shown = seg.performance.as_deref().unwrap_or(&seg.text);
                        let mut job = tagged_job(
                            ui,
                            shown,
                            14.5,
                            seg.kind == SegmentKind::Dialogue,
                            seg.kind == SegmentKind::Narration && seg.text.chars().count() < 40,
                        );
                        if !tags_active && seg.performance.is_some() {
                            // Tags exist but the model will not perform them: dim them.
                            for s in &mut job.sections {
                                if s.format.color == p.tag {
                                    s.format.color = p.muted;
                                }
                            }
                        }
                        job.wrap.max_width = text_w;
                        job.wrap.max_rows = 2;
                        job.wrap.overflow_character = Some('…');
                        left_aligned(ui, text_w, |ui| {
                            ui.add(egui::Label::new(job).wrap().selectable(false))
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(6.0);
                            let takes = self.view.take_counts.get(&seg.id).copied().unwrap_or(0);
                            let locked = seg.status == SegmentStatus::Approved;
                            if ui
                                .add_enabled(!locked, egui::Button::new("↻").small())
                                .on_hover_text("Reject and re-roll (R)")
                                .clicked()
                            {
                                act = Some((i, "reject"));
                            }
                            if ui
                                .add_enabled(
                                    !locked && seg.current_take_id.is_some(),
                                    egui::Button::new("✔").small(),
                                )
                                .on_hover_text("Approve (A)")
                                .clicked()
                            {
                                act = Some((i, "approve"));
                            }
                            if ui
                                .add_enabled(
                                    seg.current_take_id.is_some(),
                                    egui::Button::new("▶").small(),
                                )
                                .on_hover_text("Play (Space)")
                                .clicked()
                            {
                                act = Some((i, "play"));
                            }
                            if takes > 0 {
                                ui.label(RichText::new(format!("{takes}×")).small().color(p.muted));
                            }
                        });
                    },
                );
            }
        });
        self.scroll_offset = out.state.clamped_offset().y;
        self.viewport_h = out.inner_rect.height();
        if let Some(i) = clicked {
            self.selected = Some(i);
        }
        if let Some((i, a)) = act {
            self.selected = Some(i);
            match a {
                "reject" => self.reject(i),
                "approve" => self.approve(i),
                _ => self.play_segment(i),
            }
        }
    }

    fn detail_panel(&mut self, ui: &mut egui::Ui) {
        self.ensure_takes();
        let p = *theme::palette(ui);
        let Some(i) = self.selected else {
            ui.label(
                RichText::new("Select a segment. Keys: Space play · P paragraph · A approve · R reject/re-roll · ↑↓ segments · ←→ paragraphs")
                    .color(p.muted),
            );
            return;
        };
        let Some(seg) = self.view.segments.get(i).cloned() else {
            return;
        };
        let Some(project) = self.project.clone() else {
            return;
        };
        let locked = seg.status == SegmentStatus::Approved;
        let range = self.view.paragraph_range(i);
        let last_in_para = i + 1 == range.end;
        let dark = ui.visuals().dark_mode;
        if self
            .tag_edit
            .as_ref()
            .is_none_or(|t| t.segment_id != seg.id)
        {
            self.tag_edit = Some(TagEdit {
                segment_id: seg.id,
                text: seg.performance.clone().unwrap_or_else(|| seg.text.clone()),
                error: None,
            });
        }

        egui::ScrollArea::vertical()
            .id_salt("detail")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let (icon, color) = status_icon(seg.status, &p);
                    chip(ui, format!("{icon} {}", seg.status.as_str()), color);
                    ui.label(RichText::new(&seg.speaker).strong().color(speaker_color(&seg.speaker, dark)));
                    ui.label(
                        RichText::new(format!("{} · {} characters", seg.kind.as_str(), seg.spoken_text(&project.tts_model).chars().count()))
                            .color(p.muted),
                    );
                    if let Some(e) = self.failures.get(&seg.id) {
                        ui.colored_label(p.rejected, format!("Last attempt failed: {e}"));
                    }
                });
                ui.add_space(4.0);
                let shown = seg.performance.as_deref().unwrap_or(&seg.text);
                ui.add(egui::Label::new(tagged_job(ui, shown, 17.0, false, false)).wrap());
                ui.add_space(8.0);

                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Speaker").color(p.muted));
                    let mut speaker = seg.speaker.clone();
                    ui.add_enabled_ui(!locked, |ui| {
                        egui::ComboBox::from_id_salt(("speaker", seg.id))
                            .selected_text(&speaker)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut speaker, NARRATOR.to_string(), NARRATOR);
                                for c in &self.characters {
                                    ui.selectable_value(&mut speaker, c.key.clone(), format!("{} ({})", c.key, c.display_name));
                                }
                                ui.selectable_value(&mut speaker, UNKNOWN_SPEAKER.to_string(), UNKNOWN_SPEAKER);
                            });
                    });
                    if speaker != seg.speaker {
                        let r = self.db.set_segment_speaker(seg.id, &speaker);
                        self.try_db(r);
                        self.dirty = true;
                    }
                    let mut kind = seg.kind;
                    ui.add_enabled_ui(!locked, |ui| {
                        ui.selectable_value(&mut kind, SegmentKind::Narration, "narration");
                        ui.selectable_value(&mut kind, SegmentKind::Dialogue, "dialogue");
                    });
                    if kind != seg.kind {
                        let r = self.db.set_segment_kind(seg.id, kind);
                        self.try_db(r);
                        self.dirty = true;
                    }
                    ui.separator();
                    if locked {
                        if ui.button("Unapprove").clicked() {
                            self.unapprove(i);
                        }
                    } else {
                        if ui
                            .add_enabled(seg.current_take_id.is_some(), egui::Button::new("✔ Approve"))
                            .clicked()
                        {
                            self.approve(i);
                        }
                        if ui
                            .button(if seg.current_take_id.is_some() { "↻ Re-roll" } else { "Generate" })
                            .clicked()
                        {
                            self.reject(i);
                        }
                    }
                    if ui.button("▶ Paragraph").clicked() {
                        self.play_paragraph(i);
                    }
                    ui.separator();
                    if ui.add_enabled(!locked && !last_in_para, egui::Button::new("Merge with next")).clicked() {
                        let r = self.db.merge_with_next(seg.id);
                        self.try_db(r);
                        self.dirty = true;
                    }
                    if ui
                        .add_enabled(!locked && i > range.start, egui::Button::new("Merge with previous"))
                        .clicked()
                    {
                        let prev = self.view.segments[i - 1].id;
                        let r = self.db.merge_with_next(prev);
                        if self.try_db(r).is_some() {
                            self.selected = Some(i - 1);
                        }
                        self.dirty = true;
                    }
                    if ui.add_enabled(!locked, egui::Button::new("Split…")).clicked() {
                        self.dialogs.split = Some((seg.id, seg.text.clone(), None));
                    }
                });
                self.waveform_section(ui, &seg, locked);
                self.check_section(ui, &seg, locked, i);

                ui.add_space(6.0);
                egui::CollapsingHeader::new(RichText::new("✦ Audio tags").strong())
                    .id_salt("tags_header")
                    .default_open(true)
                    .show(ui, |ui| {
                        if !lang::model_supports_tags(&project.tts_model) {
                            ui.label(
                                RichText::new(format!("{} reads tags aloud instead of performing them; tags are left out of its requests.", project.tts_model))
                                    .small()
                                    .color(p.warning),
                            );
                        }
                        ui.label(
                            RichText::new("Insert cues in square brackets, e.g. [whispers], [thoughtful], [short pause], [long pause], [sighs]. The words themselves cannot change.")
                                .small()
                                .color(p.muted),
                        );
                        let mut apply = false;
                        let mut clear = false;
                        if let Some(edit) = &mut self.tag_edit {
                            ui.add_enabled(
                                !locked,
                                egui::TextEdit::multiline(&mut edit.text)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .font(egui::FontId::proportional(15.0)),
                            );
                            if let Some(e) = &edit.error {
                                ui.colored_label(p.rejected, e);
                            }
                            ui.horizontal(|ui| {
                                let current = seg.performance.clone().unwrap_or_else(|| seg.text.clone());
                                apply = ui
                                    .add_enabled(!locked && edit.text != current, egui::Button::new("Apply tags"))
                                    .clicked();
                                clear = ui
                                    .add_enabled(!locked && seg.performance.is_some(), egui::Button::new("Remove tags"))
                                    .clicked();
                                if seg.current_take_id.is_some() && !locked {
                                    ui.label(RichText::new("Changing tags needs a new take.").small().color(p.muted));
                                }
                            });
                        }
                        if apply && let Some(edit) = &mut self.tag_edit {
                            match tags::validate(&seg.text, &edit.text) {
                                Ok(perf) => {
                                    let r = self.db.set_segment_performance(seg.id, perf.as_deref());
                                    self.try_db(r);
                                    self.tag_edit = None;
                                    self.dirty = true;
                                }
                                Err(e) => edit.error = Some(format!("Not applied: {e}.")),
                            }
                        }
                        if clear {
                            let r = self.db.set_segment_performance(seg.id, None);
                            self.try_db(r);
                            self.tag_edit = None;
                            self.dirty = true;
                        }
                    });

                ui.add_enabled_ui(!locked, |ui| {
                    let mut use_override = seg.settings_override.is_some();
                    let mut vs = seg
                        .settings_override
                        .clone()
                        .unwrap_or_else(|| project.default_voice_settings.clone());
                    let before = (use_override, vs.clone());
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut use_override, "Override voice settings");
                        if use_override {
                            voice_settings_ui(ui, &mut vs);
                        }
                    });
                    if (use_override, vs.clone()) != before {
                        let r = self.db.set_segment_override(seg.id, use_override.then_some(&vs));
                        self.try_db(r);
                        self.dirty = true;
                    }
                });

                ui.add_space(6.0);
                ui.label(RichText::new(format!("TAKES ({})", self.takes.len())).small().color(p.muted));
                let mut play: Option<Id> = None;
                let mut use_take: Option<Id> = None;
                for (n, t) in self.takes.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let current = seg.current_take_id == Some(t.id);
                        if ui.small_button("▶").clicked() {
                            play = Some(t.id);
                        }
                        let cost = if t.credits_used == 0 {
                            "cached".to_string()
                        } else if t.credits_exact {
                            format!("{} credits", t.credits_used)
                        } else {
                            format!("≈{} credits (estimated)", t.credits_used)
                        };
                        let text = format!("#{} · seed {} · {cost} · {}", n + 1, t.seed, t.created_at);
                        ui.label(if current { RichText::new(text).strong() } else { RichText::new(text) });
                        dialogue::take_chip(ui, t);
                        if current {
                            chip(ui, "current", p.approved);
                        } else if ui.add_enabled(!locked, egui::Button::new("Use").small()).clicked() {
                            use_take = Some(t.id);
                        }
                    });
                }
                if let Some(t) = play {
                    self.play_take(seg.id, t);
                }
                if let Some(t) = use_take {
                    let r = self.db.set_current_take(seg.id, t);
                    self.try_db(r);
                    self.dirty = true;
                }
            });
    }

    // ---- dialogs ------------------------------------------------------------

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.dialog_new_project(ctx);
        self.dialog_settings(ctx);
        self.dialog_project_settings(ctx);
        self.dialog_voices(ctx);
        self.dialog_split(ctx);
        self.dialog_generate(ctx);
        self.dialog_confirmations(ctx);
        if self.dialogs.log {
            let mut open = true;
            egui::Window::new("Log")
                .open(&mut open)
                .default_size([640.0, 320.0])
                .show(ctx, |ui| {
                    let p = *theme::palette(ui);
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for (err, m) in &self.log {
                                if *err {
                                    ui.colored_label(p.rejected, m);
                                } else {
                                    ui.label(m);
                                }
                            }
                        });
                });
            self.dialogs.log = open;
        }
    }

    fn dialog_new_project(&mut self, ctx: &egui::Context) {
        let Some((name, language)) = &mut self.dialogs.new_project else {
            return;
        };
        let mut create = false;
        let mut cancel = false;
        egui::Window::new("New project")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
            .show(ctx, |ui| {
                egui::Grid::new("np").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(name);
                    ui.end_row();
                    ui.label("Language");
                    language_combo(ui, "np_lang", language);
                    ui.end_row();
                });
                ui.label(
                    RichText::new("One language per project; it decides quote styles and the default TTS model.")
                        .small()
                        .color(theme::palette(ui).muted),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    create = ui
                        .add_enabled(!name.trim().is_empty(), primary_button(ui, "Create"))
                        .clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if create {
            let (name, language) = self.dialogs.new_project.take().unwrap();
            let r = self.db.create_project(name.trim(), &language);
            if let Some(id) = self.try_db(r) {
                self.reload_projects();
                self.select_project(id);
            }
        } else if cancel {
            self.dialogs.new_project = None;
        }
    }

    fn dialog_settings(&mut self, ctx: &egui::Context) {
        let Some(cfg) = &mut self.dialogs.settings else {
            return;
        };
        let mut save = false;
        let mut cancel = false;
        let mut check_cmd = None;
        egui::Window::new("Settings").collapsible(false).show(ctx, |ui| {
            egui::Grid::new("settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Anthropic API key");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.anthropic_api_key)
                        .password(true)
                        .hint_text("or ANTHROPIC_API_KEY"),
                );
                ui.end_row();
                ui.label("ElevenLabs API key");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.elevenlabs_api_key)
                        .password(true)
                        .hint_text("or ELEVENLABS_API_KEY"),
                );
                ui.end_row();
                ui.label("Appearance");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut cfg.dark_mode, false, "Light");
                    ui.selectable_value(&mut cfg.dark_mode, true, "Dark");
                });
                ui.end_row();
                ui.label("Library");
                ui.label(cfg.library_dir().display().to_string());
                ui.end_row();
            });
            if ui.button("Choose library folder…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                cfg.library_dir = Some(dir);
            }
            ui.label(
                RichText::new("Keys are stored in the config file with owner-only permissions. A library change takes effect after restart.")
                    .small()
                    .color(theme::palette(ui).muted),
            );
            check_cmd = check::settings_section(ui, &mut cfg.check, &self.check, &self.lib);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                save = ui.add(primary_button(ui, "Save")).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if let Some(c) = check_cmd {
            self.send_check(c);
        }
        if save {
            let cfg = self.dialogs.settings.take().unwrap();
            self.worker.send(Command::SetKeys(cfg.api_keys()));
            self.worker
                .send(Command::Check(crate::check::CheckCommand::Config(
                    cfg.check.clone(),
                )));
            theme::apply(ctx, cfg.dark_mode);
            self.cfg = cfg;
            if let Err(e) = self.cfg.save() {
                self.note(true, format!("Saving settings: {e:#}"));
            }
        } else if cancel {
            self.dialogs.settings = None;
        }
    }

    fn dialog_project_settings(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.dialogs.project_settings else {
            return;
        };
        let mut save = false;
        let mut cancel = false;
        let mut delete = false;
        egui::Window::new("Project settings")
            .default_width(500.0)
            .show(ctx, |ui| {
                let muted = theme::palette(ui).muted;
                let section = |ui: &mut egui::Ui, title: &str| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(title).small().strong().color(muted));
                };
                egui::ScrollArea::vertical().max_height(600.0).show(ui, |ui| {
                    egui::Grid::new("ps").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut p.name);
                        ui.end_row();
                        ui.label("Language");
                        language_combo(ui, "ps_lang", &mut p.language);
                        ui.end_row();
                        ui.label("TTS model");
                        egui::ComboBox::from_id_salt("model")
                            .selected_text(&p.tts_model)
                            .show_ui(ui, |ui| {
                                for (id, desc) in lang::TTS_MODELS {
                                    ui.selectable_value(&mut p.tts_model, id.to_string(), format!("{id} — {desc}"));
                                }
                            });
                        ui.end_row();
                        ui.label("Output format");
                        egui::ComboBox::from_id_salt("fmt")
                            .selected_text(&p.settings.output_format)
                            .show_ui(ui, |ui| {
                                for (id, desc) in lang::OUTPUT_FORMATS {
                                    ui.selectable_value(&mut p.settings.output_format, id.to_string(), format!("{id} — {desc}"));
                                }
                            });
                        ui.end_row();
                        ui.label("Concurrent requests");
                        ui.add(egui::DragValue::new(&mut p.settings.concurrency).range(1..=15))
                            .on_hover_text("Your ElevenLabs plan's concurrency limit");
                        ui.end_row();
                        ui.label("Request stitching");
                        ui.checkbox(&mut p.settings.request_stitching, "send previous request ids");
                        ui.end_row();
                        ui.label("Reject key");
                        ui.checkbox(&mut p.settings.reroll_on_reject, "R re-rolls immediately");
                        ui.end_row();
                    });

                    section(ui, "AUDIO TAGS");
                    let t = &mut p.settings.audio_tags;
                    ui.checkbox(&mut t.enabled, "Add tags automatically after segmentation (Eleven v3/v4 only)");
                    ui.horizontal(|ui| {
                        ui.label("Density");
                        for d in ["light", "moderate", "rich"] {
                            ui.selectable_value(&mut t.density, d.to_string(), d);
                        }
                    });
                    ui.label("Director's notes");
                    ui.add(
                        egui::TextEdit::multiline(&mut t.direction)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .hint_text("e.g. Military sci-fi. Keep the narrator calm and clipped; save whispers for real tension."),
                    );

                    section(ui, "MULTI-SPEAKER DIALOGUE");
                    dialogue::settings_ui(ui, &mut p.settings.dialogue, &p.tts_model, &p.default_voice_settings);

                    section(ui, "DEFAULT VOICE SETTINGS");
                    ui.horizontal_wrapped(|ui| voice_settings_ui(ui, &mut p.default_voice_settings));

                    section(ui, "SILENCE (MS)");
                    let s = &mut p.settings.silence;
                    egui::Grid::new("silence").num_columns(2).show(ui, |ui| {
                        for (label, v) in [
                            ("Within paragraph", &mut s.within_paragraph_ms),
                            ("Between paragraphs", &mut s.between_paragraphs_ms),
                            ("Scene break", &mut s.scene_break_ms),
                            ("Chapter lead-in", &mut s.chapter_lead_in_ms),
                            ("Chapter tail", &mut s.chapter_tail_ms),
                        ] {
                            ui.label(label);
                            ui.add(egui::DragValue::new(v).range(0..=10000).speed(10));
                            ui.end_row();
                        }
                    });

                    section(ui, "SILENCE TRIMMING");
                    waveform::trim_settings_ui(ui, &mut p.settings.trim);

                    section(ui, "CLAUDE");
                    let g = &mut p.settings.segmentation;
                    egui::Grid::new("seg").num_columns(2).show(ui, |ui| {
                        ui.label("Model");
                        ui.text_edit_singleline(&mut g.model);
                        ui.end_row();
                        ui.label("Effort");
                        egui::ComboBox::from_id_salt("effort")
                            .selected_text(&g.effort)
                            .show_ui(ui, |ui| {
                                for e in ["low", "medium", "high", "xhigh", "max"] {
                                    ui.selectable_value(&mut g.effort, e.to_string(), e);
                                }
                            });
                        ui.end_row();
                        ui.label("Chunk (paragraphs)");
                        ui.add(egui::DragValue::new(&mut g.chunk_paragraphs).range(1..=200));
                        ui.end_row();
                        ui.label("Context overlap");
                        ui.add(egui::DragValue::new(&mut g.overlap_paragraphs).range(0..=50));
                        ui.end_row();
                        ui.label("Retries");
                        ui.add(egui::DragValue::new(&mut g.max_retries).range(0..=5));
                        ui.end_row();
                    });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    save = ui.add(primary_button(ui, "Save")).clicked();
                    cancel = ui.button("Cancel").clicked();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        delete = ui
                            .button(RichText::new("Delete project…").color(theme::palette(ui).rejected))
                            .clicked();
                    });
                });
            });
        if save {
            let p = self.dialogs.project_settings.take().unwrap();
            let r = self.db.update_project(&p);
            self.try_db(r);
            self.reload_projects();
            self.dirty = true;
        } else if cancel {
            self.dialogs.project_settings = None;
        } else if delete {
            self.dialogs.delete_project = self.dialogs.project_settings.take().map(|p| p.id);
        }
    }

    fn dialog_voices(&mut self, ctx: &egui::Context) {
        let Some(ed) = &mut self.dialogs.voices else {
            return;
        };
        let mut save = false;
        let mut cancel = false;
        let mut fetch = false;
        let mut rename: Option<(String, String)> = None;
        let voices = &self.voices;
        egui::Window::new("Voices and characters")
            .default_size([800.0, 480.0])
            .show(ctx, |ui| {
                let dark = ui.visuals().dark_mode;
                let muted = theme::palette(ui).muted;
                ui.label(
                    RichText::new("Every speaker needs an ElevenLabs voice id, including the narrator. Segments with an unassigned voice are skipped.")
                        .small()
                        .color(muted),
                );
                ui.horizontal(|ui| {
                    fetch = ui.button("Load my ElevenLabs voices").clicked();
                    ui.label(RichText::new(format!("{} loaded", voices.len())).small().color(muted));
                });
                egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                    egui::Grid::new("voices").num_columns(6).striped(true).spacing([10.0, 6.0]).show(ui, |ui| {
                        for h in ["Key", "Display name", "Aliases (comma separated)", "Voice id", "", ""] {
                            ui.label(RichText::new(h).small().strong().color(muted));
                        }
                        ui.end_row();
                        for (n, row) in ed.rows.iter_mut().enumerate() {
                            let key_text = RichText::new(&row.key).strong().color(speaker_color(&row.key, dark));
                            ui.label(if row.remove { key_text.strikethrough() } else { key_text });
                            if row.key == NARRATOR {
                                ui.label("Narrator");
                                ui.label("");
                            } else {
                                ui.text_edit_singleline(&mut row.display_name);
                                ui.text_edit_singleline(&mut row.aliases);
                            }
                            ui.add_sized([190.0, 22.0], egui::TextEdit::singleline(&mut row.voice_id).hint_text("voice id"));
                            if voices.is_empty() {
                                ui.label("");
                            } else {
                                let current = voices
                                    .iter()
                                    .find(|v| v.voice_id == row.voice_id)
                                    .map(|v| v.name.clone())
                                    .unwrap_or_else(|| "pick…".into());
                                egui::ComboBox::from_id_salt(("pick", n))
                                    .selected_text(current)
                                    .width(140.0)
                                    .show_ui(ui, |ui| {
                                        for v in voices {
                                            ui.selectable_value(&mut row.voice_id, v.voice_id.clone(), &v.name);
                                        }
                                    });
                            }
                            if row.key != NARRATOR {
                                ui.checkbox(&mut row.remove, "remove");
                            } else {
                                ui.label("");
                            }
                            ui.end_row();
                        }
                    });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Add character");
                    ui.add(egui::TextEdit::singleline(&mut ed.new_key).hint_text("name"));
                    if ui.button("Add").clicked() {
                        let key = crate::llm::normalize_key(&ed.new_key);
                        if !key.is_empty() && !ed.rows.iter().any(|r| r.key == key) {
                            ed.rows.push(VoiceRow {
                                key: key.clone(),
                                display_name: ed.new_key.trim().to_string(),
                                aliases: String::new(),
                                voice_id: String::new(),
                                character_id: None,
                                remove: false,
                            });
                        }
                        ed.new_key.clear();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Reassign speaker");
                    ui.add(egui::TextEdit::singleline(&mut ed.rename_from).desired_width(110.0).hint_text("from key"));
                    ui.label("→");
                    ui.add(egui::TextEdit::singleline(&mut ed.rename_to).desired_width(110.0).hint_text("to key"));
                    if ui.button("Apply to all unapproved segments").clicked() {
                        rename = Some((ed.rename_from.trim().to_string(), crate::llm::normalize_key(&ed.rename_to)));
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    save = ui.add(primary_button(ui, "Save")).clicked();
                    cancel = ui.button("Close").clicked();
                });
            });
        if fetch {
            self.worker.send(Command::FetchVoices);
        }
        if let Some((from, to)) = rename.filter(|(f, t)| !f.is_empty() && !t.is_empty())
            && let Some(pid) = self.project.as_ref().map(|p| p.id)
        {
            let r = self.db.rename_speaker(pid, &from, &to);
            if let Some(n) = self.try_db(r) {
                self.note(
                    false,
                    format!("Reassigned {n} segments from {from} to {to}."),
                );
            }
            self.dirty = true;
        }
        if save {
            let ed = self.dialogs.voices.take().unwrap();
            let Some(mut project) = self.project.clone() else {
                return;
            };
            for row in &ed.rows {
                if row.remove {
                    project.voice_map.remove(&row.key);
                    if let Some(id) = row.character_id {
                        let r = self.db.delete_character(id);
                        self.try_db(r);
                    }
                    continue;
                }
                project
                    .voice_map
                    .insert(row.key.clone(), row.voice_id.trim().to_string());
                if row.key == NARRATOR {
                    continue;
                }
                let aliases: Vec<String> = row
                    .aliases
                    .split(',')
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty())
                    .collect();
                let name = if row.display_name.trim().is_empty() {
                    row.key.clone()
                } else {
                    row.display_name.trim().to_string()
                };
                let r = match row.character_id {
                    Some(id) => self.db.update_character(&Character {
                        id,
                        project_id: project.id,
                        key: row.key.clone(),
                        display_name: name,
                        aliases,
                    }),
                    None if row.key != UNKNOWN_SPEAKER => self
                        .db
                        .upsert_character(project.id, &row.key, &name, &aliases),
                    None => Ok(()),
                };
                self.try_db(r);
            }
            let r = self.db.update_project(&project);
            self.try_db(r);
            self.dirty = true;
        } else if cancel {
            self.dialogs.voices = None;
        }
    }

    fn dialog_split(&mut self, ctx: &egui::Context) {
        let Some((id, marked, error)) = &mut self.dialogs.split else {
            return;
        };
        let original = self
            .view
            .segments
            .iter()
            .find(|s| s.id == *id)
            .map(|s| s.text.clone())
            .unwrap_or_default();
        let mut ok = false;
        let mut cancel = false;
        egui::Window::new("Split segment")
            .default_width(540.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(
                    "Insert | wherever the segment should be split. Do not change the text itself.",
                );
                ui.add(
                    egui::TextEdit::multiline(marked)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
                if let Some(e) = error {
                    ui.colored_label(theme::palette(ui).rejected, e.as_str());
                }
                ui.horizontal(|ui| {
                    ok = ui.add(primary_button(ui, "Split")).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if ok {
            match segment::split_at_markers(&original, marked) {
                Ok(parts) => {
                    let id = *id;
                    let r = self.db.split_segment(id, &parts);
                    self.try_db(r);
                    self.dialogs.split = None;
                    self.dirty = true;
                }
                Err(e) => *error = Some(e),
            }
        } else if cancel {
            self.dialogs.split = None;
        }
    }

    fn dialog_generate(&mut self, ctx: &egui::Context) {
        let Some(plan) = &self.dialogs.generate else {
            return;
        };
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(&plan.title)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} segments, {} characters. Each generation costs its full character count in credits (cached takes are free).",
                    plan.ids.len(),
                    plan.chars
                ));
                if plan.unassigned > 0 {
                    ui.colored_label(
                        theme::palette(ui).warning,
                        format!("{} segments skipped: unassigned voice.", plan.unassigned),
                    );
                }
                self.dialogue_generate_note(ui, &plan.ids, plan.reroll);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    go = ui
                        .add_enabled(!plan.ids.is_empty(), primary_button(ui, "Generate"))
                        .clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if go {
            let plan = self.dialogs.generate.take().unwrap();
            self.queue_generate(plan.ids, plan.reroll);
        } else if cancel {
            self.dialogs.generate = None;
        }
    }

    fn dialog_confirmations(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.dialogs.delete_chapter {
            let title = self
                .chapters
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.title.clone())
                .unwrap_or_default();
            if let Some(yes) = confirm(
                ctx,
                "Delete chapter",
                &format!(
                    "Delete \"{title}\" with all its segments and takes? Audio files stay on disk."
                ),
            ) {
                if yes {
                    let r = self.db.delete_chapter(id);
                    self.try_db(r);
                    self.reload_project_data();
                    match self.chapters.last().map(|c| c.id) {
                        Some(latest) => self.open_chapter(latest),
                        None => self.new_chapter(),
                    }
                }
                self.dialogs.delete_chapter = None;
            }
        }
        if let Some(id) = self.dialogs.delete_project
            && let Some(yes) = confirm(
                ctx,
                "Delete project",
                "Delete this project with all chapters, segments and takes? Audio files stay on disk.",
            )
        {
            if yes {
                let r = self.db.delete_project(id);
                self.try_db(r);
                self.project = None;
                self.chapters.clear();
                self.view = ChapterView::default();
                self.draft = TextDraft::default();
                self.selected = None;
                self.reload_projects();
                if let Some(first) = self.projects.first().map(|p| p.id) {
                    self.select_project(first);
                }
            }
            self.dialogs.delete_project = None;
        }
        if let Some(cid) = self.dialogs.resegment
            && let Some(yes) = confirm(
                ctx,
                "Re-segment",
                "Re-run attribution (and audio tags) for paragraphs whose segments are all untouched drafts? Paragraphs with any audio are kept as they are. Manual edits in draft paragraphs are replaced.",
            )
        {
            if yes {
                self.segment_chapter(cid);
            }
            self.dialogs.resegment = None;
        }
        if self.dialogs.resplit
            && let Some(yes) = confirm(
                ctx,
                "Save changed text",
                "Saving re-splits the chapter into paragraphs. Its segments, speakers and tags are discarded and must be generated again (no audio exists yet, so no credits are lost).",
            )
        {
            self.dialogs.resplit = false;
            if yes {
                self.save_text(true);
            }
        }
    }
}

/// A fixed-width, row-high cell whose content hugs the left edge.
fn left_aligned<R>(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        Vec2::new(width, ROW_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            add(ui)
        },
    )
    .inner
}

fn confirm(ctx: &egui::Context, title: &str, text: &str) -> Option<bool> {
    let mut answer = None;
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
        .show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(text);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add(primary_button(ui, "Yes")).clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
    answer
}

fn language_combo(ui: &mut egui::Ui, id: &str, code: &mut String) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(lang::by_code(code).name)
        .show_ui(ui, |ui| {
            for l in lang::LANGUAGES {
                ui.selectable_value(
                    code,
                    l.code.to_string(),
                    format!("{} — {}", l.name, l.quote_example()),
                );
            }
        });
}

fn voice_settings_ui(ui: &mut egui::Ui, vs: &mut VoiceSettings) {
    ui.add(egui::Slider::new(&mut vs.stability, 0.0..=1.0).text("stability"));
    ui.add(egui::Slider::new(&mut vs.similarity_boost, 0.0..=1.0).text("similarity"));
    ui.add(egui::Slider::new(&mut vs.style, 0.0..=1.0).text("style"));
    ui.add(egui::Slider::new(&mut vs.speed, 0.7..=1.2).text("speed"));
    ui.checkbox(&mut vs.use_speaker_boost, "speaker boost");
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_events();
        let ctx = ui.ctx().clone();
        self.keyboard(&ctx);
        let p = *theme::palette(ui);

        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0, p.border))
                    .inner_margin(egui::Margin::symmetric(14, 2)),
            )
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::symmetric(14, 4)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let err = self.log.last().is_some_and(|(e, _)| *e);
                    let color = if err { p.rejected } else { p.muted };
                    ui.label(RichText::new(&self.status).small().color(color));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Log").clicked() {
                            self.dialogs.log = !self.dialogs.log;
                        }
                        if self.tab == Tab::Review {
                            ui.label(
                                RichText::new(
                                    "Space play · P paragraph · A approve · R re-roll · ↑↓ ←→ move",
                                )
                                .small()
                                .color(p.muted),
                            );
                        }
                    });
                });
            });

        let tab = if self.tab == Tab::Review && self.view.chapter_id.is_none() {
            Tab::Text
        } else {
            self.tab
        };
        match tab {
            Tab::Text => {
                egui::CentralPanel::default().show(ui, |ui| self.text_tab(ui));
            }
            Tab::Review => {
                egui::Panel::top("review_toolbar")
                    .frame(
                        egui::Frame::new()
                            .fill(ui.visuals().panel_fill)
                            .inner_margin(egui::Margin::symmetric(16, 2)),
                    )
                    .show(ui, |ui| self.review_toolbar(ui));
                if !self.view.segments.is_empty() {
                    egui::Panel::bottom("detail")
                        .resizable(true)
                        .default_size(270.0)
                        .frame(
                            egui::Frame::new()
                                .fill(p.card)
                                .stroke(Stroke::new(1.0, p.border))
                                .inner_margin(egui::Margin::symmetric(18, 8)),
                        )
                        .show(ui, |ui| self.detail_panel(ui));
                }
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(ui.visuals().panel_fill)
                            .inner_margin(egui::Margin::symmetric(16, 6)),
                    )
                    .show(ui, |ui| self.segment_list(ui));
            }
        }
        self.dialogs(&ctx);

        if self.player.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
