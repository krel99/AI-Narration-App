//! Per-language rules: which quote characters delimit speech, and which TTS
//! model a project in that language starts with.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotePair {
    pub open: char,
    pub close: char,
}

const fn q(open: char, close: char) -> QuotePair {
    QuotePair { open, close }
}

#[derive(Debug, Clone, Copy)]
pub struct Language {
    /// ISO 639-1 code, also sent to ElevenLabs as `language_code` where supported.
    pub code: &'static str,
    pub name: &'static str,
    /// Quote styles that mark dialogue, tried in order.
    pub quotes: &'static [QuotePair],
    pub default_tts_model: &'static str,
}

impl Language {
    /// The closing character for `c` if `c` opens a quote in this language.
    pub fn closing_for(&self, c: char) -> Option<char> {
        self.quotes.iter().find(|p| p.open == c).map(|p| p.close)
    }

    pub fn is_opening_quote(&self, c: char) -> bool {
        self.closing_for(c).is_some()
    }

    pub fn quote_example(&self) -> String {
        self.quotes
            .iter()
            .map(|p| format!("{}…{}", p.open, p.close))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

const STRAIGHT: QuotePair = q('"', '"');

pub const LANGUAGES: &[Language] = &[
    Language {
        code: "en",
        name: "English",
        quotes: &[q('“', '”'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "cs",
        name: "Czech",
        quotes: &[q('„', '“'), q('»', '«'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "sk",
        name: "Slovak",
        quotes: &[q('„', '“'), q('»', '«'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "de",
        name: "German",
        quotes: &[q('„', '“'), q('»', '«'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "pl",
        name: "Polish",
        quotes: &[q('„', '”'), q('«', '»'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "fr",
        name: "French",
        quotes: &[q('«', '»'), q('“', '”'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "it",
        name: "Italian",
        quotes: &[q('«', '»'), q('“', '”'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "es",
        name: "Spanish",
        quotes: &[q('«', '»'), q('“', '”'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "ru",
        name: "Russian",
        quotes: &[q('«', '»'), q('„', '“'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
    Language {
        code: "nl",
        name: "Dutch",
        quotes: &[q('“', '”'), q('„', '”'), STRAIGHT],
        default_tts_model: "eleven_v4",
    },
];

/// TTS models offered in project settings.
pub const TTS_MODELS: &[(&str, &str)] = &[
    ("eleven_v4", "Eleven v4 (most expressive, audio tags)"),
    ("eleven_v4_turbo", "Eleven v4 Turbo (faster, audio tags)"),
    ("eleven_v3", "Eleven v3 (audio tags)"),
    (
        "eleven_multilingual_v2",
        "Multilingual v2 (stable, no audio tags)",
    ),
    ("eleven_turbo_v2_5", "Turbo v2.5"),
    ("eleven_flash_v2_5", "Flash v2.5"),
];

/// ElevenLabs output formats offered in project settings. PCM is preferred;
/// 44.1 kHz PCM needs the Pro plan or higher.
pub const OUTPUT_FORMATS: &[(&str, &str)] = &[
    ("pcm_44100", "PCM 44.1 kHz (Pro plan and up)"),
    ("pcm_24000", "PCM 24 kHz"),
    ("pcm_22050", "PCM 22.05 kHz"),
    (
        "mp3_44100_192",
        "MP3 192 kbps fallback (decoded to PCM once on arrival)",
    ),
];

pub fn by_code(code: &str) -> &'static Language {
    LANGUAGES
        .iter()
        .find(|l| l.code == code)
        .unwrap_or(&LANGUAGES[0])
}

/// Whether the model accepts `language_code` to force the language.
pub fn model_accepts_language_code(model: &str) -> bool {
    model.contains("v2_5") || model.contains("v3") || model.contains("v4")
}

/// Whether the model performs audio tags like `[whispers]` instead of reading them.
pub fn model_supports_tags(model: &str) -> bool {
    model.starts_with("eleven_v3") || model.starts_with("eleven_v4")
}

/// Whether the model can generate a multi-speaker paragraph through
/// ElevenLabs' HTTP Text-to-Dialogue endpoint. Eleven v3 and v4 are
/// documented for it; v4 Turbo is documented only for the dialogue
/// WebSocket, so it keeps per-segment requests.
pub fn model_supports_dialogue(model: &str) -> bool {
    matches!(model, "eleven_v3" | "eleven_v4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_code_falls_back_to_english() {
        assert_eq!(by_code("xx").code, "en");
        assert_eq!(by_code("cs").name, "Czech");
    }

    #[test]
    fn tag_capable_models() {
        assert!(model_supports_tags("eleven_v4"));
        assert!(model_supports_tags("eleven_v4_turbo"));
        assert!(model_supports_tags("eleven_v3"));
        assert!(!model_supports_tags("eleven_multilingual_v2"));
        assert!(!model_supports_tags("eleven_flash_v2_5"));
    }

    #[test]
    fn dialogue_capable_models() {
        assert!(model_supports_dialogue("eleven_v4"));
        assert!(model_supports_dialogue("eleven_v3"));
        assert!(!model_supports_dialogue("eleven_v4_turbo"));
        assert!(!model_supports_dialogue("eleven_multilingual_v2"));
        assert!(!model_supports_dialogue("eleven_flash_v2_5"));
    }

    #[test]
    fn closing_quotes() {
        let cs = by_code("cs");
        assert_eq!(cs.closing_for('„'), Some('“'));
        assert_eq!(cs.closing_for('»'), Some('«'));
        assert_eq!(cs.closing_for('“'), None);
        let fr = by_code("fr");
        assert_eq!(fr.closing_for('«'), Some('»'));
    }
}
