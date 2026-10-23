//! Lossless audio handling. Everything is mono 16-bit PCM in memory and WAV on
//! disk; the only lossy encode happens once, in the ffmpeg export pass.

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

/// Linear-interpolation resampler. Only used when takes of one chapter were
/// generated at different rates (e.g. after changing the output format).
pub fn resample_linear(pcm: &Pcm, to_rate: u32) -> Pcm {
    if pcm.rate == to_rate || pcm.samples.is_empty() {
        return Pcm {
            rate: to_rate,
            samples: pcm.samples.clone(),
        };
    }
    let ratio = pcm.rate as f64 / to_rate as f64;
    let out_len = (pcm.samples.len() as f64 / ratio).round() as usize;
    let last = pcm.samples.len() - 1;
    let samples = (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let i0 = (pos.floor() as usize).min(last);
            let i1 = (i0 + 1).min(last);
            let frac = pos - i0 as f64;
            let v = pcm.samples[i0] as f64 * (1.0 - frac) + pcm.samples[i1] as f64 * frac;
            v.round() as i16
        })
        .collect();
    Pcm {
        rate: to_rate,
        samples,
    }
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
        let up = resample_linear(
            &Pcm {
                rate: 2000,
                samples: vec![0, 100, 200, 300],
            },
            1000,
        );
        assert_eq!(up.samples, vec![0, 200]);
        assert_eq!(resample_linear(&up, 1000), up);
    }

    #[test]
    fn format_rates() {
        assert_eq!(pcm_format_rate("pcm_44100"), Some(44100));
        assert_eq!(pcm_format_rate("pcm_24000"), Some(24000));
        assert_eq!(pcm_format_rate("mp3_44100_192"), None);
    }
}
