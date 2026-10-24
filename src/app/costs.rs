//! Costs in the UI: the ElevenLabs balance in the top bar, credit and Claude
//! estimates before anything is spent, Claude usage in the Review toolbar,
//! and the Claude backend section of Settings.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::{App, chip};
use crate::claude_code::{Detection, NoteLevel};
use crate::config::{AppConfig, ClaudeBackend};
use crate::costs::{self, CreditRate, ParagraphInfo, PromptOverhead, fmt_int, fmt_tokens, fmt_usd};
use crate::db::PassUsage;
use crate::lang;
use crate::llm;
use crate::model::*;
use crate::segment::{self, ParsedParagraph};
use crate::theme::{self, Palette};
use crate::tts::account::{AccountInfo, Subscription};
use crate::worker::Command;

/// Bursts of /v1/user/subscription calls draw 429s: refresh at most this often
/// on its own (a click on refresh always goes through).
const ACCOUNT_MIN_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Default)]
pub(super) struct CostState {
    /// The balance, or why it is unavailable. None before the first answer.
    account: Option<Result<Subscription, String>>,
    account_in_flight: bool,
    last_fetch: Option<Instant>,
    /// A refresh was asked for while throttled; it runs when allowed.
    refresh_due: bool,
    detection: Option<Detection>,
    detecting: bool,
    /// Claude usage of the open chapter, per pass and backend.
    usage: Vec<PassUsage>,
    /// Credit rate of the open project's model. The estimates are drawn
    /// every frame; the rate only changes with new takes or list rates, so
    /// it is read from the database on reload, not per frame.
    rate: Option<(String, CreditRate)>,
}

impl App {
    pub(super) fn costs_startup(&mut self) {
        if !self.cfg.api_keys().elevenlabs.is_empty() {
            self.request_account(true);
        }
        if self.cfg.claude_backend == ClaudeBackend::ClaudeCode {
            self.request_detection(self.cfg.claude_code_path.clone());
        }
    }

    /// Ask the worker for the balance. Without `force`, refreshes closer
    /// together than `ACCOUNT_MIN_INTERVAL` are deferred, not dropped.
    pub(super) fn request_account(&mut self, force: bool) {
        if self.cfg.api_keys().elevenlabs.is_empty() {
            self.costs.account = None;
            return;
        }
        let recent = self
            .costs
            .last_fetch
            .is_some_and(|t| t.elapsed() < ACCOUNT_MIN_INTERVAL);
        if self.costs.account_in_flight || (recent && !force) {
            self.costs.refresh_due = true;
            return;
        }
        self.costs.account_in_flight = true;
        self.costs.refresh_due = false;
        self.costs.last_fetch = Some(Instant::now());
        self.worker.send(Command::FetchAccount);
    }

    /// Runs a deferred balance refresh once it is allowed.
    pub(super) fn costs_tick(&mut self, ctx: &egui::Context) {
        if !self.costs.refresh_due || self.costs.account_in_flight {
            return;
        }
        let waited = self
            .costs
            .last_fetch
            .map_or(ACCOUNT_MIN_INTERVAL, |t| t.elapsed());
        if waited >= ACCOUNT_MIN_INTERVAL {
            self.request_account(true);
        } else {
            ctx.request_repaint_after(ACCOUNT_MIN_INTERVAL - waited);
        }
    }

    pub(super) fn on_account(&mut self, info: AccountInfo) {
        self.costs.account_in_flight = false;
        // Say it once, not after every generation batch.
        if let Err(e) = &info.subscription
            && !matches!(&self.costs.account, Some(Err(old)) if old == e)
        {
            self.note(false, e.clone());
        }
        self.costs.account = Some(info.subscription);
        if info.model_rates > 0 {
            log::info!("ElevenLabs list rates for {} models", info.model_rates);
            self.refresh_credit_rate();
        }
    }

    pub(super) fn request_detection(&mut self, path: Option<PathBuf>) {
        if self.costs.detecting {
            return;
        }
        self.costs.detecting = true;
        self.worker.send(Command::DetectClaudeCode { path });
    }

    pub(super) fn on_claude_detection(&mut self, d: Detection) {
        self.costs.detecting = false;
        self.costs.detection = Some(d);
    }

    /// After Settings are saved: refresh what the change affects.
    pub(super) fn settings_saved(&mut self, new: &AppConfig) {
        let key_changed = new.api_keys().elevenlabs != self.cfg.api_keys().elevenlabs;
        let claude_changed = new.claude_backend != self.cfg.claude_backend
            || new.claude_code_path != self.cfg.claude_code_path;
        if key_changed {
            self.costs.account = None;
            if !new.api_keys().elevenlabs.is_empty() {
                self.costs.account_in_flight = true;
                self.costs.last_fetch = Some(Instant::now());
                self.worker.send(Command::FetchAccount);
            }
        }
        if claude_changed && new.claude_backend == ClaudeBackend::ClaudeCode {
            self.costs.detection = None;
            self.request_detection(new.claude_code_path.clone());
        }
    }

    pub(super) fn reload_cost_stats(&mut self) {
        self.costs.usage = match self.view.chapter_id {
            Some(cid) => self.db.chapter_llm_usage(cid).unwrap_or_else(|e| {
                log::warn!("Claude usage: {e:#}");
                vec![]
            }),
            None => vec![],
        };
        self.refresh_credit_rate();
    }

    fn refresh_credit_rate(&mut self) {
        self.costs.rate = self
            .project
            .as_ref()
            .map(|p| (p.tts_model.clone(), self.db.credit_rate(&p.tts_model)));
    }

    /// The cached rate, or a fresh read when the model changed since.
    pub(super) fn credit_rate(&self, model: &str) -> CreditRate {
        match &self.costs.rate {
            Some((m, r)) if m == model => *r,
            _ => self.db.credit_rate(model),
        }
    }

    /// The plan tier of the last balance answer ("free", "creator", …).
    pub(super) fn account_tier(&self) -> Option<&str> {
        match &self.costs.account {
            Some(Ok(sub)) => sub.tier.as_deref(),
            _ => None,
        }
    }

    fn remaining_credits(&self) -> Option<i64> {
        match &self.costs.account {
            Some(Ok(sub)) => sub.remaining(),
            _ => None,
        }
    }

    /// Money for `credits` when the user set a price per 1,000 credits.
    fn money(&self, credits: i64) -> Option<String> {
        let price = self.cfg.price_per_1000_credits.filter(|p| *p > 0.0)?;
        Some(costs::fmt_money(
            costs::credits_money(credits, price),
            &self.cfg.credit_currency,
        ))
    }

    /// "≈ 12,400 credits (≈ 2.98 USD)".
    pub(super) fn credits_text(&self, credits: i64) -> String {
        match self.money(credits) {
            Some(m) => format!("≈ {} credits (≈ {m})", fmt_int(credits)),
            None => format!("≈ {} credits", fmt_int(credits)),
        }
    }

    // ---- top bar --------------------------------------------------------------

    /// "412,300 credits left · resets 3 Nov" and a refresh button. Added
    /// inside the top bar's right-to-left layout.
    pub(super) fn account_chip(&mut self, ui: &mut egui::Ui) {
        if self.cfg.api_keys().elevenlabs.is_empty() {
            return;
        }
        let p = *theme::palette(ui);
        ui.add_space(6.0);
        if ui
            .add_enabled(
                !self.costs.account_in_flight,
                egui::Button::new("⟳").small(),
            )
            .on_hover_text("Refresh the ElevenLabs balance")
            .clicked()
        {
            self.request_account(true);
        }
        match &self.costs.account {
            None => {
                if self.costs.account_in_flight {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            }
            Some(Ok(sub)) => {
                let Some(left) = sub.remaining() else {
                    ui.label(RichText::new("balance unknown").small().color(p.muted))
                        .on_hover_text(subscription_details(sub));
                    return;
                };
                let mut text = format!("{} credits left", fmt_int(left));
                if let Some(reset) = sub.next_character_count_reset_unix {
                    text.push_str(&format!(" · resets {}", costs::fmt_day_month(reset)));
                }
                // The top bar's free room decides how much fits; the hover
                // always has the details.
                let room = ui.available_width();
                let fits = |t: &str, style: egui::TextStyle| {
                    let font = style.resolve(ui.style());
                    let w = ui
                        .fonts_mut(|f| f.layout_no_wrap(t.to_string(), font, egui::Color32::WHITE))
                        .size()
                        .x;
                    w + 12.0 <= room
                };
                let color = if left == 0 {
                    p.warning
                } else {
                    ui.visuals().text_color()
                };
                let short = format!("{} credits", fmt_tokens(left.max(0) as u64));
                let label = if fits(&text, egui::TextStyle::Body) {
                    Some(RichText::new(text))
                } else if fits(&short, egui::TextStyle::Small) {
                    Some(RichText::new(short).small())
                } else {
                    None
                };
                if let Some(label) = label {
                    ui.label(label.color(color))
                        .on_hover_text(subscription_details(sub));
                }
            }
            Some(Err(e)) => {
                ui.label(RichText::new("balance unavailable").small().color(p.muted))
                    .on_hover_text(e.clone());
            }
        }
    }

    // ---- estimates before spending ------------------------------------------------

    /// The Generate dialog's estimate: credits, where the rate comes from, and
    /// the share of the remaining balance. Warns when it exceeds the balance
    /// but never blocks.
    pub(super) fn generate_estimate_ui(&self, ui: &mut egui::Ui, chars: usize) {
        let Some(project) = &self.project else {
            return;
        };
        let p = *theme::palette(ui);
        let rate = self.credit_rate(&project.tts_model);
        let credits = rate.credits(chars);
        let mut line = self.credits_text(credits);
        let remaining = self.remaining_credits();
        if let Some(left) = remaining.filter(|l| *l > 0) {
            line.push_str(&format!(
                " — {} of your remaining {}",
                share(credits, left),
                fmt_int(left)
            ));
        }
        ui.label(RichText::new(line).strong());
        if let Some(left) = remaining
            && credits > left
        {
            let overage = match &self.costs.account {
                Some(Ok(sub)) if sub.overage_allowed() => {
                    " Usage-based billing is on for your plan, so the rest is billed as overage."
                }
                _ => " ElevenLabs may stop partway; you can still go ahead.",
            };
            ui.colored_label(
                p.warning,
                format!(
                    "⚠ This is more than your remaining {} credits.{overage}",
                    fmt_int(left)
                ),
            );
        }
        ui.label(
            RichText::new(format!(
                "{}. Cached takes are free, so the real cost can be lower.",
                capitalized(&rate.basis_text())
            ))
            .small()
            .color(p.muted),
        );
    }

    /// Draft segments that Generate would send: (count, spoken characters, rate).
    fn drafts_estimate(&self) -> Option<(usize, usize, CreditRate)> {
        let project = self.project.as_ref()?;
        let (mut n, mut chars) = (0usize, 0usize);
        for s in &self.view.segments {
            if s.status == SegmentStatus::Draft
                && !self.in_flight.contains(&s.id)
                && project.voice_for(&s.speaker).is_some()
            {
                n += 1;
                chars += s.spoken_text(&project.tts_model).chars().count();
            }
        }
        (n > 0).then(|| (n, chars, self.credit_rate(&project.tts_model)))
    }

    /// Review toolbar chips: "Drafts ≈ N credits" and the chapter's Claude usage.
    pub(super) fn cost_chips(&self, ui: &mut egui::Ui) {
        let p = *theme::palette(ui);
        if let Some((n, chars, rate)) = self.drafts_estimate() {
            let credits = rate.credits(chars);
            let over = self.remaining_credits().is_some_and(|l| credits > l);
            let mut tip = format!(
                "{n} draft segments with a voice, {} characters × {:.3} credits per character ({}). Cached takes are free.",
                fmt_int(chars as i64),
                rate.per_char,
                rate.basis_text()
            );
            if let Some(left) = self.remaining_credits() {
                tip.push_str(&format!(
                    "\n{} of your remaining {} credits.",
                    share(credits, left),
                    fmt_int(left)
                ));
            }
            chip(
                ui,
                format!("Drafts {}", self.credits_text(credits)),
                if over { p.warning } else { p.muted },
            )
            .on_hover_text(tip);
        }
        if let Some((text, tip)) = usage_chip(&self.costs.usage) {
            chip(ui, text, p.muted).on_hover_text(tip);
        }
    }

    /// Text tab footer: a rough Claude estimate for "Segment & add audio tags",
    /// and a warning when the chosen backend cannot run.
    pub(super) fn claude_estimate_footer(
        &self,
        ui: &mut egui::Ui,
        paragraphs: &[ParsedParagraph],
        lang: &lang::Language,
    ) {
        let Some(project) = &self.project else {
            return;
        };
        if paragraphs.is_empty() {
            return;
        }
        let p = *theme::palette(ui);
        let info: Vec<ParagraphInfo> = paragraphs
            .iter()
            .map(|pp| ParagraphInfo {
                chars: pp.text.chars().count(),
                dialogue: segment::has_dialogue(&pp.text, lang),
            })
            .collect();
        let tags = (project.settings.audio_tags.enabled
            && lang::model_supports_tags(&project.tts_model))
        .then_some(project.settings.audio_tags.density.as_str());
        let claude_code = self.cfg.claude_backend == ClaudeBackend::ClaudeCode;
        let (attribution_chars, tags_chars) = llm::prompt_overhead_chars();
        let cpt = costs::chars_per_token(&project.language);
        let est = costs::estimate_claude(
            &info,
            &project.settings.segmentation,
            tags,
            PromptOverhead {
                attribution_chars,
                tags_chars,
                claude_code,
            },
            cpt,
        );
        let model = &project.settings.segmentation.model;
        let text = if est.requests() == 0 {
            "Claude: no requests needed (no dialogue to attribute).".to_string()
        } else {
            let cost = match (est.cost_usd(model), claude_code) {
                (Some(c), true) => {
                    format!(
                        " · ≈ {} API-equivalent, covered by your subscription",
                        fmt_usd(c)
                    )
                }
                (Some(c), false) => format!(" · ≈ {}", fmt_usd(c)),
                (None, _) => format!(" · no list price known for {model}"),
            };
            format!(
                "Claude, rough estimate: ≈ {} input + {} output tokens in {} request{}{cost}",
                fmt_tokens(est.input_tokens()),
                fmt_tokens(est.output_tokens()),
                est.requests(),
                if est.requests() == 1 { "" } else { "s" },
            )
        };
        let mut tip = format!(
            "Speaker attribution: {} requests, ≈ {} input + {} output tokens.",
            est.attribution.requests,
            fmt_tokens(est.attribution.input_tokens),
            fmt_tokens(est.attribution.output_tokens)
        );
        match est.tags {
            Some(t) => tip.push_str(&format!(
                "\nAudio tags ({}): {} requests, ≈ {} input + {} output tokens.",
                project.settings.audio_tags.density,
                t.requests,
                fmt_tokens(t.input_tokens),
                fmt_tokens(t.output_tokens)
            )),
            None => tip.push_str("\nAudio tags: not added automatically for this project."),
        }
        tip.push_str(&format!(
            "\n\nRough: assumes about {cpt:.0} characters per token and no retries{}. The real usage appears in Review after the run.",
            if claude_code {
                "; Claude Code's own framing is approximated"
            } else {
                ""
            }
        ));
        if claude_code {
            tip.push_str("\nWith Claude Code the calls count against your plan's usage limits; the dollar figure is what they would cost on the API.");
        }
        ui.add(egui::Label::new(RichText::new(text).small().color(p.muted)).wrap())
            .on_hover_text(tip);
        if let Some(problem) = self.claude_problem() {
            ui.add(
                egui::Label::new(
                    RichText::new(format!("⚠ {problem}"))
                        .small()
                        .color(p.warning),
                )
                .wrap(),
            );
        }
    }

    /// Why the chosen Claude backend would not run, when known.
    fn claude_problem(&self) -> Option<String> {
        match self.cfg.claude_backend {
            ClaudeBackend::ApiKey if self.cfg.api_keys().anthropic.is_empty() => Some(
                "No Anthropic API key: dialogue would be split by the parser only. Add a key or choose Claude Code in Settings."
                    .into(),
            ),
            ClaudeBackend::ApiKey => None,
            ClaudeBackend::ClaudeCode => {
                let d = self.costs.detection.as_ref()?;
                if d.ready() {
                    return None;
                }
                let worst = d
                    .notes
                    .iter()
                    .find(|n| n.level == NoteLevel::Error)
                    .or_else(|| d.notes.iter().find(|n| n.level == NoteLevel::Warn))?;
                Some(format!("Claude Code: {} (Settings)", worst.text))
            }
        }
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// "3%", "<1%".
fn share(credits: i64, left: i64) -> String {
    if left <= 0 {
        return "all".into();
    }
    let pct = credits as f64 * 100.0 / left as f64;
    if credits > 0 && pct < 1.0 {
        "<1%".into()
    } else {
        format!("{pct:.0}%")
    }
}

fn subscription_details(sub: &Subscription) -> String {
    let mut lines = Vec::new();
    if let Some(tier) = &sub.tier {
        let status = sub
            .status
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        lines.push(format!("Plan: {tier}{status}"));
    }
    if let (Some(used), Some(limit)) = (sub.character_count, sub.character_limit) {
        lines.push(format!(
            "Credits used: {} of {}",
            fmt_int(used),
            fmt_int(limit)
        ));
    }
    if let Some(reset) = sub.next_character_count_reset_unix {
        lines.push(format!("Resets {}", costs::fmt_datetime_utc(reset)));
    }
    if let (Some(used), Some(limit)) = (sub.voice_slots_used, sub.voice_limit) {
        lines.push(format!("Voice slots: {used} of {limit}"));
    }
    if let (Some(used), Some(limit)) = (
        sub.professional_voice_slots_used,
        sub.professional_voice_limit,
    ) && limit > 0
    {
        lines.push(format!("Professional voice slots: {used} of {limit}"));
    }
    if sub.overage_allowed() {
        lines.push("Usage-based billing continues past the limit.".into());
    }
    if lines.is_empty() {
        "ElevenLabs subscription".into()
    } else {
        lines.join("\n")
    }
}

fn pass_label(pass: &str) -> &str {
    match pass {
        "attribution" => "Speaker attribution",
        "tags" => "Audio tags",
        other => other,
    }
}

/// The Claude usage chip: text and tooltip, or None without usage.
fn usage_chip(usage: &[PassUsage]) -> Option<(String, String)> {
    if usage.is_empty() {
        return None;
    }
    let tokens: u64 = usage.iter().map(PassUsage::total_tokens).sum();
    let cost: f64 = usage.iter().map(|u| u.cost_usd).sum();
    let unpriced = usage.iter().any(|u| u.unpriced > 0);
    let subscription = usage.iter().filter(|u| u.backend == "claude_code").count();
    let cost_text = if cost == 0.0 && unpriced {
        "cost unknown".to_string()
    } else {
        format!("≈ {}", fmt_usd(cost))
    };
    let suffix = if subscription == usage.len() {
        " API-equivalent (subscription)"
    } else if subscription > 0 {
        " (partly subscription)"
    } else {
        ""
    };
    let text = format!(
        "Claude ≈ {} tokens · {cost_text}{suffix}",
        fmt_tokens(tokens)
    );
    let mut tip: Vec<String> = usage
        .iter()
        .map(|u| {
            let via = if u.backend == "claude_code" {
                "Claude Code (subscription)"
            } else {
                "API key"
            };
            let cache = if u.cache_read_tokens + u.cache_write_tokens > 0 {
                format!(
                    " (+ {} cache reads, {} cache writes)",
                    fmt_tokens(u.cache_read_tokens),
                    fmt_tokens(u.cache_write_tokens)
                )
            } else {
                String::new()
            };
            format!(
                "{} via {via}: {} calls · {} input + {} output tokens{cache} · ≈ {}",
                pass_label(&u.pass),
                u.calls,
                fmt_tokens(u.input_tokens),
                fmt_tokens(u.output_tokens),
                fmt_usd(u.cost_usd)
            )
        })
        .collect();
    if subscription > 0 {
        tip.push(String::new());
        tip.push("Claude Code reports what its calls would cost at API list prices. On a Pro/Max plan they use your plan's usage limits instead of being billed.".into());
    }
    if subscription < usage.len() {
        tip.push(String::new());
        tip.push("API calls are priced at list price (Opus 5.5: $4 input / $20 output per million tokens).".into());
    }
    if unpriced {
        tip.push("Some calls used a model without a known price.".into());
    }
    Some((text, tip.join("\n")))
}

fn note_style(level: NoteLevel, p: &Palette) -> (&'static str, egui::Color32) {
    match level {
        NoteLevel::Ok => ("✔", p.approved),
        NoteLevel::Info => ("•", p.muted),
        NoteLevel::Warn => ("⚠", p.warning),
        NoteLevel::Error => ("✖", p.rejected),
    }
}

/// The Claude part of Settings. Returns true when Claude Code should be
/// (re)checked with the dialog's current path.
pub(super) fn claude_settings_section(
    ui: &mut egui::Ui,
    cfg: &mut AppConfig,
    state: &CostState,
) -> bool {
    let p = *theme::palette(ui);
    let mut check = false;
    ui.label(
        RichText::new("CLAUDE  ·  speaker attribution and audio tags")
            .small()
            .strong()
            .color(p.muted),
    );
    let before = cfg.claude_backend;
    ui.radio_value(
        &mut cfg.claude_backend,
        ClaudeBackend::ClaudeCode,
        "Claude Code — your Pro/Max subscription (default)",
    );
    if cfg.claude_backend == ClaudeBackend::ClaudeCode {
        if before != ClaudeBackend::ClaudeCode || (state.detection.is_none() && !state.detecting) {
            check = true;
        }
        ui.indent("claude_code", |ui| {
            match &state.detection {
                Some(d) if !state.detecting => {
                    for n in &d.notes {
                        let (icon, color) = note_style(n.level, &p);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon).color(color));
                            ui.add(egui::Label::new(RichText::new(&n.text).color(color)).wrap());
                        });
                    }
                    if let Some(bin) = &d.binary {
                        ui.label(
                            RichText::new(format!("Found at {}", bin.display()))
                                .small()
                                .color(p.muted),
                        );
                    }
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(RichText::new("Checking Claude Code…").color(p.muted));
                    });
                }
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!state.detecting, egui::Button::new("Check again"))
                    .clicked()
                {
                    check = true;
                }
                ui.label("Path");
                let mut path = cfg
                    .claude_code_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut path)
                            .desired_width(220.0)
                            .hint_text("auto: PATH, ~/.local/bin"),
                    )
                    .changed()
                {
                    cfg.claude_code_path =
                        (!path.trim().is_empty()).then(|| PathBuf::from(path.trim()));
                }
                if ui.button("Choose…").clicked()
                    && let Some(file) = rfd::FileDialog::new().pick_file()
                {
                    cfg.claude_code_path = Some(file);
                    check = true;
                }
            });
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Uses your plan's usage limits (shared with the Claude apps). For your own use on this computer. Sign in once with `claude auth login` in a terminal.",
                    )
                    .small()
                    .color(p.muted),
                )
                .wrap(),
            );
        });
    }
    ui.radio_value(
        &mut cfg.claude_backend,
        ClaudeBackend::ApiKey,
        "Anthropic API key — pay per use",
    );
    ui.indent("api_key", |ui| {
        ui.horizontal(|ui| {
            ui.label("Anthropic API key");
            ui.add_enabled(
                cfg.claude_backend == ClaudeBackend::ApiKey,
                egui::TextEdit::singleline(&mut cfg.anthropic_api_key)
                    .password(true)
                    .hint_text("or ANTHROPIC_API_KEY"),
            );
        });
    });
    check
}

/// The optional price of ElevenLabs credits, for showing money next to
/// credit estimates. 0 turns it off.
pub(super) fn credit_price_row(ui: &mut egui::Ui, cfg: &mut AppConfig) {
    ui.horizontal(|ui| {
        let mut price = cfg.price_per_1000_credits.unwrap_or(0.0);
        ui.add(
            egui::DragValue::new(&mut price)
                .range(0.0..=100.0)
                .speed(0.005)
                .max_decimals(3),
        )
        .on_hover_text(
            "What 1,000 credits cost you (plan price ÷ monthly credits). 0 hides money.",
        );
        cfg.price_per_1000_credits = (price > 0.0).then_some(price);
        ui.add(
            egui::TextEdit::singleline(&mut cfg.credit_currency)
                .desired_width(48.0)
                .hint_text("USD"),
        );
        ui.label(
            RichText::new("per 1,000 credits (optional)")
                .small()
                .color(theme::palette(ui).muted),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pass: &str, backend: &str, cost: f64) -> PassUsage {
        PassUsage {
            pass: pass.into(),
            backend: backend.into(),
            calls: 3,
            input_tokens: 100_000,
            output_tokens: 50_000,
            cache_read_tokens: 20_000,
            cache_write_tokens: 12_000,
            cost_usd: cost,
            unpriced: 0,
        }
    }

    #[test]
    fn usage_chip_labels_subscription_and_api() {
        assert!(usage_chip(&[]).is_none());
        let (text, tip) = usage_chip(&[
            row("attribution", "claude_code", 1.0),
            row("tags", "claude_code", 0.4),
        ])
        .unwrap();
        assert_eq!(
            text,
            "Claude ≈ 364k tokens · ≈ $1.40 API-equivalent (subscription)"
        );
        assert!(tip.contains("Speaker attribution via Claude Code (subscription): 3 calls"));
        assert!(tip.contains("usage limits"));

        let (text, tip) = usage_chip(&[row("attribution", "api", 1.4)]).unwrap();
        assert_eq!(text, "Claude ≈ 182k tokens · ≈ $1.40");
        assert!(tip.contains("list price"));

        let (text, _) = usage_chip(&[
            row("attribution", "api", 1.0),
            row("tags", "claude_code", 0.4),
        ])
        .unwrap();
        assert!(text.ends_with("(partly subscription)"));
    }

    #[test]
    fn shares() {
        assert_eq!(share(12_400, 412_300), "3%");
        assert_eq!(share(10, 412_300), "<1%");
        assert_eq!(share(500, 400), "125%");
        assert_eq!(share(0, 400), "0%");
    }
}
