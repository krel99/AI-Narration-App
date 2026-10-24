//! Cost estimates (pure, no I/O): Claude list prices, the ElevenLabs credit
//! rate per character and where it comes from, rough token estimates for the
//! Claude passes, and the number formats the UI uses.
//!
//! Credits per character are account- and date-specific (see
//! docs/design.md), so the best rate is the one LEARNED from this library's
//! own takes; the API's model multiplier and a fallback table come after it.

use crate::model::SegmentationSettings;

// ---- Claude prices --------------------------------------------------------

/// Claude list prices in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClaudePrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

/// The one place Claude prices live (first-party API list prices).
/// Haiku 5.5 is priced for prompts up to 100K tokens, which chunks stay under.
const CLAUDE_PRICES: &[(&str, ClaudePrice)] = &[
    (
        "claude-opus-5-5",
        ClaudePrice {
            input: 4.0,
            output: 20.0,
            cache_read: 0.20,
        },
    ),
    (
        "claude-sonnet-5-5",
        ClaudePrice {
            input: 2.0,
            output: 10.0,
            cache_read: 0.20,
        },
    ),
    (
        "claude-haiku-5-5",
        ClaudePrice {
            input: 0.10,
            output: 0.50,
            cache_read: 0.01,
        },
    ),
    (
        "claude-fable-5-1",
        ClaudePrice {
            input: 10.0,
            output: 50.0,
            cache_read: 0.25,
        },
    ),
    (
        "claude-opus-5",
        ClaudePrice {
            input: 5.0,
            output: 25.0,
            cache_read: 0.50,
        },
    ),
    (
        "claude-sonnet-5",
        ClaudePrice {
            input: 2.0,
            output: 10.0,
            cache_read: 0.20,
        },
    ),
];

/// Cache writes cost 1.25x the input price (5-minute TTL).
pub const CACHE_WRITE_MULTIPLIER: f64 = 1.25;

/// List price for a model id (or the `opus` / `sonnet` / `haiku` aliases).
pub fn claude_price(model: &str) -> Option<ClaudePrice> {
    let m = model.trim().to_lowercase();
    // `claude-opus-5-5[1m]` and similar context-window suffixes.
    let m = m.split('[').next().unwrap_or("").trim();
    let id = match m {
        "opus" => "claude-opus-5-5",
        "sonnet" => "claude-sonnet-5-5",
        "haiku" => "claude-haiku-5-5",
        other => other,
    };
    CLAUDE_PRICES
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, p)| *p)
}

/// API-equivalent cost of one call, or None for a model without a price.
pub fn claude_cost_usd(
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> Option<f64> {
    let p = claude_price(model)?;
    Some(
        (input as f64 * p.input
            + output as f64 * p.output
            + cache_read as f64 * p.cache_read
            + cache_write as f64 * p.input * CACHE_WRITE_MULTIPLIER)
            / 1_000_000.0,
    )
}

// ---- Claude token estimates ---------------------------------------------------

/// What the estimate needs to know about a paragraph of the Text tab.
#[derive(Debug, Clone, Copy)]
pub struct ParagraphInfo {
    pub chars: usize,
    pub dialogue: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PassEstimate {
    pub requests: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClaudeEstimate {
    pub attribution: PassEstimate,
    /// None when audio tags are not added automatically.
    pub tags: Option<PassEstimate>,
}

impl ClaudeEstimate {
    pub fn input_tokens(&self) -> u64 {
        self.attribution.input_tokens + self.tags.map_or(0, |t| t.input_tokens)
    }

    pub fn output_tokens(&self) -> u64 {
        self.attribution.output_tokens + self.tags.map_or(0, |t| t.output_tokens)
    }

    pub fn requests(&self) -> usize {
        self.attribution.requests + self.tags.map_or(0, |t| t.requests)
    }

    /// List-price cost, as if every input token were uncached.
    pub fn cost_usd(&self, model: &str) -> Option<f64> {
        claude_cost_usd(model, self.input_tokens(), self.output_tokens(), 0, 0)
    }
}

/// Fixed per-request cost of each pass: system prompt + JSON schema
/// (characters, as given by `llm`), and the CLI's own additions when the
/// request goes through Claude Code (its StructuredOutput tool and framing).
#[derive(Debug, Clone, Copy)]
pub struct PromptOverhead {
    pub attribution_chars: usize,
    pub tags_chars: usize,
    pub claude_code: bool,
}

/// Extra input tokens per request that Claude Code adds (observed ~1150
/// input tokens for a tiny prompt with a custom system prompt).
const CLAUDE_CODE_OVERHEAD_TOKENS: f64 = 700.0;
/// Segments per dialogue paragraph, on average (quote, tag, quote).
const SEGMENTS_PER_DIALOGUE_PARAGRAPH: f64 = 3.0;

/// Rough characters per token: English prose packs about 4, most other
/// languages fewer.
pub fn chars_per_token(language: &str) -> f64 {
    if language == "en" { 4.0 } else { 3.0 }
}

fn density_share(density: &str) -> f64 {
    match density {
        "light" => 0.25,
        "rich" => 0.8,
        _ => 0.5,
    }
}

/// Rough token estimate for "Segment & add audio tags", mirroring the
/// worker's chunking: attribution windows of `chunk_paragraphs` with
/// `overlap_paragraphs` of context, then (when `tags_density` is given) the
/// direction pass over every paragraph. Retries are not included.
pub fn estimate_claude(
    paragraphs: &[ParagraphInfo],
    settings: &SegmentationSettings,
    tags_density: Option<&str>,
    overhead: PromptOverhead,
    chars_per_token: f64,
) -> ClaudeEstimate {
    let cpt = chars_per_token.max(1.0);
    let tok = |chars: usize| chars as f64 / cpt;
    let cc = if overhead.claude_code {
        CLAUDE_CODE_OVERHEAD_TOKENS
    } else {
        0.0
    };
    let chunk = settings.chunk_paragraphs.max(1);

    // Attribution: only paragraphs with dialogue are targets.
    let targets: Vec<usize> = paragraphs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.dialogue)
        .map(|(i, _)| i)
        .collect();
    let mut windows: Vec<Vec<usize>> = Vec::new();
    for &t in &targets {
        match windows.last_mut() {
            Some(w) if t / chunk == w[0] / chunk => w.push(t),
            _ => windows.push(vec![t]),
        }
    }
    let (mut a_in, mut a_out) = (0.0, 0.0);
    for w in &windows {
        let lo = w[0].saturating_sub(settings.overlap_paragraphs);
        let hi = *w.last().unwrap_or(&w[0]);
        a_in += tok(overhead.attribution_chars) + cc + 80.0;
        for p in &paragraphs[lo..=hi] {
            a_in += tok(p.chars) + 8.0;
        }
        for &t in w {
            // The text comes back once more, split up, with JSON per segment.
            a_out += tok(paragraphs[t].chars) * 1.1 + SEGMENTS_PER_DIALOGUE_PARAGRAPH * 18.0;
        }
        a_out += 40.0;
    }
    let attribution = PassEstimate {
        requests: windows.len(),
        input_tokens: a_in.round() as u64,
        output_tokens: a_out.round() as u64,
    };

    let tags = tags_density.map(|density| {
        let share = density_share(density);
        let (mut t_in, mut t_out, mut requests) = (0.0, 0.0, 0usize);
        for lo in (0..paragraphs.len()).step_by(chunk) {
            let hi = (lo + chunk).min(paragraphs.len());
            requests += 1;
            t_in += tok(overhead.tags_chars) + cc + 60.0;
            for p in &paragraphs[lo.saturating_sub(settings.overlap_paragraphs)..lo] {
                t_in += tok(p.chars);
            }
            for p in &paragraphs[lo..hi] {
                let segs = if p.dialogue {
                    SEGMENTS_PER_DIALOGUE_PARAGRAPH
                } else {
                    1.0
                };
                t_in += tok(p.chars) + segs * 12.0;
                // Tagged segments come back in full, with a few tags each.
                t_out += share * (tok(p.chars) + segs * 14.0);
            }
            t_out += 15.0;
        }
        PassEstimate {
            requests,
            input_tokens: t_in.round() as u64,
            output_tokens: t_out.round() as u64,
        }
    });
    ClaudeEstimate { attribution, tags }
}

// ---- ElevenLabs credits ------------------------------------------------------

/// How many recent exact takes the learned rate looks at.
pub const LEARN_FROM_TAKES: usize = 50;
const LEARN_MIN_TAKES: usize = 3;
const LEARN_MIN_CHARS: i64 = 200;

/// Recent exact takes of one model: their credits and spoken characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearnedSample {
    pub takes: usize,
    pub credits: i64,
    pub chars: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateBasis {
    /// Measured from this library's own takes (`character-cost` headers).
    Learned { takes: usize },
    /// `character_cost_multiplier` from GET /v1/models.
    ListRate,
    /// Built-in table; used before the first takes or a /v1/models fetch.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreditRate {
    pub per_char: f64,
    pub basis: RateBasis,
}

impl CreditRate {
    pub fn credits(&self, chars: usize) -> i64 {
        (chars as f64 * self.per_char).round() as i64
    }

    /// Where the number comes from, for the UI.
    pub fn basis_text(&self) -> String {
        match self.basis {
            RateBasis::Learned { takes } => format!(
                "rate from your last {takes} take{}",
                if takes == 1 { "" } else { "s" }
            ),
            RateBasis::ListRate => {
                "ElevenLabs list rate for this model; paid plans often bill less".into()
            }
            RateBasis::Default => "default rate for this model; paid plans often bill less".into(),
        }
    }
}

/// Fallback credits per character: v4/v3/multilingual 1.0, flash/turbo 0.5.
pub fn default_multiplier(model: &str) -> f64 {
    let m = model.to_lowercase();
    if m.contains("flash") || m.contains("turbo") || m.contains("conversational") {
        0.5
    } else {
        1.0
    }
}

/// Credits per character for `model`: learned, else the API's list rate,
/// else the fallback table.
pub fn credit_rate(
    model: &str,
    learned: Option<LearnedSample>,
    list_rate: Option<f64>,
) -> CreditRate {
    if let Some(s) = learned
        && s.takes >= LEARN_MIN_TAKES
        && s.chars >= LEARN_MIN_CHARS
        && s.credits >= 0
    {
        return CreditRate {
            per_char: s.credits as f64 / s.chars as f64,
            basis: RateBasis::Learned { takes: s.takes },
        };
    }
    if let Some(r) = list_rate.filter(|r| r.is_finite() && *r >= 0.0) {
        return CreditRate {
            per_char: r,
            basis: RateBasis::ListRate,
        };
    }
    CreditRate {
        per_char: default_multiplier(model),
        basis: RateBasis::Default,
    }
}

/// Money for `credits` at the user's price per 1,000 credits.
pub fn credits_money(credits: i64, price_per_1000: f64) -> f64 {
    credits as f64 * price_per_1000 / 1000.0
}

// ---- formatting ---------------------------------------------------------------

/// 412300 → "412,300".
pub fn fmt_int(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// 182_400 → "182k", 1_250_000 → "1.2M", 950 → "950".
pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// "$1.40"; small amounts keep a meaningful digit ("$0.004"), zero is "$0".
pub fn fmt_usd(x: f64) -> String {
    if x <= 0.0 {
        "$0".into()
    } else if x < 0.01 {
        if x < 0.001 {
            "<$0.001".into()
        } else {
            format!("${x:.3}")
        }
    } else {
        format!("${x:.2}")
    }
}

/// "2.98 USD", or "€2.98" when the currency is a symbol.
pub fn fmt_money(x: f64, currency: &str) -> String {
    let c = currency.trim();
    if c.is_empty() {
        format!("{x:.2}")
    } else if c.chars().count() == 1 && !c.chars().all(char::is_alphanumeric) {
        format!("{c}{x:.2}")
    } else {
        format!("{x:.2} {c}")
    }
}

/// Days since 1970-01-01 → (year, month 1-12, day 1-31), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Unix seconds → "3 Nov" (UTC).
pub fn fmt_day_month(unix: i64) -> String {
    let (_, m, d) = civil_from_days(unix.div_euclid(86_400));
    format!("{d} {}", MONTHS[(m - 1) as usize])
}

/// Unix seconds → "3 Nov 2026, 14:05 UTC".
pub fn fmt_datetime_utc(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    let secs = unix.rem_euclid(86_400);
    format!(
        "{d} {} {y}, {:02}:{:02} UTC",
        MONTHS[(m - 1) as usize],
        secs / 3600,
        secs / 60 % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_and_costs() {
        assert_eq!(claude_price("claude-opus-5-5").unwrap().input, 4.0);
        assert_eq!(claude_price("opus"), claude_price("claude-opus-5-5"));
        assert_eq!(
            claude_price("claude-opus-5-5[1m]"),
            claude_price("claude-opus-5-5")
        );
        // claude-opus-5 must not borrow claude-opus-5-5's price, or vice versa.
        assert_eq!(claude_price("claude-opus-5").unwrap().input, 5.0);
        assert!(claude_price("gpt-9").is_none());
        // 1M input + 1M output on Opus 5.5.
        let c = claude_cost_usd("claude-opus-5-5", 1_000_000, 1_000_000, 0, 0).unwrap();
        assert!((c - 24.0).abs() < 1e-9);
        // Cache reads at $0.20/MTok, writes at 1.25x input.
        let c = claude_cost_usd("claude-opus-5-5", 0, 0, 1_000_000, 1_000_000).unwrap();
        assert!((c - (0.20 + 5.0)).abs() < 1e-9);
        let haiku = claude_cost_usd("claude-haiku-5-5", 2000, 1000, 0, 0).unwrap();
        assert!((haiku - 0.0007).abs() < 1e-12);
    }

    #[test]
    fn credit_rate_prefers_learned_then_list_then_default() {
        let learned = LearnedSample {
            takes: 50,
            credits: 4400,
            chars: 10_000,
        };
        let r = credit_rate("eleven_v4", Some(learned), Some(1.0));
        assert!((r.per_char - 0.44).abs() < 1e-12);
        assert_eq!(r.basis, RateBasis::Learned { takes: 50 });
        assert_eq!(r.credits(1000), 440);
        assert_eq!(r.basis_text(), "rate from your last 50 takes");

        // Too little data to learn from: the list rate wins.
        let thin = LearnedSample {
            takes: 2,
            credits: 10,
            chars: 30,
        };
        let r = credit_rate("eleven_v4", Some(thin), Some(1.0));
        assert_eq!((r.per_char, r.basis), (1.0, RateBasis::ListRate));

        let r = credit_rate("eleven_flash_v2_5", None, None);
        assert_eq!((r.per_char, r.basis), (0.5, RateBasis::Default));
        assert_eq!(default_multiplier("eleven_v4_turbo"), 0.5);
        assert_eq!(default_multiplier("eleven_v3"), 1.0);
        assert_eq!(default_multiplier("eleven_multilingual_v2"), 1.0);
        assert_eq!(default_multiplier("eleven_turbo_v2_5"), 0.5);
        assert!(r.basis_text().contains("default rate"));
        assert!((credits_money(12_400, 0.24) - 2.976).abs() < 1e-9);
    }

    #[test]
    fn claude_estimate_follows_chunking() {
        let settings = SegmentationSettings {
            chunk_paragraphs: 2,
            overlap_paragraphs: 1,
            ..Default::default()
        };
        let p = |chars, dialogue| ParagraphInfo { chars, dialogue };
        // Dialogue in P0 and P1 (one window: 0 / 2 == 1 / 2) and in P4.
        let paras = [
            p(400, true),
            p(400, true),
            p(400, false),
            p(400, false),
            p(400, true),
        ];
        let overhead = PromptOverhead {
            attribution_chars: 4000,
            tags_chars: 4000,
            claude_code: false,
        };
        let e = estimate_claude(&paras, &settings, None, overhead, 4.0);
        assert_eq!(e.attribution.requests, 2);
        assert!(e.tags.is_none());
        // Window 1: P0 + P1; window 2: P3 (context) + P4.
        let expected_in = 2.0 * (1000.0 + 80.0) + 4.0 * (100.0 + 8.0);
        assert_eq!(e.attribution.input_tokens, expected_in as u64);
        assert!(e.attribution.output_tokens > 3 * 100);

        let with_tags = estimate_claude(&paras, &settings, Some("moderate"), overhead, 4.0);
        let tags = with_tags.tags.unwrap();
        assert_eq!(tags.requests, 3, "windows 0..2, 2..4, 4..5");
        assert!(with_tags.input_tokens() > e.input_tokens());
        let rich = estimate_claude(&paras, &settings, Some("rich"), overhead, 4.0);
        assert!(rich.tags.unwrap().output_tokens > tags.output_tokens);

        // Claude Code adds its own framing to every request.
        let cc = estimate_claude(
            &paras,
            &settings,
            None,
            PromptOverhead {
                claude_code: true,
                ..overhead
            },
            4.0,
        );
        assert!(cc.attribution.input_tokens > e.attribution.input_tokens);
        assert!(e.cost_usd("claude-opus-5-5").unwrap() > 0.0);
        assert!(e.cost_usd("unknown-model").is_none());

        let none = estimate_claude(&[p(100, false)], &settings, None, overhead, 4.0);
        assert_eq!(none.attribution, PassEstimate::default());
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_int(412_300), "412,300");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1_000), "1,000");
        assert_eq!(fmt_int(-12_400), "-12,400");
        assert_eq!(fmt_tokens(182_400), "182k");
        assert_eq!(fmt_tokens(1_250_000), "1.2M");
        assert_eq!(fmt_tokens(3_100), "3.1k");
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_usd(1.4), "$1.40");
        assert_eq!(fmt_usd(0.004), "$0.004");
        assert_eq!(fmt_usd(0.0), "$0");
        assert_eq!(fmt_usd(0.0001), "<$0.001");
        assert_eq!(fmt_money(2.976, "USD"), "2.98 USD");
        assert_eq!(fmt_money(2.976, "€"), "€2.98");
        assert_eq!(fmt_money(2.976, ""), "2.98");
        // 2026-11-03T00:00:00Z
        assert_eq!(fmt_day_month(1_793_664_000), "3 Nov");
        assert_eq!(fmt_day_month(0), "1 Jan");
        assert_eq!(
            fmt_datetime_utc(951_782_400 + 3_660),
            "29 Feb 2000, 01:01 UTC"
        );
        assert_eq!(chars_per_token("en"), 4.0);
    }
}
