//! The egui UI: project/chapter sidebar, the keyboard-first review list, the
//! segment detail panel and the settings windows.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use eframe::egui::{self, Color32, Key, RichText, Sense, UiBuilder, Vec2};

use crate::audio;
use crate::config::AppConfig;
use crate::db::{ChapterStats, Db, Library};
use crate::export;
use crate::lang;
use crate::model::*;
use crate::player::Player;
use crate::segment::{self, UNKNOWN_SPEAKER};
use crate::tts::VoiceInfo;
use crate::worker::{self, Command, Event, WorkerHandle};

const ROW_H: f32 = 44.0;

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
    paste: Option<(String, String)>,
    settings: Option<AppConfig>,
    project_settings: Option<Project>,
    voices: Option<VoiceEditor>,
    split: Option<(Id, String, Option<String>)>,
    generate: Option<GeneratePlan>,
    delete_chapter: Option<Id>,
    delete_project: Option<Id>,
    rename_chapter: Option<(Id, String)>,
    resegment: Option<Id>,
    log: bool,
}

pub struct App {
    cfg: AppConfig,
    lib: Library,
    db: Db,
    worker: WorkerHandle,
    player: Player,

    projects: Vec<Project>,
    project: Option<Project>,
    characters: Vec<Character>,
    chapters: Vec<Chapter>,
    stats: HashMap<Id, ChapterStats>,
    view: ChapterView,
    selected: Option<usize>,
    scroll_to_selected: bool,
    scroll_offset: f32,
    viewport_h: f32,
    takes: Vec<Take>,
    takes_for: Option<Id>,

    in_flight: HashSet<Id>,
    failures: HashMap<Id, String>,
    segmenting: Option<(Id, usize, usize)>,
    generating: Option<(usize, usize)>,
    exporting: bool,
    log: Vec<(bool, String)>,
    status: String,
    voices: Vec<VoiceInfo>,
    dirty: bool,
    dialogs: Dialogs,
}

fn speaker_color(key: &str) -> Color32 {
    if key == NARRATOR {
        return Color32::from_gray(150);
    }
    let h = key
        .bytes()
        .fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    egui::ecolor::Hsva::new((h % 360) as f32 / 360.0, 0.55, 0.85, 1.0).into()
}

fn status_icon(s: SegmentStatus) -> (&'static str, Color32) {
    match s {
        SegmentStatus::Draft => ("○", Color32::from_gray(140)),
        SegmentStatus::Generated => ("●", Color32::from_rgb(90, 150, 230)),
        SegmentStatus::Approved => ("✔", Color32::from_rgb(80, 190, 100)),
        SegmentStatus::Rejected => ("✖", Color32::from_rgb(220, 80, 80)),
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        let cfg = AppConfig::load();
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
            projects: vec![],
            project: None,
            characters: vec![],
            chapters: vec![],
            stats: HashMap::new(),
            view: ChapterView::default(),
            selected: None,
            scroll_to_selected: false,
            scroll_offset: 0.0,
            viewport_h: 400.0,
            takes: vec![],
            takes_for: None,
            in_flight: HashSet::new(),
            failures: HashMap::new(),
            segmenting: None,
            generating: None,
            exporting: false,
            log: vec![],
            status: String::new(),
            voices: vec![],
            dirty: false,
            dialogs: Dialogs::default(),
        };
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
        if let Some(first) = self.chapters.first().map(|c| c.id) {
            self.open_chapter(first);
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
        self.reload_view();
        if !self.view.segments.is_empty() {
            self.selected = Some(0);
        }
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
                    let title = self
                        .chapters
                        .iter()
                        .find(|c| c.id == chapter_id)
                        .map(|c| c.title.clone())
                        .unwrap_or_default();
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
        let pcm = self
            .db
            .take(take_id)
            .and_then(|t| audio::read_wav(&self.lib.audio_path(&t.audio_path)));
        if let Some(pcm) = self.try_db(pcm) {
            self.player.play(vec![(segment_id, pcm)], 0);
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
                match self
                    .db
                    .take(t)
                    .and_then(|t| audio::read_wav(&self.lib.audio_path(&t.audio_path)))
                {
                    Ok(pcm) => items.push((seg.id, pcm)),
                    Err(e) => log::warn!("{e:#}"),
                }
            }
        }
        if items.is_empty() {
            self.note(true, "No audio in this paragraph yet.".into());
        } else {
            self.player.play(items, gap);
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
            plan.chars += s.text.chars().count();
            plan.ids.push(s.id);
        }
        self.dialogs.generate = Some(plan);
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
        if ctx.text_edit_focused() || self.any_dialog_open() {
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
            || d.paste.is_some()
            || d.settings.is_some()
            || d.project_settings.is_some()
            || d.voices.is_some()
            || d.split.is_some()
            || d.generate.is_some()
            || d.delete_chapter.is_some()
            || d.delete_project.is_some()
            || d.rename_chapter.is_some()
            || d.resegment.is_some()
    }

    fn import_files(&mut self) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let Some(files) = rfd::FileDialog::new()
            .add_filter("Text", &["txt", "md"])
            .pick_files()
        else {
            return;
        };
        let mut last = None;
        for f in files {
            let title = f
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            match std::fs::read_to_string(&f) {
                Ok(text) => {
                    let r = self.db.import_chapter(project.id, &title, &text);
                    last = self.try_db(r).or(last);
                }
                Err(e) => self.note(true, format!("{}: {e}", f.display())),
            }
        }
        self.reload_project_data();
        if let Some(id) = last {
            self.open_chapter(id);
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

    // ---- panels ---------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let current = self
                .project
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "No project".into());
            let mut pick = None;
            egui::ComboBox::from_id_salt("project")
                .selected_text(current)
                .width(150.0)
                .show_ui(ui, |ui| {
                    for p in &self.projects {
                        if ui
                            .selectable_label(
                                self.project.as_ref().is_some_and(|c| c.id == p.id),
                                &p.name,
                            )
                            .clicked()
                        {
                            pick = Some(p.id);
                        }
                    }
                });
            if let Some(id) = pick {
                self.select_project(id);
            }
            if ui.button("＋").on_hover_text("New project").clicked() {
                self.dialogs.new_project = Some((String::new(), "en".into()));
            }
        });
        let has_project = self.project.is_some();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(has_project, egui::Button::new("Voices…"))
                .clicked()
            {
                self.open_voice_editor();
            }
            if ui
                .add_enabled(has_project, egui::Button::new("Project…"))
                .clicked()
            {
                self.dialogs.project_settings = self.project.clone();
            }
            if ui.button("Settings…").clicked() {
                self.dialogs.settings = Some(self.cfg.clone());
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Chapters");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(has_project, egui::Button::new("Paste…"))
                    .clicked()
                {
                    self.dialogs.paste = Some((
                        format!("Chapter {}", self.chapters.len() + 1),
                        String::new(),
                    ));
                }
                if ui
                    .add_enabled(has_project, egui::Button::new("Import…"))
                    .clicked()
                {
                    self.import_files();
                }
            });
        });
        let mut open = None;
        let mut action: Option<(Id, &'static str)> = None;
        egui::ScrollArea::vertical()
            .id_salt("chapters")
            .auto_shrink([false, true])
            .max_height(ui.available_height() - 40.0)
            .show(ui, |ui| {
                for c in &self.chapters {
                    let st = self.stats.get(&c.id).cloned().unwrap_or_default();
                    let selected = self.view.chapter_id == Some(c.id);
                    let progress = if st.segments == 0 {
                        "not segmented".to_string()
                    } else {
                        format!("{}/{} approved", st.approved, st.segments)
                    };
                    let exported = if c.status == ChapterStatus::Exported {
                        " · exported"
                    } else {
                        ""
                    };
                    let label = format!("{:02}  {}\n     {progress}{exported}", c.order, c.title);
                    let resp = ui.selectable_label(selected, label);
                    if resp.clicked() {
                        open = Some(c.id);
                    }
                    resp.context_menu(|ui| {
                        for (name, key) in [
                            ("Rename…", "rename"),
                            ("Move up", "up"),
                            ("Move down", "down"),
                            ("Delete…", "delete"),
                        ] {
                            if ui.button(name).clicked() {
                                action = Some((c.id, key));
                                ui.close();
                            }
                        }
                    });
                }
            });
        if let Some(id) = open {
            self.open_chapter(id);
        }
        if let Some((id, a)) = action {
            match a {
                "rename" => {
                    let title = self
                        .chapters
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| c.title.clone())
                        .unwrap_or_default();
                    self.dialogs.rename_chapter = Some((id, title));
                }
                "up" | "down" => {
                    let r = self.db.move_chapter(id, if a == "up" { -1 } else { 1 });
                    self.try_db(r);
                    self.reload_project_data();
                }
                _ => self.dialogs.delete_chapter = Some(id),
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            let ready = self
                .stats
                .values()
                .any(|s| s.segments > 0 && s.approved == s.segments);
            if ui
                .add_enabled(
                    has_project && ready && !self.exporting,
                    egui::Button::new("Export book (M4B)"),
                )
                .on_hover_text("All fully approved chapters as one M4B with chapter markers")
                .clicked()
                && let Some(p) = &self.project
            {
                self.exporting = true;
                self.worker.send(Command::ExportBook { project_id: p.id });
            }
            if ui.small_button("Log").clicked() {
                self.dialogs.log = !self.dialogs.log;
            }
        });
    }

    fn chapter_toolbar(&mut self, ui: &mut egui::Ui) {
        let Some(cid) = self.view.chapter_id else {
            return;
        };
        let Some(chapter) = self.chapters.iter().find(|c| c.id == cid).cloned() else {
            return;
        };
        let st = self.stats.get(&cid).cloned().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.heading(&chapter.title);
            ui.add_space(12.0);
            if st.segments > 0 {
                ui.label(format!("{}/{} approved", st.approved, st.segments));
                if st.rejected > 0 {
                    ui.colored_label(
                        Color32::from_rgb(220, 80, 80),
                        format!("{} rejected", st.rejected),
                    );
                }
                if st.unassigned > 0 {
                    ui.colored_label(
                        Color32::from_rgb(230, 160, 40),
                        format!("⚠ {} unassigned voice", st.unassigned),
                    );
                }
                ui.label(
                    RichText::new(format!(
                        "{} credits · re-roll rate {:.0}%",
                        st.credits_used,
                        st.reroll_rate() * 100.0
                    ))
                    .weak(),
                );
            }
        });
        ui.horizontal_wrapped(|ui| {
            let segmenting_this = self.segmenting.is_some_and(|(c, _, _)| c == cid);
            if segmenting_this {
                let (_, d, t) = self.segmenting.unwrap();
                ui.add(egui::Spinner::new());
                ui.label(format!("Segmenting {d}/{t} dialogue paragraphs"));
            } else {
                let label = if st.segments == 0 { "Segment with Claude" } else { "Re-segment drafts" };
                if ui
                    .button(label)
                    .on_hover_text("Attribute speakers in paragraphs that have no audio yet (free to fix before generating)")
                    .clicked()
                {
                    if st.segments == 0 {
                        self.worker.send(Command::SegmentChapter { chapter_id: cid });
                    } else {
                        self.dialogs.resegment = Some(cid);
                    }
                }
            }
            ui.separator();
            if ui.add_enabled(st.draft > 0, egui::Button::new(format!("Generate {} drafts", st.draft))).clicked() {
                self.plan_generation(false);
            }
            if ui.add_enabled(st.rejected > 0, egui::Button::new(format!("Re-roll {} rejected", st.rejected))).clicked() {
                self.plan_generation(true);
            }
            if let Some((d, t)) = self.generating {
                ui.add(egui::Spinner::new());
                ui.label(format!("{d}/{t}"));
                if ui.small_button("Cancel").clicked() {
                    self.worker.send(Command::Cancel);
                }
            }
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
    }

    fn segment_list(&mut self, ui: &mut egui::Ui) {
        let n = self.view.segments.len();
        if n == 0 {
            ui.add_space(20.0);
            if self.view.chapter_id.is_some() {
                ui.label(format!(
                    "{} paragraphs imported. Run \"Segment with Claude\" to split them into segments and attribute speakers.",
                    self.view.paragraphs.len()
                ));
            } else if self.project.is_none() {
                ui.label("Create a project to begin.");
            } else {
                ui.label("Import a chapter (plain text) to begin.");
            }
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
                let visuals = ui.visuals();
                let fill = if Some(seg.id) == playing {
                    Some(visuals.selection.bg_fill.gamma_multiply(0.6))
                } else if self.selected == Some(i) {
                    Some(visuals.selection.bg_fill.gamma_multiply(0.35))
                } else if resp.hovered() {
                    Some(visuals.widgets.hovered.bg_fill.gamma_multiply(0.4))
                } else {
                    None
                };
                if let Some(fill) = fill {
                    ui.painter().rect_filled(rect, 4.0, fill);
                }
                let first = self.view.is_first_in_paragraph(i);
                if first && i > 0 {
                    let y = rect.top() - spacing / 2.0;
                    ui.painter().hline(
                        rect.x_range(),
                        y,
                        ui.visuals().widgets.noninteractive.bg_stroke,
                    );
                }
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.add_space(4.0);
                        let para = self.view.paragraph_of(i);
                        let ptext = match (first, para) {
                            (true, Some(p)) if p.scene_break_before => format!("✱{}", p.order + 1),
                            (true, Some(p)) => format!("¶{}", p.order + 1),
                            _ => String::new(),
                        };
                        ui.add_sized(
                            [40.0, ROW_H],
                            egui::Label::new(RichText::new(ptext).weak().small()).selectable(false),
                        );
                        let (icon, color) = if self.in_flight.contains(&seg.id) {
                            ("◌", Color32::from_rgb(230, 190, 60))
                        } else {
                            status_icon(seg.status)
                        };
                        let status_label = ui.add_sized(
                            [18.0, ROW_H],
                            egui::Label::new(RichText::new(icon).color(color)).selectable(false),
                        );
                        if let Some(err) = self.failures.get(&seg.id) {
                            status_label.on_hover_text(format!("Last generation failed: {err}"));
                        }
                        let unassigned = project
                            .as_ref()
                            .is_none_or(|p| p.voice_for(&seg.speaker).is_none());
                        let badge = if unassigned {
                            RichText::new(format!("⚠ {}", seg.speaker))
                                .color(Color32::from_rgb(230, 160, 40))
                        } else {
                            RichText::new(&seg.speaker).color(speaker_color(&seg.speaker))
                        };
                        let badge_resp = left_aligned(ui, 110.0, |ui| {
                            ui.add(
                                egui::Label::new(badge.strong())
                                    .truncate()
                                    .selectable(false),
                            )
                        });
                        if unassigned {
                            badge_resp.on_hover_text(
                                "Unassigned voice: map this speaker in Voices… before generating",
                            );
                        }
                        let buttons_w = 150.0;
                        let text_w = (ui.available_width() - buttons_w).max(80.0);
                        let mut job = egui::text::LayoutJob::single_section(
                            seg.text.clone(),
                            egui::TextFormat {
                                font_id: egui::FontId::proportional(14.0),
                                color: if seg.kind == SegmentKind::Dialogue {
                                    ui.visuals().strong_text_color()
                                } else {
                                    ui.visuals().text_color()
                                },
                                italics: seg.kind == SegmentKind::Narration
                                    && seg.speaker == NARRATOR
                                    && seg.text.chars().count() < 40,
                                ..Default::default()
                            },
                        );
                        job.wrap.max_width = text_w;
                        job.wrap.max_rows = 2;
                        job.wrap.overflow_character = Some('…');
                        left_aligned(ui, text_w, |ui| {
                            ui.add(egui::Label::new(job).selectable(false))
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                                ui.label(RichText::new(format!("{takes}×")).weak().small());
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
        let Some(i) = self.selected else {
            ui.label(RichText::new("Select a segment. Keys: Space play · P play paragraph · A approve · R reject/re-roll · ↑↓ segments · ←→ paragraphs").weak());
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

        egui::ScrollArea::vertical()
            .id_salt("detail")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (icon, color) = status_icon(seg.status);
                    ui.colored_label(color, format!("{icon} {}", seg.status.as_str()));
                    ui.label(
                        RichText::new(format!(
                            "{} · {} chars",
                            seg.kind.as_str(),
                            seg.text.chars().count()
                        ))
                        .weak(),
                    );
                    if let Some(e) = self.failures.get(&seg.id) {
                        ui.colored_label(
                            Color32::from_rgb(220, 80, 80),
                            format!("Last attempt failed: {e}"),
                        );
                    }
                });
                ui.add(egui::Label::new(RichText::new(&seg.text).size(16.0)).wrap());
                ui.add_space(6.0);

                ui.horizontal_wrapped(|ui| {
                    ui.label("Speaker:");
                    let mut speaker = seg.speaker.clone();
                    ui.add_enabled_ui(!locked, |ui| {
                        egui::ComboBox::from_id_salt(("speaker", seg.id))
                            .selected_text(&speaker)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut speaker, NARRATOR.to_string(), NARRATOR);
                                for c in &self.characters {
                                    ui.selectable_value(
                                        &mut speaker,
                                        c.key.clone(),
                                        format!("{} ({})", c.key, c.display_name),
                                    );
                                }
                                ui.selectable_value(
                                    &mut speaker,
                                    UNKNOWN_SPEAKER.to_string(),
                                    UNKNOWN_SPEAKER,
                                );
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
                            .add_enabled(
                                seg.current_take_id.is_some(),
                                egui::Button::new("✔ Approve"),
                            )
                            .clicked()
                        {
                            self.approve(i);
                        }
                        if ui
                            .button(if seg.current_take_id.is_some() {
                                "↻ Re-roll"
                            } else {
                                "Generate"
                            })
                            .clicked()
                        {
                            self.reject(i);
                        }
                    }
                    if ui.button("▶ Paragraph").clicked() {
                        self.play_paragraph(i);
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            !locked && !last_in_para,
                            egui::Button::new("Merge with next"),
                        )
                        .clicked()
                    {
                        let r = self.db.merge_with_next(seg.id);
                        self.try_db(r);
                        self.dirty = true;
                    }
                    if ui
                        .add_enabled(
                            !locked && i > range.start,
                            egui::Button::new("Merge with previous"),
                        )
                        .clicked()
                    {
                        let prev = self.view.segments[i - 1].id;
                        let r = self.db.merge_with_next(prev);
                        if self.try_db(r).is_some() {
                            self.selected = Some(i - 1);
                        }
                        self.dirty = true;
                    }
                    if ui
                        .add_enabled(!locked, egui::Button::new("Split…"))
                        .clicked()
                    {
                        self.dialogs.split = Some((seg.id, seg.text.clone(), None));
                    }
                });

                ui.add_space(4.0);
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
                        let r = self
                            .db
                            .set_segment_override(seg.id, use_override.then_some(&vs));
                        self.try_db(r);
                        self.dirty = true;
                    }
                });

                ui.add_space(4.0);
                ui.strong(format!("Takes ({})", self.takes.len()));
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
                        } else {
                            format!("{} credits", t.credits_used)
                        };
                        let text =
                            format!("#{} · seed {} · {cost} · {}", n + 1, t.seed, t.created_at);
                        ui.label(if current {
                            RichText::new(text).strong()
                        } else {
                            RichText::new(text)
                        });
                        if current {
                            ui.label(
                                RichText::new("current").color(Color32::from_rgb(80, 190, 100)),
                            );
                        } else if ui
                            .add_enabled(!locked, egui::Button::new("Use").small())
                            .clicked()
                        {
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
        self.dialog_paste(ctx);
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
                .default_size([600.0, 300.0])
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for (err, m) in &self.log {
                                if *err {
                                    ui.colored_label(Color32::from_rgb(220, 100, 100), m);
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
        egui::Window::new("New project").collapsible(false).resizable(false).show(ctx, |ui| {
            egui::Grid::new("np").num_columns(2).show(ui, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(name);
                ui.end_row();
                ui.label("Language");
                language_combo(ui, "np_lang", language);
                ui.end_row();
            });
            ui.label(RichText::new("One language per project; it decides quote styles and the default TTS model.").weak());
            ui.horizontal(|ui| {
                create = ui.add_enabled(!name.trim().is_empty(), egui::Button::new("Create")).clicked();
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

    fn dialog_paste(&mut self, ctx: &egui::Context) {
        let Some((title, text)) = &mut self.dialogs.paste else {
            return;
        };
        let mut ok = false;
        let mut cancel = false;
        egui::Window::new("Paste chapter")
            .default_size([640.0, 480.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Title");
                    ui.text_edit_singleline(title);
                });
                egui::ScrollArea::vertical()
                    .max_height(400.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(text)
                                .desired_rows(18)
                                .desired_width(f32::INFINITY),
                        );
                    });
                let n = segment::split_paragraphs(text).len();
                ui.horizontal(|ui| {
                    ok = ui
                        .add_enabled(n > 0, egui::Button::new(format!("Import {n} paragraphs")))
                        .clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if ok {
            let (title, text) = self.dialogs.paste.take().unwrap();
            if let Some(p) = self.project.clone() {
                let r = self.db.import_chapter(p.id, title.trim(), &text);
                if let Some(id) = self.try_db(r) {
                    self.reload_project_data();
                    self.open_chapter(id);
                }
            }
        } else if cancel {
            self.dialogs.paste = None;
        }
    }

    fn dialog_settings(&mut self, ctx: &egui::Context) {
        let Some(cfg) = &mut self.dialogs.settings else {
            return;
        };
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Settings").collapsible(false).show(ctx, |ui| {
            egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                ui.label("Anthropic API key");
                ui.add(egui::TextEdit::singleline(&mut cfg.anthropic_api_key).password(true).hint_text("or ANTHROPIC_API_KEY"));
                ui.end_row();
                ui.label("ElevenLabs API key");
                ui.add(egui::TextEdit::singleline(&mut cfg.elevenlabs_api_key).password(true).hint_text("or ELEVENLABS_API_KEY"));
                ui.end_row();
                ui.label("Library");
                ui.label(cfg.library_dir().display().to_string());
                ui.end_row();
            });
            if ui.button("Choose library folder…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    cfg.library_dir = Some(dir);
                }
            ui.label(RichText::new("Keys are stored in the config file with owner-only permissions. A library change takes effect after restart.").weak());
            ui.horizontal(|ui| {
                save = ui.button("Save").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if save {
            let cfg = self.dialogs.settings.take().unwrap();
            self.worker.send(Command::SetKeys(cfg.api_keys()));
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
            .default_width(460.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(560.0)
                    .show(ui, |ui| {
                        egui::Grid::new("ps")
                            .num_columns(2)
                            .spacing([12.0, 6.0])
                            .show(ui, |ui| {
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
                                            ui.selectable_value(
                                                &mut p.tts_model,
                                                id.to_string(),
                                                format!("{id} — {desc}"),
                                            );
                                        }
                                    });
                                ui.end_row();
                                ui.label("Output format");
                                egui::ComboBox::from_id_salt("fmt")
                                    .selected_text(&p.settings.output_format)
                                    .show_ui(ui, |ui| {
                                        for (id, desc) in lang::OUTPUT_FORMATS {
                                            ui.selectable_value(
                                                &mut p.settings.output_format,
                                                id.to_string(),
                                                format!("{id} — {desc}"),
                                            );
                                        }
                                    });
                                ui.end_row();
                                ui.label("Concurrent requests");
                                ui.add(
                                    egui::DragValue::new(&mut p.settings.concurrency).range(1..=15),
                                )
                                .on_hover_text("Your ElevenLabs plan's concurrency limit");
                                ui.end_row();
                                ui.label("Request stitching");
                                ui.checkbox(
                                    &mut p.settings.request_stitching,
                                    "send previous request ids",
                                );
                                ui.end_row();
                                ui.label("Reject key");
                                ui.checkbox(
                                    &mut p.settings.reroll_on_reject,
                                    "R re-rolls immediately",
                                );
                                ui.end_row();
                            });
                        ui.separator();
                        ui.strong("Default voice settings");
                        ui.horizontal_wrapped(|ui| {
                            voice_settings_ui(ui, &mut p.default_voice_settings)
                        });
                        ui.separator();
                        ui.strong("Silence (ms)");
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
                        ui.separator();
                        ui.strong("Segmentation (Claude)");
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
                    save = ui.button("Save").clicked();
                    cancel = ui.button("Cancel").clicked();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        delete = ui
                            .button(
                                RichText::new("Delete project…")
                                    .color(Color32::from_rgb(220, 80, 80)),
                            )
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
        egui::Window::new("Voices and characters").default_size([760.0, 480.0]).show(ctx, |ui| {
            ui.label(RichText::new("Every speaker needs an ElevenLabs voice id, including the narrator. Segments with an unassigned voice are skipped.").weak());
            ui.horizontal(|ui| {
                fetch = ui.button("Load my ElevenLabs voices").clicked();
                ui.label(RichText::new(format!("{} loaded", voices.len())).weak());
            });
            egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                egui::Grid::new("voices").num_columns(6).striped(true).show(ui, |ui| {
                    ui.strong("Key");
                    ui.strong("Display name");
                    ui.strong("Aliases (comma separated)");
                    ui.strong("Voice id");
                    ui.strong("");
                    ui.strong("");
                    ui.end_row();
                    for (n, row) in ed.rows.iter_mut().enumerate() {
                        let key_text = RichText::new(&row.key).color(speaker_color(&row.key));
                        ui.label(if row.remove { key_text.strikethrough() } else { key_text });
                        if row.key == NARRATOR {
                            ui.label("Narrator");
                            ui.label("");
                        } else {
                            ui.text_edit_singleline(&mut row.display_name);
                            ui.text_edit_singleline(&mut row.aliases);
                        }
                        ui.add_sized(
                            [190.0, 20.0],
                            egui::TextEdit::singleline(&mut row.voice_id).hint_text("voice id"),
                        );
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
            ui.horizontal(|ui| {
                ui.label("Add character key:");
                ui.text_edit_singleline(&mut ed.new_key);
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
            ui.horizontal(|ui| {
                save = ui.button("Save").clicked();
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
            .default_width(520.0)
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
                    ui.colored_label(Color32::from_rgb(220, 80, 80), e.as_str());
                }
                ui.horizontal(|ui| {
                    ok = ui.button("Split").clicked();
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
        egui::Window::new(&plan.title).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label(format!(
                "{} segments, {} characters. Each generation costs its full character count in credits (cached takes are free).",
                plan.ids.len(),
                plan.chars
            ));
            if plan.unassigned > 0 {
                ui.colored_label(
                    Color32::from_rgb(230, 160, 40),
                    format!("{} segments skipped: unassigned voice.", plan.unassigned),
                );
            }
            ui.horizontal(|ui| {
                go = ui.add_enabled(!plan.ids.is_empty(), egui::Button::new("Generate")).clicked();
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
        if let Some((id, title)) = &mut self.dialogs.rename_chapter {
            let mut ok = false;
            let mut cancel = false;
            egui::Window::new("Rename chapter")
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.text_edit_singleline(title);
                    ui.horizontal(|ui| {
                        ok = ui.button("Rename").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if ok {
                let r = self.db.rename_chapter(*id, title.trim());
                self.try_db(r);
                self.dialogs.rename_chapter = None;
                self.dirty = true;
            } else if cancel {
                self.dialogs.rename_chapter = None;
            }
        }
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
                    if self.view.chapter_id == Some(id) {
                        self.view = ChapterView::default();
                        self.selected = None;
                    }
                    self.dirty = true;
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
                "Re-run attribution for paragraphs whose segments are all untouched drafts? Paragraphs with any audio are kept as they are. Manual edits in draft paragraphs are replaced.",
            )
        {
            if yes {
                self.worker
                    .send(Command::SegmentChapter { chapter_id: cid });
            }
            self.dialogs.resegment = None;
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
        .show(ctx, |ui| {
            ui.label(text);
            ui.horizontal(|ui| {
                if ui.button("Yes").clicked() {
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

        egui::Panel::left("sidebar")
            .default_size(250.0)
            .show(ui, |ui| self.sidebar(ui));
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let err = self.log.last().is_some_and(|(e, _)| *e);
                if err {
                    ui.colored_label(Color32::from_rgb(220, 100, 100), &self.status);
                } else {
                    ui.label(RichText::new(&self.status).weak());
                }
            });
        });
        if self.view.chapter_id.is_some() {
            egui::Panel::top("toolbar").show(ui, |ui| {
                ui.add_space(4.0);
                self.chapter_toolbar(ui);
                ui.add_space(4.0);
            });
            if !self.view.segments.is_empty() {
                egui::Panel::bottom("detail")
                    .resizable(true)
                    .default_size(240.0)
                    .show(ui, |ui| self.detail_panel(ui));
            }
        }
        egui::CentralPanel::default().show(ui, |ui| self.segment_list(ui));
        self.dialogs(&ctx);

        if self.player.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
