//! The Claude backend of the passes: a client for the configured backend
//! (API key or Claude Code), usage recording, and the Claude Code check
//! shown in Settings.

use super::*;
use crate::claude_code::{self, Invocation};
use crate::config::ClaudeBackend;
use crate::llm::Transport;

pub(super) const PASS_ATTRIBUTION: &str = "attribution";
pub(super) const PASS_TAGS: &str = "tags";

/// The empty, app-owned working directory of Claude Code runs.
pub(super) fn claude_cwd(ctx: &Ctx) -> PathBuf {
    ctx.lib.root.join("claude-cwd")
}

/// A client for the configured backend, or why there is none.
pub(super) async fn claude_client(ctx: &Ctx) -> std::result::Result<ClaudeClient, String> {
    let keys = ctx.keys.read().map(|k| k.clone()).unwrap_or_default();
    match keys.claude_backend {
        ClaudeBackend::ApiKey => {
            if keys.anthropic.is_empty() {
                return Err("No Anthropic API key set (Settings → Claude; Claude Code can use your Pro/Max plan instead).".into());
            }
            Ok(ClaudeClient::new(
                ctx.http.clone(),
                keys.anthropic,
                &ctx.endpoints.anthropic,
            ))
        }
        ClaudeBackend::ClaudeCode => {
            let bin = claude_code::find_binary(keys.claude_code_path.as_deref()).ok_or_else(|| {
                match &keys.claude_code_path {
                    Some(p) => format!(
                        "Claude Code not found at {} (Settings → Claude).",
                        p.display()
                    ),
                    None => "Claude Code is not installed: no `claude` on PATH or in ~/.local/bin (Settings → Claude).".into(),
                }
            })?;
            let cwd = claude_cwd(ctx);
            std::fs::create_dir_all(&cwd)
                .map_err(|e| format!("creating {}: {e}", cwd.display()))?;
            let inv = Invocation::new(bin, cwd);
            match inv.version().await {
                Ok(v) if v < claude_code::MIN_VERSION => {
                    return Err(format!(
                        "Claude Code {v} is too old: {} or newer is needed. Run `claude update` in a terminal.",
                        claude_code::RECOMMENDED_VERSION
                    ));
                }
                Ok(v) if v < claude_code::RECOMMENDED_VERSION => log::warn!(
                    "Claude Code {v}: {} or newer is recommended",
                    claude_code::RECOMMENDED_VERSION
                ),
                Ok(_) => {}
                // The run itself reports anything that really is broken.
                Err(e) => log::warn!("{e}"),
            }
            Ok(ClaudeClient::with_transport(
                ctx.http.clone(),
                Transport::ClaudeCode(inv),
            ))
        }
    }
}

/// Store the usage of the calls `claude` made since the last record.
/// Failures are logged; bookkeeping never stops a pass.
pub(super) fn record_usage(
    ctx: &Ctx,
    claude: &ClaudeClient,
    project_id: Id,
    chapter_id: Id,
    pass: &str,
) {
    let usage = claude.take_usage();
    if usage.is_empty() {
        return;
    }
    let backend = claude.backend();
    let r = ctx.db(|db| {
        for u in &usage {
            db.record_llm_usage(Some(project_id), Some(chapter_id), pass, backend, u)?;
        }
        Ok(())
    });
    if let Err(e) = r {
        log::warn!("recording Claude usage: {e:#}");
    }
}

/// Check the Claude Code installation and sign-in for Settings.
pub(super) async fn detect_claude_code(ctx: &Ctx, path: Option<PathBuf>) {
    let path = path.filter(|p| !p.as_os_str().is_empty());
    let d = claude_code::detect(path.as_deref(), &claude_cwd(ctx)).await;
    ctx.out.emit(Event::ClaudeCode(Box::new(d)));
}
