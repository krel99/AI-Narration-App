//! The voice picker's jobs: the voices in the user's account, lookups of
//! assigned voices that are not among them, and voice previews (downloaded
//! once, then played from `<library>/voice-previews/`).

use super::*;
use crate::tts::account::AccountError;
use crate::voices::{self, PreviewSource};

/// At most this many ids are looked up per request.
const MAX_LOOKUPS: usize = 50;

fn elevenlabs_key(ctx: &Ctx) -> String {
    ctx.keys
        .read()
        .map(|k| k.elevenlabs.clone())
        .unwrap_or_default()
}

/// A voices error as the picker shows it.
fn describe(e: &AccountError) -> String {
    match e {
        AccountError::MissingPermission(scope) => format!(
            "Couldn't load your voices: the API key lacks the {scope} permission (ElevenLabs → API keys → Voices: Read)."
        ),
        AccountError::Rejected { status: 401, .. } => {
            "Couldn't load your voices: ElevenLabs rejected the API key (HTTP 401). Check it in Settings.".into()
        }
        AccountError::Transport(m) => format!("Couldn't reach ElevenLabs: {m}"),
        e => format!("Couldn't load your voices: {e}"),
    }
}

/// The account's voices. Always answers with `Event::Voices`.
pub(super) async fn fetch_voices(ctx: &Ctx) -> Result<()> {
    let key = elevenlabs_key(ctx);
    let result = if key.is_empty() {
        Err("Set the ElevenLabs API key in Settings first.".to_string())
    } else {
        ElevenLabsClient::new(ctx.http.clone(), key, &ctx.endpoints.elevenlabs)
            .voices()
            .await
            .map_err(|e| describe(&e))
    };
    ctx.out.emit(Event::Voices(result));
    Ok(())
}

/// Voices assigned by id that are not in the account list (an ElevenLabs
/// default voice, or a pasted id). Answers with `Event::VoicesLookedUp`;
/// ids that could not be checked (network, permission) are in neither list.
pub(super) async fn lookup_voices(ctx: &Ctx, ids: Vec<String>) -> Result<()> {
    let key = elevenlabs_key(ctx);
    let mut found = Vec::new();
    let mut missing = Vec::new();
    if !key.is_empty() {
        let client = ElevenLabsClient::new(ctx.http.clone(), key, &ctx.endpoints.elevenlabs);
        for id in ids.into_iter().take(MAX_LOOKUPS) {
            match client.voice(&id).await {
                Ok(Some(v)) => found.push(v),
                Ok(None) => missing.push(id),
                Err(e) => log::info!("looking up voice {id}: {e}"),
            }
        }
    }
    ctx.out.emit(Event::VoicesLookedUp { found, missing });
    Ok(())
}

/// Play-ready preview audio of a voice: from the cache, or downloaded once
/// and cached. Tries the voice's preview sources in order. Always answers
/// with `Event::VoicePreview`.
pub(super) async fn preview_voice(ctx: &Ctx, voice: VoiceInfo, language: String) -> Result<()> {
    let result = preview_pcm(ctx, &voice, &language).await;
    if let Err(e) = &result {
        log::info!("preview of {}: {e}", voice.voice_id);
    }
    ctx.out.emit(Event::VoicePreview {
        voice_id: voice.voice_id,
        result,
    });
    Ok(())
}

async fn preview_pcm(
    ctx: &Ctx,
    voice: &VoiceInfo,
    language: &str,
) -> std::result::Result<audio::Pcm, String> {
    let sources = voices::preview_sources(voice, language);
    if sources.is_empty() {
        return Err("This voice has no preview.".into());
    }
    let dir = voices::previews_dir(&ctx.lib.root);
    let key = elevenlabs_key(ctx);
    // The key is only sent to the API (samples), never to the preview host.
    let client = ElevenLabsClient::new(ctx.http.clone(), key.clone(), &ctx.endpoints.elevenlabs);
    // Anything cached first, so a dead preview URL is not asked again on
    // every click when a later source already played.
    let paths: Vec<PathBuf> = sources
        .iter()
        .map(|s| voices::cache_file(&dir, &voice.voice_id, s))
        .collect();
    let cached = paths.clone();
    if let Ok(Some(pcm)) =
        tokio::task::spawn_blocking(move || cached.iter().find_map(|p| voices::read_cached(p)))
            .await
    {
        return Ok(pcm);
    }
    let mut last_error = String::new();
    for (source, path) in sources.into_iter().zip(paths) {
        let bytes = match &source {
            PreviewSource::Url(url) => client.download_preview(url).await,
            PreviewSource::Sample { .. } if key.is_empty() => {
                last_error = "no ElevenLabs API key for the voice's sample".into();
                continue;
            }
            PreviewSource::Sample { sample_id, .. } => {
                client.sample_audio(&voice.voice_id, sample_id).await
            }
        };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                last_error = format!("download failed: {e}");
                continue;
            }
        };
        let decoded = tokio::task::spawn_blocking(move || {
            let pcm = voices::decode_preview(&bytes)?;
            // Cache only what decodes, so a bad download is fetched again.
            if let Err(e) = voices::store(&path, &bytes) {
                log::warn!("caching preview {}: {e:#}", path.display());
            }
            anyhow::Ok(pcm)
        })
        .await
        .map_err(|e| e.to_string())?;
        match decoded {
            Ok(pcm) => return Ok(pcm),
            Err(e) => last_error = format!("can't play the preview: {e:#}"),
        }
    }
    Err(format!("No playable preview ({last_error})."))
}
