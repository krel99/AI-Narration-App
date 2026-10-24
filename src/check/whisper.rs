//! whisper-rs 0.16 backend (feature `whisper`). One context plus one state,
//! used from the take checker's single thread.
//!
//! From the verified research: `FullParams::set_abort_callback_safe` is
//! type-confused in whisper-rs 0.13.2–0.16.0 (its closure never runs), so the
//! raw callback is used with our own trampoline. whisper.cpp polls it only
//! after each whole encoder pass and decoder call, so a flag that is already
//! set is checked before `full()`, and an abort returns `GenericError(-6)`,
//! `(-8)` or `(-9)`.

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState,
};

use super::resample::WHISPER_RATE;
use super::{AsrError, HeardSegment, Transcriber, Transcript};

pub struct Whisper {
    // `state` holds an Arc to the context's inner data; keep both.
    state: WhisperState,
    ctx: WhisperContext,
}

unsafe extern "C" fn abort_trampoline(user_data: *mut c_void) -> bool {
    // SAFETY: `user_data` is the `&AtomicBool` passed to `transcribe`, which
    // outlives the `full()` call that invokes this.
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    flag.load(Ordering::Relaxed)
}

impl Whisper {
    pub fn load(model: &Path) -> anyhow::Result<Self> {
        whisper_rs::install_logging_hooks();
        let mut cp = WhisperContextParameters::default();
        cp.use_gpu(false);
        let ctx = WhisperContext::new_with_params(model, cp)
            .with_context(|| format!("loading Whisper model {}", model.display()))?;
        let state = ctx.create_state().context("creating the Whisper state")?;
        Ok(Self { state, ctx })
    }
}

impl Transcriber for Whisper {
    fn is_multilingual(&self) -> bool {
        self.ctx.is_multilingual()
    }

    fn transcribe(
        &mut self,
        pcm16k: &[f32],
        lang: &str,
        threads: usize,
        cancel: &AtomicBool,
    ) -> Result<Transcript, AsrError> {
        // A flag that is already set would still cost a full encoder pass.
        if cancel.load(Ordering::Relaxed) {
            return Err(AsrError::Cancelled);
        }
        if pcm16k.len() < WHISPER_RATE as usize / 10 {
            return Err(anyhow!("audio shorter than 100 ms").into());
        }
        let secs = pcm16k.len() as f32 / WHISPER_RATE as f32;
        let one_window = single_window(secs, lang);
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(threads.clamp(1, 64) as i32);
        // The chapter's language, never auto-detect.
        p.set_language(Some(lang));
        p.set_translate(false);
        p.set_detect_language(false);
        p.set_no_context(true);
        p.set_no_timestamps(one_window);
        p.set_single_segment(one_window);
        // Bound runaway decoding (a hallucination loop on silence, or noise)
        // at about 15 tokens per second of audio. Whisper's tokenizer needs
        // about 0.2 tokens per character for English but about 0.48 for
        // Czech and Slovak (measured), i.e. 7-9 tokens/s at narration pace:
        // a tighter cap would cut real transcripts short and flag them.
        p.set_max_tokens(max_tokens(secs));
        p.set_suppress_blank(true);
        // Blanks exact "(", "[", ... tokens only; annotations are still
        // stripped from the transcript.
        p.set_suppress_nst(true);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        // No initial prompt: the expected text would bias Whisper toward the
        // script and hide exactly the errors being checked.
        // SAFETY: the trampoline only reads the AtomicBool behind the user
        // data pointer, and `cancel` outlives `full()`.
        unsafe {
            p.set_abort_callback(Some(abort_trampoline));
            p.set_abort_callback_user_data(cancel as *const AtomicBool as *mut c_void);
        }
        match self.state.full(p, pcm16k) {
            Ok(()) => {}
            Err(WhisperError::GenericError(-6 | -8 | -9)) if cancel.load(Ordering::Relaxed) => {
                return Err(AsrError::Cancelled);
            }
            Err(e) => return Err(anyhow!("Whisper failed: {e:?}").into()),
        }
        let mut segments = Vec::new();
        for seg in self.state.as_iter() {
            segments.push(HeardSegment {
                text: seg
                    .to_str_lossy()
                    .map_err(|e| anyhow!("Whisper text: {e:?}"))?
                    .into_owned(),
                no_speech_prob: seg.no_speech_probability(),
            });
        }
        Ok(Transcript { segments })
    }
}

/// Decode the take as one segment without timestamps? whisper.cpp ends a
/// window's decoding after 220 tokens, and without timestamps it cannot
/// resume after them, so the rest of the take would be lost. English needs
/// about 0.2 tokens per character, so 28 s is safe; Czech and Slovak about
/// 0.48 (about 9 a second when spoken fast), so longer takes there use
/// timestamps, which let whisper.cpp continue where it stopped.
fn single_window(secs: f32, lang: &str) -> bool {
    secs <= if lang == "en" { 28.0 } else { 20.0 }
}

/// Token cap per segment for `secs` of audio (a 30 s window holds at most
/// 224 text tokens anyway).
fn max_tokens(secs: f32) -> i32 {
    (16.0 + 15.0 * secs).min(224.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_cap_scales_with_the_audio() {
        assert_eq!(max_tokens(1.5), 38);
        assert_eq!(max_tokens(8.0), 136);
        assert_eq!(max_tokens(60.0), 224);
        // Czech at a fast 18 characters a second, about 0.48 tokens per
        // character (measured with Whisper's tokenizer), with a margin, for
        // every take decoded as a single segment.
        for secs in [1.5f32, 4.0, 10.0, 15.0, 20.0] {
            assert!(single_window(secs, "cs"));
            let needed = secs * 18.0 * 0.48;
            assert!(max_tokens(secs) as f32 > needed * 1.25, "{secs} s");
            // ...and within whisper.cpp's own 220 tokens per window.
            assert!(needed * 1.25 < 220.0, "{secs} s");
        }
        assert!(!single_window(24.0, "cs"));
        assert!(single_window(24.0, "en") && !single_window(29.0, "en"));
    }

    /// Runs only with `NS_WHISPER_MODEL=<ggml model>`. With `NS_WHISPER_WAV`
    /// (a 16-bit WAV) and `NS_WHISPER_TEXT` it also prints the comparison.
    #[test]
    #[ignore]
    fn real_whisper_model() {
        let Some(model) = std::env::var_os("NS_WHISPER_MODEL") else {
            eprintln!("NS_WHISPER_MODEL not set; skipped");
            return;
        };
        let lang = std::env::var("NS_WHISPER_LANG").unwrap_or_else(|_| "en".into());
        let mut w = Whisper::load(Path::new(&model)).unwrap();
        let threads = super::super::default_threads();
        let never = AtomicBool::new(false);
        let pcm = match std::env::var_os("NS_WHISPER_WAV") {
            Some(wav) => {
                let pcm = crate::audio::read_wav(Path::new(&wav)).unwrap();
                super::super::resample::to_whisper_input(&pcm.samples, pcm.rate)
            }
            None => (0..2 * WHISPER_RATE as usize)
                .map(|i| 0.1 * (i as f32 * 0.0866).sin())
                .collect(),
        };
        let pcm = super::super::resample::pad_for_whisper(pcm);
        let t = std::time::Instant::now();
        let out = w.transcribe(&pcm, &lang, threads, &never).unwrap();
        eprintln!(
            "multilingual={} {:.1} s audio in {:.2?}: {:?}",
            w.is_multilingual(),
            pcm.len() as f32 / WHISPER_RATE as f32,
            t.elapsed(),
            out.segments
        );
        if let Ok(text) = std::env::var("NS_WHISPER_TEXT") {
            let l = super::super::text::Lang::from_code(&lang);
            let c = super::super::text::check(
                &text,
                &out.text(),
                l,
                &[],
                &super::super::text::Thresholds::default(),
            );
            eprintln!(
                "errors={} wer={:.3} verdict={:?}",
                c.errors, c.wer, c.verdict
            );
        }
        // A cancel flag that is already set never starts the encoder.
        let stop = AtomicBool::new(true);
        assert!(matches!(
            w.transcribe(&pcm, &lang, threads, &stop),
            Err(AsrError::Cancelled)
        ));
        // Cancelling during a long transcription returns promptly-ish.
        let long: Vec<f32> = (0..60 * WHISPER_RATE as usize)
            .map(|i| 0.1 * (i as f32 * 0.0866).sin())
            .collect();
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            f2.store(true, Ordering::Relaxed);
        });
        let r = w.transcribe(&long, &lang, threads, &flag);
        killer.join().unwrap();
        assert!(matches!(r, Err(AsrError::Cancelled)), "{:?}", r.err());
    }
}
