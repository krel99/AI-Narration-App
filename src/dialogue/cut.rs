//! Validating a dialogue response and cutting its audio back into one take
//! per input.
//!
//! `voice_segments` say which stretch of audio belongs to which input
//! (`dialogue_input_index`). They are checked before anything is cut: every
//! input has a segment with a length, the spans run in order without
//! overlapping, each is long enough for its text and has sound in it, they
//! end inside the audio, and the character ranges cover exactly the inputs'
//! texts. Known server defects (an `[interrupting]` tag collapsing several
//! inputs onto one timestamp, compressed timings on long v3 dialogues) fail
//! these checks. Then each cut is placed in the pause between two inputs:
//! the quietest stretch near the boundary the timings give, never inside
//! sound.
//!
//! Any failure means the dialogue audio is discarded for that paragraph and
//! it is generated per segment instead (which is billed again), so the
//! checks are strict about the mapping and lenient about representation:
//! the server's characters are compared with the inputs after Unicode
//! compatibility normalisation and whitespace folding.

use super::Input;
use super::client::{Alignment, VoiceSegment};
use crate::audio::Pcm;

/// Spans of neighbouring inputs may overlap by this much (rounding).
const OVERLAP_TOL: f64 = 0.02;
/// The last span may end this far after the audio (MP3 padding, rounding).
const END_TOL: f64 = 0.15;
/// A span shorter than this per spoken character (no tags, no spaces) is
/// implausible: 25 characters a second is far faster than narration.
const MIN_SECS_PER_CHAR: f64 = 0.04;
/// How far around the timings' boundary a pause is looked for.
const SEARCH_MARGIN: f64 = 0.3;
/// Alignment times further than this outside their input's span are not
/// trusted as a hint.
const HINT_TOL: f64 = 0.25;
const FRAME_MS: usize = 10;
/// A pause is at least two quiet 10 ms frames.
const MIN_PAUSE_FRAMES: usize = 2;
/// An input needs at least three frames of sound.
const MIN_SOUND_FRAMES: usize = 3;

/// Where to cut: `bounds[i]..bounds[i + 1]` (samples) is input `i`'s take.
/// The first take starts at 0 and the last ends with the audio, so the takes
/// played back to back are the dialogue exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct Cuts {
    pub bounds: Vec<usize>,
    /// Observations worth logging that did not fail validation.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
struct Span {
    t0: f64,
    t1: f64,
    c0: i64,
    c1: i64,
}

fn spans(
    n: usize,
    segments: &[VoiceSegment],
    inputs: &[Input],
) -> Result<(Vec<Span>, Vec<String>), String> {
    let mut out: Vec<Option<Span>> = vec![None; n];
    let mut notes = Vec::new();
    for s in segments {
        let i = s.dialogue_input_index;
        if i < 0 || i as usize >= n {
            return Err(format!(
                "a voice segment belongs to input {}, but {n} inputs were sent",
                i + 1
            ));
        }
        let (a, b) = (s.start_time_seconds, s.end_time_seconds);
        if !a.is_finite() || !b.is_finite() || a < -0.001 || b < a {
            return Err(format!("input {}: invalid times {a}–{b} s", i + 1));
        }
        let i = i as usize;
        if !s.voice_id.is_empty() && s.voice_id != inputs[i].voice_id {
            notes.push(format!(
                "input {}: voice segment names voice {}, {} was sent",
                i + 1,
                s.voice_id,
                inputs[i].voice_id
            ));
        }
        let span = out[i].get_or_insert(Span {
            t0: a,
            t1: b,
            c0: s.character_start_index,
            c1: s.character_end_index,
        });
        span.t0 = span.t0.min(a);
        span.t1 = span.t1.max(b);
        span.c0 = span.c0.min(s.character_start_index);
        span.c1 = span.c1.max(s.character_end_index);
    }
    let spans = out
        .into_iter()
        .enumerate()
        .map(|(i, s)| s.ok_or_else(|| format!("input {} has no voice segment", i + 1)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((spans, notes))
}

/// Compatibility-normalised text with whitespace folded: how the server's
/// characters are compared with what was sent.
fn text_key(s: &str) -> String {
    let n = icu_normalizer::ComposingNormalizerBorrowed::new_nfkc().normalize(s);
    n.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Check the character ranges: in input order, contiguous, covering every
/// character, each with its input's text (indices count the alignment's
/// characters when there is one, the inputs' code points otherwise).
fn check_characters(
    inputs: &[Input],
    spans: &[Span],
    alignment: Option<&Alignment>,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let total = match alignment {
        Some(a) => a.characters.len(),
        None => inputs.iter().map(|i| i.text.chars().count()).sum(),
    };
    let mut expect = 0usize;
    let mut representation = false;
    for (i, (s, input)) in spans.iter().zip(inputs).enumerate() {
        if s.c0 < 0 || s.c1 < s.c0 || s.c1 as usize > total {
            return Err(format!(
                "input {}: characters {}–{} are outside the {total} characters",
                i + 1,
                s.c0,
                s.c1
            ));
        }
        let (c0, c1) = (s.c0 as usize, s.c1 as usize);
        if c0 != expect {
            return Err(format!(
                "input {}: its characters start at {c0}, expected {expect}",
                i + 1
            ));
        }
        expect = c1;
        match alignment {
            Some(a) => {
                let got: String = a.characters[c0..c1].concat();
                if got != input.text {
                    if text_key(&got) != text_key(&input.text) {
                        return Err(format!(
                            "input {}: the aligned characters “{}” are not its text",
                            i + 1,
                            got.chars().take(60).collect::<String>()
                        ));
                    }
                    representation = true;
                }
            }
            None => {
                let sent = input.text.chars().count();
                if c1 - c0 != sent {
                    return Err(format!(
                        "input {}: {} characters attributed, {sent} sent",
                        i + 1,
                        c1 - c0
                    ));
                }
            }
        }
    }
    if expect != total {
        return Err(format!(
            "{} of {total} characters belong to no input",
            total - expect
        ));
    }
    if alignment.is_none() {
        notes.push("no character alignment in the response; checked the counts only".into());
    }
    if representation {
        notes.push(
            "the aligned text differs from the sent text in Unicode form or spacing only".into(),
        );
    }
    Ok(())
}

/// Spoken characters for the plausibility check: no tags, no whitespace.
fn spoken_chars(text: &str) -> usize {
    crate::tags::strip(text)
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
}

/// End time of input `i`'s last spoken character and start time of its first,
/// from the alignment, when the alignment's times are usable.
fn speech_times(a: &Alignment, span: &Span) -> Option<(f64, f64)> {
    let n = a.characters.len();
    if a.character_start_times_seconds.len() != n || a.character_end_times_seconds.len() != n {
        return None;
    }
    let (c0, c1) = (span.c0 as usize, span.c1 as usize);
    let chars = &a.characters[c0..c1];
    // Which characters are inside a tag: find the tags in the joined text.
    let joined: String = chars.concat();
    let tags = crate::tags::find(&joined);
    let mut pos = 0usize;
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    for (k, c) in chars.iter().enumerate() {
        let in_tag = tags.iter().any(|&(x, y)| pos >= x && pos < y);
        pos += c.len();
        if in_tag || c.trim().is_empty() {
            continue;
        }
        first.get_or_insert(c0 + k);
        last = Some(c0 + k);
    }
    let (f, l) = (first?, last?);
    let (start, end) = (
        a.character_start_times_seconds[f],
        a.character_end_times_seconds[l],
    );
    (start.is_finite() && end.is_finite() && end >= start).then_some((start, end))
}

/// dBFS of each 10 ms frame's RMS.
fn frame_levels(pcm: &Pcm) -> (usize, Vec<f32>) {
    let frame = (pcm.rate as usize * FRAME_MS / 1000).max(1);
    let levels = pcm
        .samples
        .chunks(frame)
        .map(|c| {
            let ms = c.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / c.len() as f64;
            if ms <= 0.0 {
                -120.0
            } else {
                (10.0 * (ms / (32768.0f64 * 32768.0)).log10()) as f32
            }
        })
        .collect();
    (frame, levels)
}

/// Runs of frames below `threshold` within frames `lo..hi`.
fn quiet_runs(levels: &[f32], lo: usize, hi: usize, threshold: f32) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (f, &level) in levels.iter().enumerate().take(hi).skip(lo) {
        match (level < threshold, start) {
            (true, None) => start = Some(f),
            (false, Some(s)) => {
                runs.push((s, f));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, hi));
    }
    runs.retain(|(a, b)| b - a >= MIN_PAUSE_FRAMES);
    runs
}

/// The pause to cut in: one that touches the boundary region (the longest
/// of those), otherwise the nearest one (the longer on ties).
fn pick_run(runs: &[(usize, usize)], core: (f64, f64), frame_secs: f64) -> Option<(usize, usize)> {
    let secs = |(a, b): (usize, usize)| (a as f64 * frame_secs, b as f64 * frame_secs);
    let distance = |r: (usize, usize)| {
        let (a, b) = secs(r);
        if b < core.0 {
            core.0 - b
        } else if a > core.1 {
            a - core.1
        } else {
            0.0
        }
    };
    runs.iter().copied().min_by(|&x, &y| {
        distance(x)
            .total_cmp(&distance(y))
            .then((y.1 - y.0).cmp(&(x.1 - x.0)))
    })
}

/// Validate a dialogue response and find where to cut `pcm` (the decoded
/// audio) into one take per input.
pub fn cut_points(
    pcm: &Pcm,
    inputs: &[Input],
    segments: &[VoiceSegment],
    alignment: Option<&Alignment>,
) -> Result<Cuts, String> {
    let n = inputs.len();
    if n == 0 {
        return Err("no inputs".into());
    }
    if pcm.rate == 0 || pcm.samples.is_empty() {
        return Err("no audio".into());
    }
    let (spans, mut notes) = spans(n, segments, inputs)?;
    let duration = pcm.samples.len() as f64 / pcm.rate as f64;
    for (i, s) in spans.iter().enumerate() {
        if s.t1 - s.t0 < 0.001 {
            return Err(format!(
                "input {} has no length (it starts and ends at {:.3} s)",
                i + 1,
                s.t0
            ));
        }
        if i > 0 && s.t0 < spans[i - 1].t1 - OVERLAP_TOL {
            return Err(format!(
                "inputs {} and {} overlap ({:.3} s < {:.3} s)",
                i,
                i + 1,
                s.t0,
                spans[i - 1].t1
            ));
        }
        let chars = spoken_chars(&inputs[i].text);
        let min = MIN_SECS_PER_CHAR * chars as f64;
        if s.t1 - s.t0 < min {
            return Err(format!(
                "input {} lasts {:.2} s, too short for {chars} characters",
                i + 1,
                s.t1 - s.t0
            ));
        }
    }
    let end = spans[n - 1].t1;
    if end > duration + END_TOL {
        return Err(format!(
            "the timings run to {end:.2} s, past the {duration:.2} s of audio"
        ));
    }
    check_characters(inputs, &spans, alignment, &mut notes)?;

    // Levels: "quiet" is relative to how loud the speech is.
    let (frame, levels) = frame_levels(pcm);
    let frame_secs = frame as f64 / pcm.rate as f64;
    let mut sorted = levels.clone();
    sorted.sort_by(f32::total_cmp);
    let reference = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    if reference <= -100.0 {
        return Err("the audio is silent".into());
    }
    let quiet = (reference - 30.0).clamp(-65.0, -40.0);
    let frame_of = |t: f64| ((t.max(0.0) / frame_secs) as usize).min(levels.len());
    for (i, s) in spans.iter().enumerate() {
        let (a, b) = (frame_of(s.t0), frame_of(s.t1).max(frame_of(s.t0) + 1));
        let loud = levels[a.min(levels.len())..b.min(levels.len())]
            .iter()
            .filter(|&&l| l >= quiet)
            .count();
        if loud < MIN_SOUND_FRAMES {
            return Err(format!("input {} is silent", i + 1));
        }
    }

    // Alignment times as a hint for where the speech ends and starts, used
    // only when they agree with the voice segments.
    let hints: Option<Vec<(f64, f64)>> = alignment.and_then(|a| {
        spans
            .iter()
            .map(|s| speech_times(a, s))
            .collect::<Option<Vec<_>>>()
    });
    let mut ignored_hints = false;

    let mut bounds = vec![0usize];
    for k in 0..n - 1 {
        let (a, b) = (&spans[k], &spans[k + 1]);
        let mut core = (a.t1.min(b.t0), a.t1.max(b.t0));
        if let Some(h) = &hints {
            let (end_a, start_b) = (h[k].1, h[k + 1].0);
            let agrees = end_a >= a.t0 - HINT_TOL
                && end_a <= a.t1 + HINT_TOL
                && start_b >= b.t0 - HINT_TOL
                && start_b <= b.t1 + HINT_TOL
                && end_a <= start_b + 0.05;
            if agrees {
                core = (core.0.min(end_a), core.1.max(start_b));
            } else {
                ignored_hints = true;
            }
        }
        let prev = *bounds.last().unwrap();
        let lo = (core.0 - SEARCH_MARGIN).max((a.t0 + a.t1) / 2.0);
        let hi = (core.1 + SEARCH_MARGIN)
            .min((b.t0 + b.t1) / 2.0)
            .min(duration);
        // Whole frames inside the window, after the previous cut.
        let f_lo = ((lo.max(0.0) / frame_secs).ceil() as usize).max(prev.div_ceil(frame));
        let f_hi = ((hi / frame_secs).floor() as usize).min(levels.len());
        let mut chosen = None;
        for threshold in [quiet, quiet + 10.0] {
            if f_lo < f_hi
                && let Some(r) = pick_run(
                    &quiet_runs(&levels, f_lo, f_hi, threshold),
                    core,
                    frame_secs,
                )
            {
                chosen = Some(r);
                break;
            }
        }
        let Some((ra, rb)) = chosen else {
            return Err(format!(
                "no pause between inputs {} and {} near {:.2} s",
                k + 1,
                k + 2,
                core.0
            ));
        };
        // The middle of the pause, moved to the quietest sample within 2 ms.
        let (sa, sb) = (ra * frame, (rb * frame).min(pcm.samples.len()));
        let mid = (sa + sb) / 2;
        let reach = pcm.rate as usize * 2 / 1000;
        let cut = (mid.saturating_sub(reach).max(sa)..(mid + reach + 1).min(sb))
            .min_by_key(|&j| (pcm.samples[j].unsigned_abs(), j.abs_diff(mid)))
            .unwrap_or(mid);
        if cut <= prev || cut >= pcm.samples.len() {
            return Err(format!(
                "the cut between inputs {} and {} is out of order",
                k + 1,
                k + 2
            ));
        }
        bounds.push(cut);
    }
    bounds.push(pcm.samples.len());
    if ignored_hints {
        notes.push("character times disagree with the voice segments; used the segments".into());
    }
    Ok(Cuts { bounds, notes })
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Synth, synth};
    use super::*;

    const RATE: u32 = 16_000;

    fn inputs(texts: &[&str], voices: &[&str]) -> Vec<Input> {
        texts
            .iter()
            .zip(voices)
            .map(|(t, v)| Input {
                text: t.to_string(),
                voice_id: v.to_string(),
            })
            .collect()
    }

    const TEXTS: [&str; 3] = ["\"Who goes there?\"", "asked the guard.", "\"Only me.\""];
    const VOICES: [&str; 3] = ["vg", "vn", "vv"];

    fn run(s: &Synth, texts: &[&str], voices: &[&str]) -> Result<Cuts, String> {
        cut_points(
            &s.pcm,
            &inputs(texts, voices),
            &s.segments,
            Some(&s.alignment),
        )
    }

    /// Each cut lies in the silence between two lines, near its middle.
    fn assert_cuts_in_gaps(s: &Synth, cuts: &Cuts) {
        let n = s.speech.len();
        assert_eq!(cuts.bounds.len(), n + 1);
        assert_eq!(cuts.bounds[0], 0);
        assert_eq!(cuts.bounds[n], s.pcm.samples.len());
        for k in 0..n - 1 {
            let (end, start) = (s.speech[k].1, s.speech[k + 1].0);
            let cut = cuts.bounds[k + 1];
            assert!(
                cut > end && cut < start,
                "cut {cut} not in gap {end}..{start}"
            );
            let mid = (end + start) / 2;
            let tol = RATE as usize * 15 / 1000;
            assert!(
                cut.abs_diff(mid) <= tol,
                "cut {cut} far from the gap's middle {mid}"
            );
        }
    }

    #[test]
    fn cuts_land_in_the_pauses() {
        let s = synth(&TEXTS, &VOICES, RATE);
        let cuts = run(&s, &TEXTS, &VOICES).unwrap();
        assert_cuts_in_gaps(&s, &cuts);
        assert!(cuts.notes.is_empty(), "{:?}", cuts.notes);
    }

    #[test]
    fn boundaries_at_either_end_of_the_pause_and_several_segments_per_input() {
        // The server may give the pause to the next speaker instead.
        let mut s = synth(&TEXTS, &VOICES, RATE);
        for k in 0..2 {
            let end = s.speech[k].1 as f64 / RATE as f64;
            s.segments[k].end_time_seconds = end;
            s.segments[k + 1].start_time_seconds = end;
        }
        assert_cuts_in_gaps(&s, &run(&s, &TEXTS, &VOICES).unwrap());

        // One input reported as two segments.
        let mut s = synth(&TEXTS, &VOICES, RATE);
        let seg = s.segments[1].clone();
        let mid_t = (seg.start_time_seconds + seg.end_time_seconds) / 2.0;
        let mid_c = (seg.character_start_index + seg.character_end_index) / 2;
        let mut first = seg.clone();
        first.end_time_seconds = mid_t;
        first.character_end_index = mid_c;
        let mut second = seg;
        second.start_time_seconds = mid_t;
        second.character_start_index = mid_c;
        s.segments.splice(1..2, [first, second]);
        assert_cuts_in_gaps(&s, &run(&s, &TEXTS, &VOICES).unwrap());
    }

    #[test]
    fn a_pause_inside_a_line_is_not_taken_for_the_turn() {
        // A 120 ms pause 160 ms before the end of the first line ("Who goes,
        // there?"): the longer pause between the speakers wins.
        let mut s = synth(&TEXTS, &VOICES, RATE);
        let end = s.speech[0].1;
        let ms = |m: usize| RATE as usize * m / 1000;
        for x in &mut s.pcm.samples[end - ms(280)..end - ms(160)] {
            *x = 0;
        }
        assert_cuts_in_gaps(&s, &run(&s, &TEXTS, &VOICES).unwrap());
    }

    #[test]
    fn broken_voice_segments_fail() {
        let base = synth(&TEXTS, &VOICES, RATE);
        let err = |f: &dyn Fn(&mut Synth)| {
            let mut s = synth(&TEXTS, &VOICES, RATE);
            f(&mut s);
            run(&s, &TEXTS, &VOICES).unwrap_err()
        };
        assert!(run(&base, &TEXTS, &VOICES).is_ok());

        // The [interrupting] defect: inputs collapse onto one timestamp.
        let e = err(&|s| {
            let t = s.segments[1].start_time_seconds;
            s.segments[1].end_time_seconds = t;
            s.segments[2].start_time_seconds = t;
        });
        assert!(e.contains("input 2 has no length"), "{e}");
        let e = err(&|s| {
            s.segments.remove(1);
        });
        assert!(e.contains("input 2 has no voice segment"), "{e}");
        let e = err(&|s| s.segments[2].dialogue_input_index = 3);
        assert!(e.contains("4, but 3 inputs"), "{e}");
        let e = err(&|s| s.segments[0].dialogue_input_index = -1);
        assert!(e.contains("input 0"), "{e}");
        let e = err(&|s| {
            s.segments[1].start_time_seconds = s.segments[0].start_time_seconds + 0.1;
        });
        assert!(e.contains("overlap"), "{e}");
        let e = err(&|s| s.segments[2].end_time_seconds += 1.0);
        assert!(e.contains("past the"), "{e}");
        let e = err(&|s| s.segments[1].end_time_seconds = f64::NAN);
        assert!(e.contains("invalid times"), "{e}");
        // Compressed timings: a line squeezed into 0.2 s.
        let e = err(&|s| {
            let t = s.segments[2].start_time_seconds;
            s.segments[1].end_time_seconds = t - 0.01;
            s.segments[1].start_time_seconds = t - 0.2;
            s.segments[0].end_time_seconds = t - 0.2;
        });
        assert!(e.contains("too short"), "{e}");
    }

    #[test]
    fn character_ranges_must_match_the_inputs() {
        let err = |f: &dyn Fn(&mut Synth)| {
            let mut s = synth(&TEXTS, &VOICES, RATE);
            f(&mut s);
            run(&s, &TEXTS, &VOICES).unwrap_err()
        };
        // One character missing from the alignment.
        let e = err(&|s| {
            s.alignment.characters.pop();
            s.alignment.character_start_times_seconds.pop();
            s.alignment.character_end_times_seconds.pop();
        });
        assert!(
            e.contains("outside") || e.contains("belong to no input"),
            "{e}"
        );
        // Ranges shifted by one.
        let e = err(&|s| {
            s.segments[1].character_start_index += 1;
        });
        assert!(e.contains("start at"), "{e}");
        // The ranges swapped between inputs 2 and 3 (counts no longer match the text).
        let e = err(&|s| {
            let a = s.segments[1].character_start_index;
            s.segments[1].character_end_index = a + 10;
            s.segments[2].character_start_index = a + 10;
        });
        assert!(e.contains("not its text"), "{e}");
        // A character the voice segments do not cover.
        let e = err(&|s| {
            s.alignment.characters.push("!".into());
            s.alignment.character_start_times_seconds.push(2.0);
            s.alignment.character_end_times_seconds.push(2.1);
        });
        assert!(e.contains("1 of 44 characters belong to no input"), "{e}");
        // Without an alignment the counts are still checked.
        let s = synth(&TEXTS, &VOICES, RATE);
        let ins = inputs(&TEXTS, &VOICES);
        let ok = cut_points(&s.pcm, &ins, &s.segments, None).unwrap();
        assert!(
            ok.notes
                .iter()
                .any(|n| n.contains("no character alignment"))
        );
        let mut segs = s.segments.clone();
        segs[0].character_end_index -= 1;
        segs[1].character_start_index -= 1;
        assert!(cut_points(&s.pcm, &ins, &segs, None).is_err());
    }

    #[test]
    fn non_ascii_text_is_validated_per_character() {
        let texts = [
            "„Kdo je tam?“",
            "zeptal se strážný — tiše.",
            "„Jenom já…“ řekl Vortram.",
        ];
        let s = synth(&texts, &VOICES, RATE);
        assert_cuts_in_gaps(&s, &run(&s, &texts, &VOICES).unwrap());

        // A server that answers in decomposed form (NFD) or expands “…”: its
        // indices count its own characters, and the text still matches.
        let mut d = synth(&texts, &VOICES, RATE);
        let decomposed: Vec<String> = texts
            .iter()
            .map(|t| t.replace('á', "a\u{301}").replace('…', "..."))
            .collect();
        let mut alignment = Alignment::default();
        let mut offset = 0i64;
        for (i, t) in decomposed.iter().enumerate() {
            let (a, b) = d.speech[i];
            let per = (b - a) as f64 / RATE as f64 / t.chars().count() as f64;
            for (k, c) in t.chars().enumerate() {
                alignment.characters.push(c.to_string());
                let t0 = a as f64 / RATE as f64 + per * k as f64;
                alignment.character_start_times_seconds.push(t0);
                alignment.character_end_times_seconds.push(t0 + per);
            }
            let len = t.chars().count() as i64;
            d.segments[i].character_start_index = offset;
            d.segments[i].character_end_index = offset + len;
            offset += len;
        }
        assert!(alignment.characters.len() > texts.iter().map(|t| t.chars().count()).sum());
        d.alignment = alignment;
        let cuts = run(&d, &texts, &VOICES).unwrap();
        assert_cuts_in_gaps(&d, &cuts);
        assert!(cuts.notes.iter().any(|n| n.contains("Unicode form")));

        // Different words are not a representation difference.
        let mut w = synth(&texts, &VOICES, RATE);
        w.alignment.characters[3] = "x".into();
        assert!(
            run(&w, &texts, &VOICES)
                .unwrap_err()
                .contains("not its text")
        );
    }

    #[test]
    fn no_pause_or_no_sound_fails() {
        // Lines glued together: no silence between the first two.
        let mut s = synth(&TEXTS, &VOICES, RATE);
        let (end, start) = (s.speech[0].1, s.speech[1].0);
        for (j, x) in s.pcm.samples[end..start].iter_mut().enumerate() {
            *x = ((j as f64 * 0.11).sin() * 8000.0) as i16;
        }
        let e = run(&s, &TEXTS, &VOICES).unwrap_err();
        assert!(e.contains("no pause between inputs 1 and 2"), "{e}");

        // A silent line.
        let mut s = synth(&TEXTS, &VOICES, RATE);
        let (a, b) = s.speech[2];
        for x in &mut s.pcm.samples[a..b] {
            *x = 0;
        }
        let e = run(&s, &TEXTS, &VOICES).unwrap_err();
        assert!(e.contains("input 3 is silent"), "{e}");

        let mut s = synth(&TEXTS, &VOICES, RATE);
        s.pcm.samples.iter_mut().for_each(|x| *x = 0);
        assert!(run(&s, &TEXTS, &VOICES).unwrap_err().contains("silent"));
    }

    #[test]
    fn misleading_character_times_are_ignored() {
        // Compressed character times (all at the start) do not move the cut.
        let mut s = synth(&TEXTS, &VOICES, RATE);
        for t in s
            .alignment
            .character_start_times_seconds
            .iter_mut()
            .chain(s.alignment.character_end_times_seconds.iter_mut())
        {
            *t = 0.01;
        }
        let cuts = run(&s, &TEXTS, &VOICES).unwrap();
        assert_cuts_in_gaps(&s, &cuts);
        assert!(cuts.notes.iter().any(|n| n.contains("disagree")));
        // Tags are not speech for the hint either.
        let texts = [
            "\"[whispers] Who goes there?\"",
            "asked the guard.",
            "\"[laughs] Only me.\"",
        ];
        let s = synth(&texts, &VOICES, RATE);
        assert_cuts_in_gaps(&s, &run(&s, &texts, &VOICES).unwrap());
    }

    #[test]
    fn voice_id_mismatch_is_only_noted() {
        let mut s = synth(&TEXTS, &VOICES, RATE);
        s.segments[1].voice_id = "other".into();
        let cuts = run(&s, &TEXTS, &VOICES).unwrap();
        assert!(cuts.notes.iter().any(|n| n.contains("voice other")));
    }
}
