//! Picking from the user's own voices: search and filters for the voice
//! picker, which speakers already use a voice, and the on-disk cache of
//! voice previews.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::audio::{self, Pcm};
use crate::lang;
use crate::tts::VoiceInfo;

/// The filters the picker offers, built from the values in the loaded list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facet {
    Category,
    Gender,
    Age,
    Accent,
    Language,
}

impl Facet {
    pub const ALL: [Facet; 5] = [
        Facet::Category,
        Facet::Gender,
        Facet::Age,
        Facet::Accent,
        Facet::Language,
    ];

    fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Facet::Category => "Category",
            Facet::Gender => "Gender",
            Facet::Age => "Age",
            Facet::Accent => "Accent",
            Facet::Language => "Language",
        }
    }

    /// This facet's values on a voice, normalised (lowercase; languages as
    /// codes where known).
    pub fn values(self, v: &VoiceInfo) -> Vec<String> {
        let label = |k: &str| v.labels.get(k).map(|s| norm(s)).into_iter().collect();
        match self {
            Facet::Category => v.category.as_deref().map(norm).into_iter().collect(),
            Facet::Gender => label("gender"),
            Facet::Age => label("age"),
            Facet::Accent => {
                let mut out: Vec<String> = label("accent");
                for l in &v.verified_languages {
                    if let Some(a) = &l.accent {
                        push_unique(&mut out, norm(a));
                    }
                }
                out
            }
            Facet::Language => {
                let mut out = Vec::new();
                if let Some(l) = v.labels.get("language") {
                    push_unique(&mut out, language_key(l));
                }
                for l in &v.verified_languages {
                    push_unique(&mut out, language_key(&l.language));
                }
                out
            }
        }
    }
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !value.is_empty() && !out.contains(&value) {
        out.push(value);
    }
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// A language label or verified language as a comparable key: a known
/// name or code becomes the code, a locale loses its region
/// ("en-US" → "en").
pub fn language_key(raw: &str) -> String {
    let s = norm(raw);
    if let Some(l) = lang::LANGUAGES
        .iter()
        .find(|l| l.code == s || l.name.to_lowercase() == s)
    {
        return l.code.to_string();
    }
    match s.split(['-', '_']).next() {
        Some(code) if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) => {
            code.to_string()
        }
        _ => s,
    }
}

/// How a facet value is shown: languages by name where known, others as
/// words ("high_quality" → "high quality").
pub fn facet_display(facet: Facet, value: &str) -> String {
    if facet == Facet::Language
        && let Some(l) = lang::LANGUAGES.iter().find(|l| l.code == value)
    {
        return l.name.to_string();
    }
    value.replace('_', " ")
}

/// The search text and the chosen facet values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub query: String,
    facets: [Option<String>; 5],
}

impl Filter {
    pub fn get(&self, f: Facet) -> Option<&str> {
        self.facets[f.index()].as_deref()
    }

    pub fn set(&mut self, f: Facet, value: Option<String>) {
        self.facets[f.index()] = value;
    }

    /// Whether anything narrows the list.
    pub fn is_active(&self) -> bool {
        !self.query.trim().is_empty() || self.facets.iter().any(Option::is_some)
    }

    pub fn clear(&mut self) {
        *self = Filter::default();
    }
}

/// Lowercase words (letters and digits; `_`, `-` and punctuation split).
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

/// A word and, when it runs words together ("DeepCalm", "Studio2024"), its
/// parts as well, so "calm" and "2024" find them too.
fn with_parts(word: &str, out: &mut Vec<String>) {
    out.push(word.to_lowercase());
    let chars: Vec<char> = word.chars().collect();
    let mut start = 0;
    for i in 1..chars.len() {
        let (a, b) = (chars[i - 1], chars[i]);
        let boundary = (a.is_lowercase() && b.is_uppercase())
            || (a.is_alphabetic() && b.is_numeric())
            || (a.is_numeric() && b.is_alphabetic());
        if boundary {
            out.push(chars[start..i].iter().collect::<String>().to_lowercase());
            start = i;
        }
    }
    if start > 0 {
        out.push(chars[start..].iter().collect::<String>().to_lowercase());
    }
}

/// The words the search box looks at: name, category, description, label
/// values and language names, plus the voice id as a whole (a pasted id
/// finds its voice).
fn haystack(v: &VoiceInfo) -> Vec<String> {
    let mut parts: Vec<String> = vec![v.name.clone()];
    parts.extend(v.category.clone());
    parts.extend(v.description.clone());
    parts.extend(v.labels.values().cloned());
    for code in Facet::Language.values(v) {
        parts.push(facet_display(Facet::Language, &code));
    }
    let mut out = vec![v.voice_id.to_lowercase()];
    for p in &parts {
        for w in p
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            with_parts(w, &mut out);
        }
    }
    out
}

/// Whether a voice passes the filter. Every word of the query must begin a
/// word (case-insensitively) of the name, labels, description or category,
/// so "brit" finds "British" and "male" does not find "female" (the parts of
/// a run-together name count as words: "calm" finds "DeepCalm"); every
/// chosen facet value must be one of the voice's.
pub fn matches(v: &VoiceInfo, filter: &Filter) -> bool {
    for f in Facet::ALL {
        if let Some(want) = filter.get(f)
            && !f.values(v).iter().any(|have| have == want)
        {
            return false;
        }
    }
    let query: Vec<String> = words(&filter.query).collect();
    if query.is_empty() {
        return true;
    }
    let hay = haystack(v);
    query
        .iter()
        .all(|q| hay.iter().any(|word| word.starts_with(q.as_str())))
}

/// Indices of the voices that pass, in list order.
pub fn filtered(voices: &[VoiceInfo], filter: &Filter) -> Vec<usize> {
    (0..voices.len())
        .filter(|&i| matches(&voices[i], filter))
        .collect()
}

/// The values of `facet` that occur in the list, sorted as shown.
pub fn facet_options(voices: &[VoiceInfo], facet: Facet) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in voices {
        for value in facet.values(v) {
            push_unique(&mut out, value);
        }
    }
    out.sort_by_key(|v| facet_display(facet, v).to_lowercase());
    out
}

/// The speakers (other than `except`) whose voice is `voice_id`.
pub fn used_by<'a>(
    assignments: impl IntoIterator<Item = (&'a str, &'a str)>,
    voice_id: &str,
    except: &str,
) -> Vec<String> {
    let id = voice_id.trim();
    if id.is_empty() {
        return vec![];
    }
    assignments
        .into_iter()
        .filter(|(key, voice)| *key != except && voice.trim() == id)
        .map(|(key, _)| key.to_string())
        .collect()
}

// ---- previews -----------------------------------------------------------

/// Where a voice's preview audio can come from.
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewSource {
    /// A public preview file.
    Url(String),
    /// The sample the user uploaded (fetched with the API key).
    Sample {
        sample_id: String,
        mime_type: Option<String>,
    },
}

impl PreviewSource {
    fn cache_key(&self) -> String {
        match self {
            PreviewSource::Url(u) => u.clone(),
            PreviewSource::Sample { sample_id, .. } => format!("sample:{sample_id}"),
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            PreviewSource::Sample {
                mime_type: Some(m), ..
            } if m.contains("wav") => "wav",
            _ => "mp3",
        }
    }
}

/// The preview sources of a voice, best first: its preview, then a
/// verified-language preview (the project's language first), then its
/// uploaded sample. Empty when there is nothing to play.
pub fn preview_sources(v: &VoiceInfo, language: &str) -> Vec<PreviewSource> {
    let mut out = Vec::new();
    let mut add = |u: &str| {
        let s = PreviewSource::Url(u.to_string());
        if !out.contains(&s) {
            out.push(s);
        }
    };
    if let Some(u) = &v.preview_url {
        add(u);
    }
    let lang = language_key(language);
    let (same, other): (Vec<_>, Vec<_>) = v
        .verified_languages
        .iter()
        .partition(|l| !lang.is_empty() && language_key(&l.language) == lang);
    for l in same.into_iter().chain(other) {
        if let Some(u) = &l.preview_url {
            add(u);
        }
    }
    if let Some(s) = &v.sample {
        out.push(PreviewSource::Sample {
            sample_id: s.sample_id.clone(),
            mime_type: s.mime_type.clone(),
        });
    }
    out
}

/// `<library>/voice-previews/`.
pub fn previews_dir(library_root: &Path) -> PathBuf {
    library_root.join("voice-previews")
}

/// Voice ids are plain identifiers, but a file name must never escape the
/// cache directory.
fn safe_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// `<dir>/<voice_id>-<hash of the source>.mp3`: keyed by the preview URL,
/// so a voice whose preview changes is fetched again.
pub fn cache_file(dir: &Path, voice_id: &str, source: &PreviewSource) -> PathBuf {
    let digest = Sha256::digest(source.cache_key().as_bytes());
    let tag = hex::encode(&digest[..5]);
    dir.join(format!(
        "{}-{tag}.{}",
        safe_name(voice_id),
        source.extension()
    ))
}

/// A cached preview, decoded. A file that no longer decodes is removed so
/// it is fetched again.
pub fn read_cached(path: &Path) -> Option<Pcm> {
    let bytes = std::fs::read(path).ok()?;
    match decode_preview(&bytes) {
        Ok(pcm) => Some(pcm),
        Err(e) => {
            log::warn!("dropping cached preview {}: {e:#}", path.display());
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

/// Store a downloaded preview atomically and drop older previews of the
/// same voice (from a previous preview URL).
pub fn store(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("preview path has no directory")?;
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("preview file name")?
        .to_string();
    let tmp = dir.join(format!(".{name}.{:08x}.part", rand::random::<u32>()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    if let Some((voice, _)) = name.rsplit_once('-') {
        for entry in std::fs::read_dir(dir)?.flatten() {
            let other = entry.file_name().to_string_lossy().to_string();
            if other != name && is_cache_name_of(&other, voice) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// Whether `file` is `<voice>-<10 hex>.mp3|wav`.
fn is_cache_name_of(file: &str, voice: &str) -> bool {
    let Some(rest) = file.strip_prefix(voice).and_then(|r| r.strip_prefix('-')) else {
        return false;
    };
    let Some((tag, ext)) = rest.split_once('.') else {
        return false;
    };
    tag.len() == 10 && tag.chars().all(|c| c.is_ascii_hexdigit()) && matches!(ext, "mp3" | "wav")
}

/// Decode preview audio: WAV (an uploaded sample) by its header, anything
/// else as MP3. The server's content type is not trusted (preview files are
/// served as text/plain).
pub fn decode_preview(bytes: &[u8]) -> Result<Pcm> {
    let pcm = if bytes.starts_with(b"RIFF") {
        decode_wav(bytes)?
    } else {
        audio::decode_mp3(bytes.to_vec())?
    };
    if pcm.rate == 0 || pcm.samples.is_empty() {
        bail!("the preview has no audio");
    }
    Ok(pcm)
}

fn decode_wav(bytes: &[u8]) -> Result<Pcm> {
    let mut r = hound::WavReader::new(Cursor::new(bytes)).context("reading WAV")?;
    let spec = r.spec();
    let channels = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    let samples = interleaved
        .chunks(channels)
        .map(|f| {
            let mean = f.iter().sum::<f32>() / f.len() as f32;
            (mean.clamp(-1.0, 1.0) * 32767.0).round() as i16
        })
        .collect();
    Ok(Pcm {
        rate: spec.sample_rate,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tts::voices::{VerifiedLanguage, VoiceSample};

    fn voice(id: &str, name: &str, category: &str, labels: &[(&str, &str)]) -> VoiceInfo {
        VoiceInfo {
            voice_id: id.into(),
            name: name.into(),
            category: Some(category.into()),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn list() -> Vec<VoiceInfo> {
        let mut vale = voice(
            "v1",
            "Vale",
            "cloned",
            &[
                ("gender", "female"),
                ("age", "young"),
                ("accent", "British"),
            ],
        );
        vale.description = Some("Warm, low narration voice".into());
        let mut voss = voice(
            "v2",
            "Commander Voss",
            "generated",
            &[
                ("gender", "male"),
                ("age", "middle_aged"),
                ("accent", "american"),
                ("use_case", "social_media"),
            ],
        );
        voss.verified_languages = vec![VerifiedLanguage {
            language: "cs".into(),
            accent: Some("standard".into()),
            ..Default::default()
        }];
        let lib = voice(
            "v3",
            "Old Archivist",
            "professional",
            &[("gender", "male"), ("language", "English")],
        );
        vec![vale, voss, lib]
    }

    fn names(voices: &[VoiceInfo], f: &Filter) -> Vec<String> {
        filtered(voices, f)
            .into_iter()
            .map(|i| voices[i].name.clone())
            .collect()
    }

    #[test]
    fn search_is_case_insensitive_over_name_labels_description_category() {
        let voices = list();
        let mut f = Filter::default();
        assert_eq!(names(&voices, &f).len(), 3);
        f.query = "VOSS".into();
        assert_eq!(names(&voices, &f), ["Commander Voss"]);
        f.query = "british".into();
        assert_eq!(names(&voices, &f), ["Vale"], "labels");
        f.query = "narration warm".into();
        assert_eq!(names(&voices, &f), ["Vale"], "description, every word");
        f.query = "professional".into();
        assert_eq!(names(&voices, &f), ["Old Archivist"], "category");
        f.query = "social media".into();
        assert_eq!(
            names(&voices, &f),
            ["Commander Voss"],
            "underscores as spaces"
        );
        f.query = "czech".into();
        assert_eq!(names(&voices, &f), ["Commander Voss"], "verified languages");
        f.query = "male".into();
        assert_eq!(
            names(&voices, &f),
            ["Commander Voss", "Old Archivist"],
            "word starts, so not \"female\""
        );
        f.query = "brit narr".into();
        assert_eq!(names(&voices, &f), ["Vale"], "prefixes");
        f.query = "male young".into();
        assert!(names(&voices, &f).is_empty());
        f.query = "middle-aged".into();
        assert_eq!(names(&voices, &f), ["Commander Voss"]);
        f.query = "  ".into();
        assert!(!f.is_active());
    }

    #[test]
    fn search_finds_run_together_names_and_pasted_ids() {
        let mut voices = list();
        voices.push(voice("pDeep0000000000000001", "DeepCalm", "cloned", &[]));
        voices.push(voice(
            "cStudio00000000000001",
            "StudioRead2024",
            "cloned",
            &[],
        ));
        let mut f = Filter {
            query: "calm".into(),
            ..Default::default()
        };
        assert_eq!(names(&voices, &f), ["DeepCalm"]);
        f.query = "deepcalm".into();
        assert_eq!(names(&voices, &f), ["DeepCalm"]);
        f.query = "read 2024".into();
        assert_eq!(names(&voices, &f), ["StudioRead2024"]);
        f.query = "pDeep0000000000000001".into();
        assert_eq!(names(&voices, &f), ["DeepCalm"], "a pasted voice id");
        // Still word starts: no match inside a word or an id.
        f.query = "eep".into();
        assert!(names(&voices, &f).is_empty());
        f.query = "0001".into();
        assert!(names(&voices, &f).is_empty());
    }

    #[test]
    fn facets_come_from_the_data_and_combine() {
        let voices = list();
        assert_eq!(
            facet_options(&voices, Facet::Category),
            ["cloned", "generated", "professional"]
        );
        assert_eq!(facet_options(&voices, Facet::Gender), ["female", "male"]);
        assert_eq!(
            facet_options(&voices, Facet::Accent),
            ["american", "british", "standard"]
        );
        // "English" (a label) and "cs" (a verified language) as codes.
        let langs = facet_options(&voices, Facet::Language);
        assert_eq!(langs, ["cs", "en"]);
        assert_eq!(facet_display(Facet::Language, "cs"), "Czech");
        assert_eq!(facet_display(Facet::Age, "middle_aged"), "middle aged");
        assert!(facet_options(&[], Facet::Age).is_empty());

        let mut f = Filter::default();
        f.set(Facet::Gender, Some("male".into()));
        assert_eq!(names(&voices, &f), ["Commander Voss", "Old Archivist"]);
        f.set(Facet::Language, Some("en".into()));
        assert_eq!(names(&voices, &f), ["Old Archivist"]);
        f.query = "voss".into();
        assert!(names(&voices, &f).is_empty());
        assert!(f.is_active());
        f.clear();
        assert_eq!(f, Filter::default());
    }

    #[test]
    fn language_keys() {
        assert_eq!(language_key("English"), "en");
        assert_eq!(language_key("en-US"), "en");
        assert_eq!(language_key(" CS "), "cs");
        assert_eq!(language_key("pt_BR"), "pt");
        assert_eq!(language_key("Klingon"), "klingon");
    }

    #[test]
    fn used_by_lists_other_speakers() {
        let rows = [
            ("narrator", "v1"),
            ("vale", " v1 "),
            ("voss", "v2"),
            ("ann", ""),
        ];
        assert_eq!(used_by(rows, "v1", "voss"), ["narrator", "vale"]);
        assert_eq!(used_by(rows, "v1", "vale"), ["narrator"]);
        assert!(used_by(rows, "", "voss").is_empty());
        assert!(used_by(rows, "v9", "voss").is_empty());
    }

    #[test]
    fn preview_sources_in_order_and_cache_names() {
        let mut v = voice("abc", "A", "cloned", &[]);
        assert!(preview_sources(&v, "en").is_empty());
        v.sample = Some(VoiceSample {
            sample_id: "s1".into(),
            mime_type: Some("audio/x-wav".into()),
        });
        v.verified_languages = vec![
            VerifiedLanguage {
                language: "en".into(),
                preview_url: Some("https://x/en.mp3".into()),
                ..Default::default()
            },
            VerifiedLanguage {
                language: "cs".into(),
                preview_url: Some("https://x/cs.mp3".into()),
                ..Default::default()
            },
        ];
        v.preview_url = Some("https://x/p.mp3".into());
        let s = preview_sources(&v, "cs");
        assert_eq!(
            s,
            vec![
                PreviewSource::Url("https://x/p.mp3".into()),
                PreviewSource::Url("https://x/cs.mp3".into()),
                PreviewSource::Url("https://x/en.mp3".into()),
                PreviewSource::Sample {
                    sample_id: "s1".into(),
                    mime_type: Some("audio/x-wav".into())
                },
            ]
        );
        let dir = Path::new("/lib/voice-previews");
        let a = cache_file(dir, "abc", &s[0]);
        assert!(a.starts_with(dir));
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("abc-") && name.ends_with(".mp3"), "{name}");
        assert_ne!(a, cache_file(dir, "abc", &s[1]), "keyed by URL");
        assert!(
            cache_file(dir, "abc", &s[3])
                .to_str()
                .unwrap()
                .ends_with(".wav")
        );
        let odd = cache_file(dir, "../../etc", &s[0]);
        assert_eq!(odd.parent(), Some(dir));
    }

    fn wav_bytes(rate: u32, samples: &[i16]) -> Vec<u8> {
        let mut cur = Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        cur.into_inner()
    }

    #[test]
    fn decodes_wav_samples_and_rejects_garbage() {
        let pcm = decode_preview(&wav_bytes(22050, &[0, 16384, -16384])).unwrap();
        assert_eq!(pcm.rate, 22050);
        assert_eq!(pcm.samples.len(), 3);
        assert!((pcm.samples[1] - 16383).abs() <= 1);
        assert!(decode_preview(b"<html>not audio</html>").is_err());
        assert!(decode_preview(&[]).is_err());
    }

    #[test]
    fn store_replaces_older_previews_of_the_voice() {
        let dir = tempfile::tempdir().unwrap();
        let old = cache_file(dir.path(), "abc", &PreviewSource::Url("https://x/1".into()));
        let new = cache_file(dir.path(), "abc", &PreviewSource::Url("https://x/2".into()));
        let other = cache_file(dir.path(), "abd", &PreviewSource::Url("https://x/1".into()));
        let bytes = wav_bytes(8000, &[1, 2, 3]);
        store(&old, &bytes).unwrap();
        store(&other, &bytes).unwrap();
        store(&new, &bytes).unwrap();
        assert!(!old.exists() && new.exists() && other.exists());
        assert_eq!(read_cached(&new).unwrap().samples.len(), 3);

        std::fs::write(&new, b"broken").unwrap();
        assert!(read_cached(&new).is_none());
        assert!(!new.exists(), "a broken cache file is dropped");
    }
}
