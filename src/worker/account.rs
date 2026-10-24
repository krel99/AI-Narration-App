//! The ElevenLabs balance and per-model list rates.

use super::*;
use crate::tts::account::AccountInfo;

/// Fetch the subscription and the model rates; the rates are stored so the
/// estimates keep working offline. Always answers with `Event::Account`.
pub(super) async fn fetch_account(ctx: &Ctx) -> Result<()> {
    let key = ctx
        .keys
        .read()
        .map(|k| k.elevenlabs.clone())
        .unwrap_or_default();
    if key.is_empty() {
        ctx.out.emit(Event::Account(AccountInfo {
            subscription: Err("no ElevenLabs API key".into()),
            model_rates: 0,
        }));
        return Ok(());
    }
    let client = ElevenLabsClient::new(ctx.http.clone(), key, &ctx.endpoints.elevenlabs);
    let (subscription, models) = tokio::join!(client.subscription(), client.models());
    let model_rates = match models {
        Ok(rates) => {
            let rows: Vec<(String, f64)> = rates
                .into_iter()
                .map(|r| (r.model_id, r.multiplier))
                .collect();
            if let Err(e) = ctx.db(|db| db.set_model_rates(&rows)) {
                log::warn!("storing model rates: {e:#}");
            }
            rows.len()
        }
        Err(e) => {
            log::info!("ElevenLabs model rates unavailable: {e}");
            0
        }
    };
    ctx.out.emit(Event::Account(AccountInfo {
        subscription: subscription.map_err(|e| format!("ElevenLabs balance unavailable: {e}")),
        model_rates,
    }));
    Ok(())
}
