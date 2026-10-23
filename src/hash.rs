//! Take hash: identical inputs map to the same audio file, so a take is reused
//! instead of paying for the same characters twice.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::model::VoiceSettings;

#[derive(Serialize)]
struct TakeInputs<'a> {
    text: &'a str,
    voice_id: &'a str,
    settings: &'a VoiceSettings,
    model: &'a str,
    seed: u32,
    output_format: &'a str,
}

/// hash(text + voice_id + settings + model + seed), plus the output format so a
/// PCM take never collides with an MP3-fallback take.
pub fn take_hash(
    text: &str,
    voice_id: &str,
    settings: &VoiceSettings,
    model: &str,
    seed: u32,
    output_format: &str,
) -> String {
    let inputs = TakeInputs {
        text,
        voice_id,
        settings,
        model,
        seed,
        output_format,
    };
    let json = serde_json::to_vec(&inputs).expect("take inputs serialize");
    hex::encode(Sha256::digest(&json))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_sensitive_to_every_input() {
        let s = VoiceSettings::default();
        let base = take_hash("Hi", "v1", &s, "m", 1, "pcm_44100");
        assert_eq!(base, take_hash("Hi", "v1", &s, "m", 1, "pcm_44100"));
        assert_eq!(base.len(), 64);
        assert_ne!(base, take_hash("Hi!", "v1", &s, "m", 1, "pcm_44100"));
        assert_ne!(base, take_hash("Hi", "v2", &s, "m", 1, "pcm_44100"));
        assert_ne!(base, take_hash("Hi", "v1", &s, "m2", 1, "pcm_44100"));
        assert_ne!(base, take_hash("Hi", "v1", &s, "m", 2, "pcm_44100"));
        assert_ne!(base, take_hash("Hi", "v1", &s, "m", 1, "mp3_44100_192"));
        let s2 = VoiceSettings {
            stability: 0.3,
            ..s.clone()
        };
        assert_ne!(base, take_hash("Hi", "v1", &s2, "m", 1, "pcm_44100"));
    }
}
