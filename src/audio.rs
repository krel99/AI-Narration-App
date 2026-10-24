//! Lossless audio handling. Everything is mono 16-bit PCM in memory and WAV on
//! disk; the only lossy encode happens once, in the ffmpeg export pass.

use std::f64::consts::PI;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub rate: u32,
    pub samples: Vec<i16>,
}

impl Pcm {
    pub fn append_silence(&mut self, ms: u32) {
        let n = self.rate as u64 * ms as u64 / 1000;
        self.samples.extend(std::iter::repeat_n(0i16, n as usize));
    }

    pub fn to_f32(&self) -> Vec<f32> {
        self.samples.iter().map(|&s| s as f32 / 32768.0).collect()
    }
}

/// Sample rate of an ElevenLabs PCM output format (`pcm_44100` → 44100).
pub fn pcm_format_rate(output_format: &str) -> Option<u32> {
    output_format.strip_prefix("pcm_")?.parse().ok()
}

/// Raw little-endian signed 16-bit mono PCM, as ElevenLabs returns it.
pub fn from_pcm_bytes(bytes: &[u8], rate: u32) -> Pcm {
    let samples = bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    Pcm { rate, samples }
}

fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Decode an MP3 response once on arrival (fallback path when the plan lacks PCM).
pub fn decode_mp3(bytes: Vec<u8>) -> Result<Pcm> {
    use rodio::Source;
    let decoder = rodio::Decoder::new_mp3(Cursor::new(bytes)).context("decoding MP3")?;
    let channels = decoder.channels().get() as usize;
    let rate = decoder.sample_rate().get();
    let interleaved: Vec<f32> = decoder.collect();
    let samples = interleaved
        .chunks(channels.max(1))
        .map(|frame| f32_to_i16(frame.iter().sum::<f32>() / frame.len() as f32))
        .collect();
    Ok(Pcm { rate, samples })
}

/// Write mono 16-bit WAV (raw PCM plus a 44-byte header) atomically.
pub fn write_wav(path: &Path, pcm: &Pcm) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("wav.part");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: pcm.rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    {
        let mut w = hound::WavWriter::create(&tmp, spec)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let mut w16 = w.get_i16_writer(pcm.samples.len() as u32);
        for &s in &pcm.samples {
            w16.write_sample(s);
        }
        w16.flush()?;
        w.finalize()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read_wav(path: &Path) -> Result<Pcm> {
    let mut r =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!("{}: expected 16-bit integer WAV", path.display());
    }
    let channels = spec.channels.max(1) as usize;
    let interleaved: Vec<i16> = r.samples::<i16>().collect::<Result<_, _>>()?;
    let samples = if channels == 1 {
        interleaved
    } else {
        interleaved
            .chunks(channels)
            .map(|f| (f.iter().map(|&s| s as i32).sum::<i32>() / f.len() as i32) as i16)
            .collect()
    };
    Ok(Pcm {
        rate: spec.sample_rate,
        samples,
    })
}

/// Band-limited resampler, for takes of one chapter generated at different
/// rates (after changing the output format). Nothing above the lower
/// Nyquist folds back into the audible band (the sibilance of a 44.1 kHz
/// take resampled to 24 kHz), and upsampling adds no images.
pub fn resample(pcm: &Pcm, to_rate: u32) -> Pcm {
    if pcm.rate == to_rate || pcm.samples.is_empty() {
        return Pcm {
            rate: to_rate,
            samples: pcm.samples.clone(),
        };
    }
    let input: Vec<f32> = pcm.samples.iter().map(|&s| s as f32).collect();
    let samples = resample_poly(&input, pcm.rate, to_rate)
        .into_iter()
        .map(|v| v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
        .collect();
    Pcm {
        rate: to_rate,
        samples,
    }
}

/// Polyphase resampling with a Blackman-windowed sinc low-pass (16 zero
/// crossings, cutoff 0.9 × the lower Nyquist), weights precomputed per
/// phase. Zero-phase: output sample i sits at input position i × in / out.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_bytes_roundtrip_through_wav() {
        let bytes: Vec<u8> = [0i16, 1000, -1000, i16::MAX, i16::MIN]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let pcm = from_pcm_bytes(&bytes, 44100);
        assert_eq!(pcm.samples, vec![0, 1000, -1000, i16::MAX, i16::MIN]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b.wav");
        write_wav(&path, &pcm).unwrap();
        // 44-byte header + 2 bytes per sample.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 44 + 10);
        assert_eq!(read_wav(&path).unwrap(), pcm);
    }

    #[test]
    fn silence_and_resample() {
        let mut pcm = Pcm {
            rate: 1000,
            samples: vec![],
        };
        pcm.append_silence(250);
        assert_eq!(pcm.samples.len(), 250);
        let same = Pcm {
            rate: 1000,
            samples: vec![0, 100, 200],
        };
        assert_eq!(resample(&same, 1000), same);
    }

    /// Level in dB of the `hz` component of `pcm` (relative to full scale).
    fn level_db(pcm: &Pcm, hz: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * hz / pcm.rate as f64;
        // Skip the edges, where the filter sees zeros.
        let x = &pcm.samples[pcm.samples.len() / 10..pcm.samples.len() * 9 / 10];
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &s) in x.iter().enumerate() {
            re += s as f64 * (w * i as f64).cos();
            im += s as f64 * (w * i as f64).sin();
        }
        let amp = 2.0 * (re * re + im * im).sqrt() / x.len() as f64;
        20.0 * (amp / 32768.0).max(1e-12).log10()
    }

    fn tone(rate: u32, parts: &[(f64, f64)]) -> Pcm {
        Pcm {
            rate,
            samples: (0..rate as usize / 2)
                .map(|i| {
                    let t = i as f64 / rate as f64;
                    parts
                        .iter()
                        .map(|(hz, amp)| {
                            amp * 32767.0 * (2.0 * std::f64::consts::PI * hz * t).sin()
                        })
                        .sum::<f64>() as i16
                })
                .collect(),
        }
    }

    #[test]
    fn resampling_does_not_alias() {
        // A 44.1 kHz take in a 24 kHz chapter: speech stays, the 14 kHz
        // sibilance band is filtered instead of folding back to 10 kHz.
        let take = tone(44_100, &[(1000.0, 0.25), (14_000.0, 0.25)]);
        let down = resample(&take, 24_000);
        assert_eq!(down.rate, 24_000);
        assert!((down.samples.len() as i64 - 12_000).abs() <= 1);
        assert!((level_db(&down, 1000.0) - level_db(&take, 1000.0)).abs() < 0.5);
        assert!(
            level_db(&down, 10_000.0) < -80.0,
            "{}",
            level_db(&down, 10_000.0)
        );
        // A 24 kHz take in a 44.1 kHz chapter: no image at 15 kHz.
        let take = tone(24_000, &[(9000.0, 0.5)]);
        let up = resample(&take, 44_100);
        assert!((level_db(&up, 9000.0) - level_db(&take, 9000.0)).abs() < 0.5);
        assert!(
            level_db(&up, 15_000.0) < -70.0,
            "{}",
            level_db(&up, 15_000.0)
        );
        // Loud input does not wrap around.
        let loud = Pcm {
            rate: 44_100,
            samples: (0..4410)
                .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
                .collect(),
        };
        assert_eq!(resample(&loud, 22_050).samples.len(), 2205);
    }

    #[test]
    fn format_rates() {
        assert_eq!(pcm_format_rate("pcm_44100"), Some(44100));
        assert_eq!(pcm_format_rate("pcm_24000"), Some(24000));
        assert_eq!(pcm_format_rate("mp3_44100_192"), None);
    }
}
