//! Multi-speaker dialogue in the UI: the project setting, the Generate
//! confirmation's note and the take list's marker. The generation itself is
//! in `worker/dialogue.rs`.

use std::collections::HashSet;

use eframe::egui::{self, RichText};

use super::{App, chip};
use crate::dialogue::{self, DialogueSettings, MAX_CHARS_LIMIT};
use crate::lang;
use crate::model::*;
use crate::theme;

/// Project settings: "MULTI-SPEAKER DIALOGUE".
pub(super) fn settings_ui(
    ui: &mut egui::Ui,
    d: &mut DialogueSettings,
    model: &str,
    voice: &VoiceSettings,
) {
    let p = *theme::palette(ui);
    ui.checkbox(
        &mut d.enabled,
        "Generate multi-speaker paragraphs as one dialogue (Eleven v3/v4)",
    );
    ui.label(
        RichText::new(
            "A paragraph that goes back and forth between speakers is sent as one Text-to-Dialogue request, for more natural turn-taking, and cut back into one take per segment in the pauses between the speakers, so each segment is still reviewed on its own. Only the first generation of a whole draft paragraph with 2–10 voices qualifies; re-rolls and single segments are generated per segment. If a result cannot be cut cleanly, the paragraph is generated again per segment and its characters are billed twice.",
        )
        .small()
        .color(p.muted),
    );
    ui.horizontal(|ui| {
        ui.label("Largest dialogue");
        ui.add(
            egui::DragValue::new(&mut d.max_chars)
                .range(100..=MAX_CHARS_LIMIT)
                .speed(10)
                .suffix(" characters"),
        )
        .on_hover_text(
            "Spoken characters of the paragraph, audio tags included. ElevenLabs advises at most 2,000 per request.",
        );
    });
    if !lang::model_supports_dialogue(model) {
        ui.label(
            RichText::new(format!(
                "{model} has no dialogue endpoint: every paragraph is generated per segment."
            ))
            .small()
            .color(p.warning),
        );
    } else if (voice.speed - 1.0).abs() > 1e-3 || voice.style.abs() > 1e-3 {
        ui.label(
            RichText::new(
                "Dialogue takes only stability and similarity, so with a speed or style other than the default every paragraph is generated per segment.",
            )
            .small()
            .color(p.warning),
        );
    } else {
        let stability = if model.starts_with("eleven_v3") {
            "stability (rounded to 0, 0.5 or 1 for Eleven v3)"
        } else {
            "stability and similarity"
        };
        ui.label(
            RichText::new(format!(
                "Dialogue uses the default voice settings' {stability}. Paragraphs with a segment's own voice settings, or an [interrupting] tag, are generated per segment."
            ))
            .small()
            .color(p.muted),
        );
    }
}

/// Marks a take that was cut from a dialogue request.
pub(super) fn take_chip(ui: &mut egui::Ui, take: &Take) {
    if take.dialogue_group.is_none() {
        return;
    }
    let muted = theme::palette(ui).muted;
    chip(ui, "dialogue", muted).on_hover_text(
        "Cut from one multi-speaker dialogue request for the whole paragraph. Its credits are its share of that request, by characters. Re-rolling generates this segment on its own.",
    );
}

impl App {
    /// The Generate confirmation's note on the paragraphs that go through
    /// dialogue (the worker plans the same way).
    pub(super) fn dialogue_generate_note(&self, ui: &mut egui::Ui, ids: &[Id], reroll: bool) {
        let Some(project) = &self.project else {
            return;
        };
        let requested: HashSet<Id> = ids.iter().copied().collect();
        let plan = dialogue::plan(project, &self.view.segments, &requested, reroll);
        if plan.is_empty() {
            return;
        }
        let segments: usize = plan.iter().map(Vec::len).sum();
        let what = if plan.len() == 1 {
            format!("1 paragraph ({segments} segments) goes")
        } else {
            format!("{} paragraphs ({segments} segments) go", plan.len())
        };
        let p = *theme::palette(ui);
        ui.label(
            RichText::new(format!(
                "{what} through multi-speaker dialogue: one request per paragraph, cut back into one take per segment."
            ))
            .color(p.accent),
        );
        ui.label(
            RichText::new(
                "A paragraph whose dialogue cannot be cut cleanly is generated again per segment, which bills its characters a second time.",
            )
            .small()
            .color(p.muted),
        );
    }
}
