//! Whisper's input: 16 kHz mono f32. Takes are 22.05–44.1 kHz 16-bit, so they
//! are low-pass filtered and decimated with a Blackman-windowed sinc (16 zero
//! crossings, cutoff 0.9 × the output Nyquist = 7.2 kHz), weights precomputed
//! per polyphase phase. The app's `audio::resample_linear` has no anti-alias
//! filter (a 10 kHz tone at 44.1 kHz comes through at -1.5 dB), which is fine
//! for its purpose but not for recognition.

use std::f64::consts::PI;

/// Whisper's sample rate.
pub const WHISPER_RATE: u32 = 16_000;

pub fn resample_poly(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if in_rate == out_rate || input.is_empty() || in_rate == 0 || out_rate == 0 {
        return input.to_vec();
    }
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let g = gcd(in_rate as u64, out_rate as u64);
    // Output sample i sits at input position i * m / l.
    let (m, l) = (in_rate as u64 / g, out_rate as u64 / g);
    let cutoff = (0.9 / (m as f64 / l as f64)).min(0.9);
    let hw = (16.0 / cutoff).ceil() as i64;
    let taps = (2 * hw) as usize;
    let blackman = |x: f64| {
        let t = (x + 1.0) * 0.5;
        0.42 - 0.5 * (2.0 * PI * t).cos() + 0.08 * (4.0 * PI * t).cos()
    };
    let sinc = |x: f64| {
        if x.abs() < 1e-12 {
            1.0
        } else {
            (PI * x).sin() / (PI * x)
        }
    };
    let mut table = vec![0f32; l as usize * taps];
    for p in 0..l as usize {
        let frac = p as f64 / l as f64;
        let row = &mut table[p * taps..(p + 1) * taps];
        let mut sum = 0.0;
        let mut vals = vec![0f64; taps];
        for (j, v) in vals.iter_mut().enumerate() {
            let d = frac - (j as i64 - hw + 1) as f64;
            let x = d / hw as f64;
            *v = if x.abs() >= 1.0 {
                0.0
            } else {
                cutoff * sinc(cutoff * d) * blackman(x)
            };
            sum += *v;
        }
        for (w, v) in row.iter_mut().zip(vals) {
            *w = (v / sum) as f32;
        }
    }
    let n = input.len() as i64;
    let out_len = (input.len() as u64 * l / m) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len as u64 {
        let center = (i * m / l) as i64;
        let row = &table[((i * m) % l) as usize * taps..][..taps];
        let k0 = center - hw + 1;
        let mut acc = 0f32;
        if k0 >= 0 && k0 + taps as i64 <= n {
            for (a, b) in input[k0 as usize..k0 as usize + taps].iter().zip(row) {
                acc += a * b;
            }
        } else {
            for (j, w) in row.iter().enumerate() {
                let k = k0 + j as i64;
                if (0..n).contains(&k) {
                    acc += input[k as usize] * w;
                }
            }
        }
        out.push(acc);
    }
    out
}

/// 16-bit mono PCM at any rate → 16 kHz f32 in [-1, 1].
pub fn to_whisper_input(samples: &[i16], rate: u32) -> Vec<f32> {
    let f: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
    resample_poly(&f, rate, WHISPER_RATE)
}

/// whisper.cpp returns no text for input under 100 ms. Short takes get a
/// 200 ms lead-in and are padded to 1.5 s (a heuristic from the research).
pub fn pad_for_whisper(mut pcm16k: Vec<f32>) -> Vec<f32> {
    let (lead, min_len) = (3_200, 24_000);
    if pcm16k.len() >= min_len {
        return pcm16k;
    }
    let mut v = vec![0.0f32; lead];
    v.append(&mut pcm16k);
    if v.len() < min_len {
        v.resize(min_len, 0.0);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        // Ignore the filter's edges.
        let x = &x[x.len() / 10..x.len() * 9 / 10];
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn keeps_speech_band_and_removes_aliases() {
        for rate in [44_100, 24_000, 22_050, 48_000] {
            let pass = resample_poly(&tone(rate, 1_000.0, 0.5), rate, WHISPER_RATE);
            assert_eq!(pass.len(), WHISPER_RATE as usize / 2, "{rate}");
            let gain = rms(&pass) / (0.5 / 2f32.sqrt());
            assert!((gain - 1.0).abs() < 0.01, "{rate}: 1 kHz gain {gain}");
            // 10 kHz is above the output Nyquist: it must not fold back.
            let alias = resample_poly(&tone(rate, 10_000.0, 0.5), rate, WHISPER_RATE);
            let db = 20.0 * (rms(&alias) / (0.5 / 2f32.sqrt())).log10();
            assert!(db < -60.0, "{rate}: 10 kHz at {db} dB");
        }
    }

    #[test]
    fn identity_dc_and_integer_input() {
        let x = vec![0.25f32; 100];
        assert_eq!(resample_poly(&x, 16_000, 16_000), x);
        assert!(resample_poly(&[], 44_100, 16_000).is_empty());
        let dc = resample_poly(&vec![0.25f32; 4_410], 44_100, 16_000);
        assert!(dc[200..1_400].iter().all(|v| (v - 0.25).abs() < 1e-3));
        let pcm = to_whisper_input(&[i16::MAX; 2_400], 24_000);
        assert_eq!(pcm.len(), 1_600);
        assert!((pcm[800] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn short_input_is_padded() {
        let p = pad_for_whisper(vec![0.5; 800]);
        assert_eq!(p.len(), 24_000);
        assert_eq!(
            (p[3_199], p[3_200], p[3_999], p[4_000]),
            (0.0, 0.5, 0.5, 0.0)
        );
        assert_eq!(pad_for_whisper(vec![0.1; 30_000]).len(), 30_000);
    }
}
