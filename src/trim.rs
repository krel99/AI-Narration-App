//! Silence trimming of takes. ElevenLabs takes start and end with uneven
//! silence, so "0 ms within a paragraph" does not give even gaps. A trim cuts a
//! take's leading and trailing silence (never the middle) with a short fade at
//! each end.
//!
//! Trimming is non-destructive: the WAV file is never modified. A trim is a
//! `(start_ms, end_ms)` range stored on the take row, applied when the take is
//! loaded for playback or export ([`load_take`]). Automatic trimming of new
//! takes is off by default.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::audio::{self, Pcm};
use crate::db::Library;
use crate::model::Take;

/// Analysis frame for the silence detector.
pub const FRAME_MS: u32 = 10;

/// Per-project trimming settings (`ProjectSettings.trim`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrimSettings {
    /// Trim every new take automatically. Off by default; the waveform's
    /// Auto-trim and Review's "Auto-trim chapter" work either way.
    pub enabled: bool,
    /// A 10 ms frame whose RMS is above this level (dBFS) counts as sound.
    pub threshold_db: f32,
    /// Silence kept before the first sound.
    pub keep_head_ms: u32,
    /// Silence kept after the last sound (decays and breaths fade slowly).
    pub keep_tail_ms: u32,
    /// Linear fade-in and fade-out at the cut points.
    pub fade_ms: u32,
}

impl Default for TrimSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_db: -50.0,
            keep_head_ms: 40,
            keep_tail_ms: 80,
            fade_ms: 10,
        }
    }
}

/// Length of `pcm` in milliseconds, rounded up so that a trim ending at the
/// duration always keeps the last sample.
pub fn duration_ms(pcm: &Pcm) -> u32 {
    if pcm.rate == 0 {
        return 0;
    }
    (pcm.samples.len() as u64 * 1000).div_ceil(pcm.rate as u64) as u32
}

fn ms_to_sample(pcm: &Pcm, ms: u32) -> usize {
    ((ms as u64 * pcm.rate as u64 / 1000) as usize).min(pcm.samples.len())
}

/// Find the range to keep: from the first to the last 10 ms frame above the
/// threshold, widened by the head/tail margins and clamped to the take.
/// Returns `None` for an empty or all-silent take (nothing is ever cut to
/// nothing). The range is never empty or inverted.
pub fn detect(pcm: &Pcm, s: &TrimSettings) -> Option<(u32, u32)> {
    if pcm.rate == 0 || pcm.samples.is_empty() {
        return None;
    }
    let frame = ((pcm.rate as u64 * FRAME_MS as u64 / 1000) as usize).max(1);
    // Compare mean squares, so no square root or log per frame.
    let full_scale = 32768.0f64;
    let threshold = 10f64.powf(s.threshold_db as f64 / 10.0) * full_scale * full_scale;
    let loud = |chunk: &[i16]| {
        let sum: f64 = chunk.iter().map(|&x| (x as f64) * (x as f64)).sum();
        sum / chunk.len() as f64 > threshold
    };
    let frames: Vec<&[i16]> = pcm.samples.chunks(frame).collect();
    let first = frames.iter().position(|c| loud(c))?;
    let last = frames.iter().rposition(|c| loud(c))?;
    let start_sample = first * frame;
    let end_sample = ((last + 1) * frame).min(pcm.samples.len());
    let rate = pcm.rate as u64;
    let start_ms = (start_sample as u64 * 1000 / rate) as u32;
    let end_ms = (end_sample as u64 * 1000).div_ceil(rate) as u32;
    let dur = duration_ms(pcm);
    let start = start_ms.saturating_sub(s.keep_head_ms);
    let end = end_ms.saturating_add(s.keep_tail_ms).min(dur);
    (end > start).then_some((start, end))
}

/// Cut `pcm` to `start_ms..end_ms` and fade the cut points in and out
/// linearly over `fade_ms` (at most half the kept length). A range that
/// would leave nothing (e.g. a stale trim) returns the audio untouched.
pub fn apply(pcm: &Pcm, start_ms: u32, end_ms: u32, fade_ms: u32) -> Pcm {
    let len = pcm.samples.len();
    let a = ms_to_sample(pcm, start_ms);
    let b = if end_ms >= duration_ms(pcm) {
        len
    } else {
        ms_to_sample(pcm, end_ms)
    };
    if b <= a {
        return pcm.clone();
    }
    let mut samples = pcm.samples[a..b].to_vec();
    let n = ms_to_sample(pcm, fade_ms).min(samples.len() / 2);
    if n > 0 {
        let k = samples.len();
        for i in 0..n {
            let gain = i as f32 / n as f32;
            samples[i] = (samples[i] as f32 * gain).round() as i16;
            samples[k - 1 - i] = (samples[k - 1 - i] as f32 * gain).round() as i16;
        }
    }
    Pcm {
        rate: pcm.rate,
        samples,
    }
}

/// Min/max sample per bucket, for drawing a waveform. Returns `buckets`
/// pairs (each covering at least one sample), or nothing for empty audio.
pub fn peaks(pcm: &Pcm, buckets: usize) -> Vec<(i16, i16)> {
    let len = pcm.samples.len();
    if len == 0 || buckets == 0 {
        return Vec::new();
    }
    (0..buckets)
        .map(|i| {
            let lo = (i * len / buckets).min(len - 1);
            let hi = ((i + 1) * len / buckets).clamp(lo + 1, len);
            let chunk = &pcm.samples[lo..hi];
            let min = chunk.iter().copied().min().unwrap_or(0);
            let max = chunk.iter().copied().max().unwrap_or(0);
            (min, max)
        })
        .collect()
}

/// The one place that loads a take's audio for playback and export: the WAV
/// with the take's trim (if any) applied.
pub fn load_take(lib: &Library, take: &Take, settings: &TrimSettings) -> Result<Pcm> {
    let pcm = audio::read_wav(&lib.audio_path(&take.audio_path))?;
    Ok(match take.trim_ms {
        Some((start, end)) => apply(&pcm, start, end, settings.fade_ms),
        None => pcm,
    })
}

/// Read a take's WAV and detect its trim (blocking; run it off the UI and
/// async threads).
pub fn detect_take(
    lib: &Library,
    take: &Take,
    settings: &TrimSettings,
) -> Result<Option<(u32, u32)>> {
    let pcm = audio::read_wav(&lib.audio_path(&take.audio_path))?;
    Ok(detect(&pcm, settings))
}

/// "0.42 s" for trim labels.
pub fn secs(ms: u32) -> String {
    format!("{:.2} s", ms as f64 / 1000.0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const RATE: u32 = 24000;

    /// `ms` of a 440 Hz-ish tone at roughly -12 dBFS.
    pub(crate) fn tone(ms: u32) -> Vec<i16> {
        let n = (RATE as u64 * ms as u64 / 1000) as usize;
        (0..n)
            .map(|i| ((i as f32 * 0.115).sin() * 8000.0) as i16)
            .collect()
    }

    pub(crate) fn silence(ms: u32) -> Vec<i16> {
        vec![0; (RATE as u64 * ms as u64 / 1000) as usize]
    }

    /// Low-level noise around -66 dBFS: below the default threshold.
    fn hiss(ms: u32) -> Vec<i16> {
        let n = (RATE as u64 * ms as u64 / 1000) as usize;
        (0..n).map(|i| if i % 2 == 0 { 16 } else { -16 }).collect()
    }

    pub(crate) fn pcm(parts: &[Vec<i16>]) -> Pcm {
        Pcm {
            rate: RATE,
            samples: parts.concat(),
        }
    }

    #[test]
    fn detects_speech_between_silences_with_margins() {
        let s = TrimSettings::default();
        let p = pcm(&[silence(300), tone(500), hiss(400)]);
        assert_eq!(duration_ms(&p), 1200);
        // Sound spans 300..800 ms; keep 40 ms before and 80 ms after.
        assert_eq!(detect(&p, &s), Some((260, 880)));

        let tight = TrimSettings {
            keep_head_ms: 0,
            keep_tail_ms: 0,
            ..s.clone()
        };
        assert_eq!(detect(&p, &tight), Some((300, 800)));
        // A threshold below the hiss keeps it as sound.
        let sensitive = TrimSettings {
            threshold_db: -80.0,
            ..s
        };
        assert_eq!(detect(&p, &sensitive), Some((260, 1200)));
    }

    #[test]
    fn all_silent_or_empty_takes_are_left_alone() {
        let s = TrimSettings::default();
        assert_eq!(detect(&pcm(&[silence(500)]), &s), None);
        assert_eq!(detect(&pcm(&[hiss(500)]), &s), None);
        assert_eq!(detect(&pcm(&[]), &s), None);
        assert_eq!(
            detect(
                &Pcm {
                    rate: 0,
                    samples: vec![1000; 10]
                },
                &s
            ),
            None
        );
    }

    #[test]
    fn tone_to_the_edges_keeps_everything() {
        let s = TrimSettings::default();
        let p = pcm(&[tone(250)]);
        assert_eq!(detect(&p, &s), Some((0, 250)));
        // Margins never reach outside the take.
        let p = pcm(&[silence(20), tone(100), silence(30)]);
        assert_eq!(detect(&p, &s), Some((0, 150)));
    }

    #[test]
    fn very_short_takes_never_invert() {
        let s = TrimSettings::default();
        // 5 ms: shorter than one frame.
        let p = pcm(&[tone(5)]);
        assert_eq!(detect(&p, &s), Some((0, 5)));
        // A single sample at an odd rate.
        let p = Pcm {
            rate: 22050,
            samples: vec![20000],
        };
        assert_eq!(detect(&p, &s), Some((0, 1)));
        let (a, b) = detect(&p, &s).unwrap();
        assert!(!apply(&p, a, b, 10).samples.is_empty());
    }

    #[test]
    fn apply_cuts_and_fades() {
        let p = pcm(&[silence(100), vec![10000; 4800], silence(100)]);
        // Keep 50..250 ms = 200 ms, with a 10 ms fade.
        let out = apply(&p, 50, 250, 10);
        assert_eq!(out.rate, RATE);
        assert_eq!(out.samples.len(), 4800);
        let fade = 240;
        // 50 ms of the leading silence are kept, then the tone starts.
        let first_loud = 50 * RATE as usize / 1000;
        assert!(out.samples[..first_loud].iter().all(|&x| x == 0));
        assert_eq!(out.samples[first_loud + 10], 10000);
        // The cut at 250 ms lands inside the tone: it fades out to zero.
        let k = out.samples.len();
        assert_eq!(out.samples[k - 1], 0);
        assert_eq!(out.samples[k - 1 - fade / 2], 5000);
        assert_eq!(out.samples[k - 1 - fade], 10000);

        // A fade over loud audio ramps linearly from zero.
        let loud = pcm(&[vec![10000; 2400]]);
        let out = apply(&loud, 0, 100, 10);
        assert_eq!(out.samples.len(), 2400);
        assert_eq!(out.samples[0], 0);
        assert_eq!(out.samples[120], 5000);
        assert_eq!(out.samples[1200], 10000);
        assert_eq!(*out.samples.last().unwrap(), 0);
        assert!(out.samples[..fade].windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn apply_handles_bad_ranges_and_long_fades() {
        let p = pcm(&[tone(100)]);
        // Inverted or out-of-range trims leave the take untouched.
        assert_eq!(apply(&p, 80, 20, 10), p);
        assert_eq!(apply(&p, 5000, 6000, 10), p);
        // End past the duration keeps the last sample.
        assert_eq!(apply(&p, 0, 9999, 0), p);
        // The fade is capped at half the kept length.
        let out = apply(&pcm(&[vec![1000; 48]]), 0, 2, 500);
        assert_eq!(out.samples.len(), 48);
        assert_eq!(out.samples[0], 0);
        assert_eq!(*out.samples.last().unwrap(), 0);
    }

    #[test]
    fn detect_then_apply_gives_even_edges() {
        let s = TrimSettings::default();
        let a = pcm(&[silence(400), tone(300), silence(100)]);
        let b = pcm(&[silence(50), tone(300), silence(700)]);
        let ta = detect(&a, &s).unwrap();
        let tb = detect(&b, &s).unwrap();
        let la = apply(&a, ta.0, ta.1, s.fade_ms).samples.len();
        let lb = apply(&b, tb.0, tb.1, s.fade_ms).samples.len();
        // Both become head margin + tone + tail margin.
        assert_eq!(la, lb);
        assert_eq!(la as u32, (40 + 300 + 80) * RATE / 1000);
    }

    #[test]
    fn peaks_cover_the_audio() {
        let p = pcm(&[silence(100), vec![5000, -7000], silence(100)]);
        let pk = peaks(&p, 10);
        assert_eq!(pk.len(), 10);
        assert_eq!(pk.iter().map(|x| x.0).min(), Some(-7000));
        assert_eq!(pk.iter().map(|x| x.1).max(), Some(5000));
        assert_eq!(pk[0], (0, 0));
        assert_eq!(pk[9], (0, 0));
        // More buckets than samples: every bucket still holds one sample.
        let tiny = Pcm {
            rate: RATE,
            samples: vec![1, -2, 3],
        };
        assert_eq!(
            peaks(&tiny, 6),
            vec![(1, 1), (1, 1), (-2, -2), (-2, -2), (3, 3), (3, 3)]
        );
        assert!(peaks(&pcm(&[]), 10).is_empty());
        assert!(peaks(&p, 0).is_empty());
    }
}
