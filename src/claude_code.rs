//! Claude through the user's own Claude Code CLI (`claude -p`), signed in
//! with their Pro/Max plan, so the passes use the subscription instead of
//! API billing. The app never sees or stores a Claude token: the user signs
//! in once with `claude auth login` in a terminal.
//!
//! Every run is isolated from the user's Claude Code setup: `--safe-mode`,
//! `--setting-sources=` (no settings files, so no `env` block can swap in an
//! API key), no tools, no MCP, no session files, an empty app-owned working
//! directory, and an environment with every variable that would change the
//! credentials or the model removed. The prompt goes on stdin.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::llm::{CallUsage, LlmError, Via};

/// Fixes structured-output runs that exited non-zero after a dropped
/// connection; older versions work but may report false failures.
pub const RECOMMENDED_VERSION: Version = Version(2, 1, 290);
/// `--safe-mode` first shipped here; older versions reject the flags.
pub const MIN_VERSION: Version = Version(2, 1, 169);

const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Piped stdin is capped at 10 MiB; stay clear of it.
const MAX_STDIN: usize = 10 * 1024 * 1024 - 64 * 1024;

/// Removed from the child's environment: each one would switch the
/// credentials away from the subscription, redirect the OAuth bearer, or
/// override the model or effort the app asks for.
pub const SCRUB_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
    "CLAUDE_CODE_SIMPLE",
    "CLAUDECODE",
    "CLAUDE_CODE_EFFORT_LEVEL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
];

/// Set in the child's environment.
const SET_ENV: &[(&str, &str)] = &[
    // Same output cap as the API transport's max_tokens.
    ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "32000"),
    // Prose may contain "@"; never expand it into file mentions.
    ("CLAUDE_CODE_DISABLE_FILE_MENTIONS", "1"),
    ("CLAUDE_CODE_DISABLE_ATTACHMENTS", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("MAX_STRUCTURED_OUTPUT_RETRIES", "5"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// Parses `claude --version` output: "2.1.310 (Claude Code)".
    pub fn parse(s: &str) -> Option<Self> {
        let word = s.split_whitespace().next()?;
        let mut it = word.trim_start_matches('v').split('.');
        let mut num = || -> Option<u32> {
            let part = it.next()?;
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        };
        Some(Version(num()?, num()?, num().unwrap_or(0)))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// A configured path, with a leading `~` meaning the home directory (as
/// typed in Settings; no shell expands it).
fn expand_home(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

/// The `claude` binary: the configured path, else `claude` on PATH, else
/// the native installer's `~/.local/bin/claude` (a desktop launcher's PATH
/// often lacks it).
pub fn find_binary(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| !p.as_os_str().is_empty()) {
        let p = expand_home(p);
        return is_executable(&p).then_some(p);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("claude");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let local = home.join(".local/bin/claude");
    is_executable(&local).then_some(local)
}

/// A request for one structured answer.
pub struct StructuredRequest<'a> {
    /// Full model id (`claude-opus-5-5`): aliases can be redirected by the
    /// user's environment.
    pub model: &'a str,
    pub effort: &'a str,
    pub system: &'a str,
    pub schema: &'a Value,
    /// Sent on stdin, never as an argument.
    pub prompt: String,
}

/// How to run the CLI.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub bin: PathBuf,
    /// An empty directory the app owns, so no project `.claude/` settings,
    /// hooks or `.mcp.json` apply.
    pub cwd: PathBuf,
    /// Applied before the scrub (tests use it to prove the scrub works).
    pub extra_env: Vec<(String, String)>,
}

/// `claude auth status` (JSON).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthStatus {
    pub logged_in: bool,
    /// none | claude.ai | oauth_token | api_key | api_key_helper | third_party
    pub auth_method: String,
    pub api_provider: Option<String>,
    pub api_key_source: Option<String>,
}

impl AuthStatus {
    pub fn parse(stdout: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(stdout.trim()).ok()?;
        let s = |k: &str| v[k].as_str().map(str::to_string);
        Some(AuthStatus {
            logged_in: v["loggedIn"].as_bool().unwrap_or(false),
            auth_method: s("authMethod").unwrap_or_else(|| "none".into()),
            api_provider: s("apiProvider"),
            api_key_source: s("apiKeySource"),
        })
    }

    /// Calls are billed to the user's plan.
    pub fn uses_subscription(&self) -> bool {
        self.logged_in && matches!(self.auth_method.as_str(), "claude.ai" | "oauth_token")
    }
}

/// Retries a spawn that hits ETXTBSY (the binary is being replaced, e.g. by
/// `claude update`, or was just written).
async fn spawn(cmd: &mut Command) -> std::io::Result<tokio::process::Child> {
    let mut attempt = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(26) && attempt < 10 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => return other,
        }
    }
}

fn not_installed(bin: &Path) -> String {
    format!(
        "Claude Code is not installed (no {}). Install it, sign in once with `claude auth login` in a terminal, or set its path in Settings → Claude.",
        bin.display()
    )
}

/// Why the CLI could not be started. ENOENT for a binary that exists means
/// its interpreter (e.g. `node` for an npm install) is missing.
fn spawn_error(bin: &Path, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound && !bin.exists() {
        not_installed(bin)
    } else {
        format!("could not run Claude Code ({}): {e}", bin.display())
    }
}

impl Invocation {
    pub fn new(bin: PathBuf, cwd: PathBuf) -> Self {
        Self {
            bin,
            cwd,
            extra_env: vec![],
        }
    }

    /// The base command: isolated environment and working directory.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        for k in SCRUB_ENV {
            cmd.env_remove(k);
        }
        for (k, v) in SET_ENV {
            cmd.env(k, v);
        }
        // Always the app-owned directory (see `ensure_cwd`), never the one
        // the app was started from.
        cmd.current_dir(&self.cwd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// Creates the working directory when it is missing. Claude Code must not
    /// fall back to the app's own working directory, where a project's
    /// `.claude/` settings or `.mcp.json` could apply.
    fn ensure_cwd(&self) -> Result<(), String> {
        if !self.cwd.is_dir() {
            std::fs::create_dir_all(&self.cwd).map_err(|e| {
                format!(
                    "cannot create Claude Code's working directory {}: {e}",
                    self.cwd.display()
                )
            })?;
        }
        Ok(())
    }

    /// Runs a short probe (`--version`, `auth status`): (exit code, stdout, stderr).
    async fn probe(&self, args: &[&str]) -> Result<(Option<i32>, String, String), String> {
        self.ensure_cwd()?;
        let mut cmd = self.command();
        cmd.args(args).stdin(Stdio::null());
        let child = spawn(&mut cmd)
            .await
            .map_err(|e| spawn_error(&self.bin, &e))?;
        let out = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| format!("`claude {}` did not answer", args.join(" ")))?
            .map_err(|e| e.to_string())?;
        Ok((
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ))
    }

    pub async fn version(&self) -> Result<Version, String> {
        let (_, stdout, stderr) = self.probe(&["--version"]).await?;
        Version::parse(&stdout).ok_or_else(|| {
            format!(
                "unrecognised `claude --version` output: {}",
                first_line(if stdout.trim().is_empty() {
                    &stderr
                } else {
                    &stdout
                })
            )
        })
    }

    /// `claude --setting-sources= auth status` (the flag must come before the
    /// subcommand) with the same scrubbed environment as a real run. With
    /// `isolated: false` the settings files are read, as a plain `claude`
    /// in a terminal would.
    pub async fn auth_status(&self, isolated: bool) -> Result<AuthStatus, String> {
        let args: &[&str] = if isolated {
            &["--setting-sources=", "auth", "status"]
        } else {
            &["auth", "status"]
        };
        let (code, stdout, stderr) = self.probe(args).await?;
        AuthStatus::parse(&stdout).ok_or_else(|| {
            let detail = if stderr.trim().is_empty() {
                first_line(&stdout)
            } else {
                first_line(&stderr)
            };
            format!(
                "`claude auth status` failed (exit {}): {detail}",
                code.map_or("?".into(), |c| c.to_string())
            )
        })
    }

    /// One structured request. Returns the `structured_output` object, plus
    /// the usage the run reported (also when it failed).
    pub async fn run(
        &self,
        req: &StructuredRequest<'_>,
    ) -> (Result<Value, LlmError>, Vec<CallUsage>) {
        if req.prompt.len() > MAX_STDIN {
            return (Err(LlmError::TooLong), vec![]);
        }
        if let Err(e) = self.ensure_cwd() {
            return (Err(LlmError::Unavailable(e)), vec![]);
        }
        let mut cmd = self.command();
        cmd.args(run_args(req));
        let mut child = match spawn(&mut cmd).await {
            Ok(c) => c,
            Err(e) => {
                return (
                    Err(LlmError::Unavailable(spawn_error(&self.bin, &e))),
                    vec![],
                );
            }
        };
        // Write stdin while stdout/stderr are read, so neither side stalls.
        let stdin = child.stdin.take();
        let body = req.prompt.clone().into_bytes();
        let writer = tokio::spawn(async move {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(&body).await;
                let _ = stdin.shutdown().await;
            }
        });
        // On timeout the child is dropped, and kill_on_drop ends it.
        let out = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output()).await;
        let _ = writer.await;
        match out {
            Err(_) => (
                Err(LlmError::Fatal(
                    Via::ClaudeCode,
                    "no answer within 20 minutes".into(),
                )),
                vec![],
            ),
            Ok(Err(e)) => (
                Err(LlmError::Transient(Via::ClaudeCode, e.to_string())),
                vec![],
            ),
            Ok(Ok(o)) => parse_result(
                &String::from_utf8_lossy(&o.stdout),
                &String::from_utf8_lossy(&o.stderr),
                o.status.code(),
                req.model,
            ),
        }
    }
}

/// The CLI arguments of a structured run. Never `--bare` (it ignores the
/// subscription login) and no `--max-budget-usd` (a tripped cap throws the
/// finished answer away).
pub fn run_args(req: &StructuredRequest<'_>) -> Vec<String> {
    [
        "-p",
        "--output-format",
        "json",
        "--json-schema",
        &req.schema.to_string(),
        "--model",
        req.model,
        "--effort",
        req.effort,
        "--system-prompt",
        req.system,
        "--tools",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--safe-mode",
        "--setting-sources=",
        "--no-session-persistence",
        "--max-turns",
        "8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn first_line(s: &str) -> String {
    let line = s.trim().lines().next().unwrap_or("").trim();
    line.chars().take(300).collect()
}

/// The result object: the whole of stdout, the last `type: result` element
/// of an array (a Stop hook can make the CLI print one), or the last JSON
/// line.
fn find_result(stdout: &str) -> Option<Value> {
    let pick = |v: Value| -> Option<Value> {
        match v {
            Value::Array(items) => items.into_iter().rev().find(|e| e["type"] == "result"),
            v if v.is_object() => Some(v),
            _ => None,
        }
    };
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return pick(v);
    }
    trimmed
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find_map(pick)
}

/// Token accounting from `modelUsage` (never `usage`, which is zero on some
/// error results). `costUSD` is the CLI's list-price estimate.
pub fn usage_of(v: &Value, requested_model: &str) -> Vec<CallUsage> {
    let mut out: Vec<CallUsage> = v["modelUsage"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(model, u)| {
            let n = |k: &str| u[k].as_u64().unwrap_or(0);
            let (input, output, cache_read, cache_write) = (
                n("inputTokens"),
                n("outputTokens"),
                n("cacheReadInputTokens"),
                n("cacheCreationInputTokens"),
            );
            CallUsage {
                model: model.clone(),
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                cost_usd: u["costUSD"].as_f64().or_else(|| {
                    crate::costs::claude_cost_usd(model, input, output, cache_read, cache_write)
                }),
            }
        })
        .collect();
    if out.is_empty()
        && let Some(cost) = v["total_cost_usd"].as_f64().filter(|c| *c > 0.0)
    {
        out.push(CallUsage {
            model: requested_model.to_string(),
            cost_usd: Some(cost),
            ..Default::default()
        });
    }
    out
}

/// Turns one run's output into the structured answer or a classified error.
pub fn parse_result(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    requested_model: &str,
) -> (Result<Value, LlmError>, Vec<CallUsage>) {
    let Some(v) = find_result(stdout) else {
        return (Err(no_result(stderr, exit_code)), vec![]);
    };
    let usage = usage_of(&v, requested_model);
    let ok = v["is_error"] == false && v["subtype"] == "success";
    match v.get("structured_output") {
        Some(s) if ok && !s.is_null() => (Ok(s.clone()), usage),
        _ => (Err(classify(&v)), usage),
    }
}

fn no_result(stderr: &str, exit_code: Option<i32>) -> LlmError {
    let err = stderr.trim();
    if err.contains("unknown option") {
        return LlmError::Unavailable(format!(
            "Claude Code rejected an option ({}). Update it with `claude update` ({RECOMMENDED_VERSION} or newer is needed).",
            first_line(err)
        ));
    }
    if err.contains("exceeds 10MB") {
        return LlmError::TooLong;
    }
    if !err.is_empty() {
        return LlmError::Unavailable(format!("Claude Code failed: {}", first_line(err)));
    }
    LlmError::Unavailable(format!(
        "Claude Code exited with code {} without a result (possibly a usage limit; run `claude` in a terminal to check).",
        exit_code.map_or("?".into(), |c| c.to_string())
    ))
}

/// Error classification, in this order: usage limit, output too long, plan
/// lacks the model (its text mentions /login, so it goes before auth), auth,
/// API credit, context too long, refusal, error subtypes, temporary
/// throttling, anything else.
fn classify(v: &Value) -> LlmError {
    let result = v["result"].as_str().unwrap_or("").trim();
    let api_error = v["api_error"].as_str().unwrap_or("");
    let status = v["api_error_status"].as_u64();
    let subtype = v["subtype"].as_str().unwrap_or("");
    let errors: Vec<&str> = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let detail = if !result.is_empty() {
        result.to_string()
    } else if !errors.is_empty() {
        errors.join("; ")
    } else {
        format!("{subtype} without an answer")
    };

    // The CLI may print a typographic apostrophe; older versions said
    // "Claude AI usage limit reached|<reset>". The temporary throttle says
    // "(not your usage limit)" and must not match.
    let plain = result.replace('\u{2019}', "'");
    let hit_limit = plain.starts_with("You've hit your")
        || (status == Some(429) && plain.starts_with("You've hit"));
    if api_error == "usage_limit_reached"
        || hit_limit
        || result.to_lowercase().contains("usage limit reached")
    {
        let text = if result.is_empty() {
            "usage limit reached"
        } else {
            result
        };
        return LlmError::UsageLimit(format!(
            "Claude usage limit: {text}. Your plan's limits are shared with the Claude apps; try again after the reset, or switch to the API key backend in Settings."
        ));
    }
    if api_error == "max_output_tokens" || result.contains("output token maximum") {
        return LlmError::TooLong;
    }
    // Before the auth checks: this message ends "run /logout and /login for
    // the plan to take effect", but signing in again does not help.
    if result.contains("is not available with the Claude") {
        return LlmError::Unavailable(format!(
            "{result} Choose Sonnet (claude-sonnet-5-5) under Project → Claude → Model."
        ));
    }
    if result.starts_with("Invalid API key") {
        return LlmError::Auth(
            "Claude Code used an API key instead of your subscription, and the key was rejected (\"Invalid API key\"). Narration Studio removes ANTHROPIC_API_KEY and skips your settings files, so the key comes from managed settings or an apiKeyHelper; check `claude auth status` in a terminal.".into(),
        );
    }
    if result.starts_with("Not logged in")
        || result.starts_with("Login expired")
        || result.starts_with("Failed to authenticate")
        || result.contains("/login")
        || api_error == "authentication_failed"
        || status == Some(401)
    {
        return LlmError::Auth(format!(
            "Claude Code is not signed in ({detail}). Run `claude auth login` in a terminal, then try again."
        ));
    }
    if result.contains("Credit balance is too low") {
        return LlmError::Unavailable(
            "Claude Code is billing an API key that has no credit (\"Credit balance is too low\") instead of your subscription. Check `claude auth status` in a terminal.".into(),
        );
    }
    if result.contains("Prompt is too long") {
        return LlmError::TooLong;
    }
    if v["stop_reason"] == "refusal" {
        return LlmError::Fatal(
            Via::ClaudeCode,
            format!("the model declined this chunk ({detail})"),
        );
    }
    match subtype {
        "error_max_structured_output_retries" | "error_max_turns" => {
            return LlmError::Transient(Via::ClaudeCode, format!("{subtype}: {detail}"));
        }
        "error_during_execution" | "error_max_budget_usd" => {
            return LlmError::Fatal(Via::ClaudeCode, format!("{subtype}: {detail}"));
        }
        _ => {}
    }
    if result.contains("temporarily limiting requests")
        || result.to_lowercase().contains("overloaded")
        || matches!(status, Some(429 | 500..=599))
    {
        return LlmError::Transient(Via::ClaudeCode, detail);
    }
    if v["is_error"] == false && v["subtype"] == "success" {
        return LlmError::Transient(Via::ClaudeCode, "no structured output in the answer".into());
    }
    LlmError::Fatal(Via::ClaudeCode, detail)
}

// ---- detection ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteLevel {
    Ok,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub level: NoteLevel,
    pub text: String,
}

fn note(level: NoteLevel, text: impl Into<String>) -> Note {
    Note {
        level,
        text: text.into(),
    }
}

/// What Settings shows about the Claude Code backend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detection {
    pub binary: Option<PathBuf>,
    pub version: Option<Version>,
    /// From the isolated status check, which matches what real runs see.
    pub status: Option<AuthStatus>,
    pub notes: Vec<Note>,
}

impl Detection {
    /// Installed, new enough, and signed in with a subscription.
    pub fn ready(&self) -> bool {
        self.binary.is_some()
            && self.version.is_none_or(|v| v >= MIN_VERSION)
            && self
                .status
                .as_ref()
                .is_some_and(AuthStatus::uses_subscription)
    }
}

/// Environment variables of this app that the backend removes, worth
/// telling the user about (the removal is deliberate, not silent).
pub fn ignored_env_vars() -> Vec<&'static str> {
    [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
    ]
    .into_iter()
    .filter(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
    .collect()
}

/// The notes for a detection result (pure).
pub fn assess(
    version: Option<Result<Version, String>>,
    status: Option<Result<AuthStatus, String>>,
    plain_status: Option<AuthStatus>,
    ignored_env: &[&str],
) -> Vec<Note> {
    let mut notes = Vec::new();
    match version {
        Some(Ok(v)) if v < MIN_VERSION => notes.push(note(
            NoteLevel::Error,
            format!(
                "Claude Code {v} is too old: {RECOMMENDED_VERSION} or newer is needed. Run `claude update` in a terminal."
            ),
        )),
        Some(Ok(v)) if v < RECOMMENDED_VERSION => notes.push(note(
            NoteLevel::Warn,
            format!(
                "Claude Code {v} works, but {RECOMMENDED_VERSION} or newer is recommended (it fixes runs wrongly reported as failed). Run `claude update`."
            ),
        )),
        Some(Ok(v)) => notes.push(note(NoteLevel::Ok, format!("Claude Code {v}"))),
        Some(Err(e)) => notes.push(note(NoteLevel::Warn, e)),
        None => {}
    }
    match status {
        Some(Ok(s)) if !s.logged_in || s.auth_method == "none" => {
            notes.push(note(
                NoteLevel::Error,
                "Not signed in. Run `claude auth login` in a terminal (choose your Claude subscription).",
            ));
            if plain_status.is_some_and(|p| p.logged_in) {
                notes.push(note(
                    NoteLevel::Info,
                    "A plain `claude` is signed in, probably through a token in a settings file's \"env\" block, which this app skips. Export CLAUDE_CODE_OAUTH_TOKEN in your shell profile instead, or run `claude auth login`.",
                ));
            }
        }
        Some(Ok(s)) => match s.auth_method.as_str() {
            "claude.ai" => notes.push(note(
                NoteLevel::Ok,
                "Signed in with your Claude subscription",
            )),
            "oauth_token" => notes.push(note(
                NoteLevel::Ok,
                "Signed in with a subscription token (CLAUDE_CODE_OAUTH_TOKEN)",
            )),
            "api_key" | "api_key_helper" => {
                let source = s
                    .api_key_source
                    .as_deref()
                    .map(|x| format!(" ({x})"))
                    .unwrap_or_default();
                notes.push(note(
                    NoteLevel::Warn,
                    format!(
                        "Claude Code uses an API key{source}: calls are billed to the API, not your plan."
                    ),
                ));
            }
            "third_party" => notes.push(note(
                NoteLevel::Warn,
                format!(
                    "Claude Code uses a cloud provider ({}): calls are billed there, not to your plan.",
                    s.api_provider.as_deref().unwrap_or("Bedrock, Vertex or Foundry")
                ),
            )),
            other => notes.push(note(
                NoteLevel::Warn,
                format!("Signed in with an unrecognised method ({other})."),
            )),
        },
        Some(Err(e)) => notes.push(note(NoteLevel::Warn, e)),
        None => {}
    }
    if !ignored_env.is_empty() {
        notes.push(note(
            NoteLevel::Info,
            format!(
                "{} {} set in your environment; this backend removes {} so your plan is used.{}",
                ignored_env.join(", "),
                if ignored_env.len() == 1 { "is" } else { "are" },
                if ignored_env.len() == 1 { "it" } else { "them" },
                if ignored_env.contains(&"ANTHROPIC_API_KEY") {
                    " The API-key backend still reads ANTHROPIC_API_KEY."
                } else {
                    ""
                }
            ),
        ));
    }
    notes
}

/// Looks for the CLI and checks its version and sign-in.
pub async fn detect(configured: Option<&Path>, cwd: &Path) -> Detection {
    let Some(bin) = find_binary(configured) else {
        let text = match configured.filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => format!(
                "{} is not an executable file. Fix the path, or clear it to search PATH and ~/.local/bin.",
                p.display()
            ),
            None => "Claude Code is not installed: no `claude` on PATH or in ~/.local/bin. Install Claude Code, then run `claude auth login` in a terminal.".into(),
        };
        return Detection {
            notes: vec![note(NoteLevel::Error, text)],
            ..Default::default()
        };
    };
    let _ = std::fs::create_dir_all(cwd);
    let inv = Invocation::new(bin.clone(), cwd.to_path_buf());
    let version = inv.version().await;
    let status = inv.auth_status(true).await;
    let plain = match &status {
        Ok(s) if !s.logged_in => inv.auth_status(false).await.ok(),
        _ => None,
    };
    Detection {
        binary: Some(bin),
        version: version.as_ref().ok().copied(),
        notes: assess(
            Some(version),
            Some(status.clone()),
            plain,
            &ignored_env_vars(),
        ),
        status: status.ok(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    /// A fake `claude`: records argv (NUL-separated), env, cwd and stdin of
    /// call N into `<dir>/argv.N` …, and answers `--version`, `auth status`
    /// and runs from files in `<dir>`: `version`, `auth.json`/`auth_exit`,
    /// `out.N.json` (falling back to `out.json`), `err.txt`, `exit`.
    pub struct FakeClaude {
        pub dir: tempfile::TempDir,
    }

    const SCRIPT: &str = r#"#!/bin/sh
D="$(dirname "$0")"
if [ "$1" = "--version" ]; then cat "$D/version"; exit 0; fi
case " $* " in
  *" auth status "*)
    printf '%s\0' "$@" > "$D/auth_argv"
    cat "$D/auth.json"; exit "$(cat "$D/auth_exit" 2>/dev/null || echo 0)";;
esac
N="$(cat "$D/count" 2>/dev/null || echo 0)"
echo $((N + 1)) > "$D/count"
printf '%s\0' "$@" > "$D/argv.$N"
env > "$D/env.$N"
pwd -P > "$D/cwd.$N"
cat > "$D/stdin.$N"
if [ -f "$D/out.$N.json" ]; then cat "$D/out.$N.json"; elif [ -f "$D/out.json" ]; then cat "$D/out.json"; fi
if [ -f "$D/err.txt" ]; then cat "$D/err.txt" >&2; fi
exit "$(cat "$D/exit" 2>/dev/null || echo 0)"
"#;

    impl FakeClaude {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("claude");
            std::fs::write(&path, SCRIPT).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let fake = FakeClaude { dir };
            fake.set("version", "2.1.310 (Claude Code)\n");
            fake.set(
                "auth.json",
                r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}"#,
            );
            std::fs::create_dir_all(fake.cwd()).unwrap();
            fake
        }

        pub fn bin(&self) -> PathBuf {
            self.dir.path().join("claude")
        }

        pub fn cwd(&self) -> PathBuf {
            self.dir.path().join("cwd")
        }

        pub fn set(&self, name: &str, content: &str) {
            std::fs::write(self.dir.path().join(name), content).unwrap();
        }

        pub fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
        }

        /// Arguments of run N (empty ones included, like `--tools ""`).
        pub fn argv(&self, n: usize) -> Vec<String> {
            split_nul(&self.read(&format!("argv.{n}")))
        }

        pub fn calls(&self) -> usize {
            self.read("count").trim().parse().unwrap_or(0)
        }

        pub fn invocation(&self) -> Invocation {
            Invocation::new(self.bin(), self.cwd())
        }
    }

    pub fn split_nul(s: &str) -> Vec<String> {
        let s = s.strip_suffix('\0').unwrap_or(s);
        if s.is_empty() {
            return vec![];
        }
        s.split('\0').map(str::to_string).collect()
    }

    /// A success result as Claude Code 2.1.310 prints it (trimmed).
    pub fn success(structured: Value, cost: f64) -> String {
        json!({
            "type": "result", "subtype": "success", "is_error": false,
            "api_error_status": null, "num_turns": 2, "stop_reason": "tool_use",
            "terminal_reason": "completed", "total_cost_usd": cost,
            "usage": {"input_tokens": 0, "output_tokens": 0},
            "modelUsage": {"claude-opus-5-5": {
                "inputTokens": 2, "outputTokens": 183, "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 1153, "costUSD": cost,
                "contextWindow": 1000000, "maxOutputTokens": 128000
            }},
            "result": structured.to_string(),
            "structured_output": structured
        })
        .to_string()
    }

    fn request<'a>(schema: &'a Value) -> StructuredRequest<'a> {
        StructuredRequest {
            model: "claude-opus-5-5",
            effort: "medium",
            system: "You attribute speech.",
            schema,
            prompt: "Paragraphs:\n\n[P1 TARGET]\n\"Hi,\" @she said.".into(),
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(
            Version::parse("2.1.310 (Claude Code)"),
            Some(Version(2, 1, 310))
        );
        assert_eq!(Version::parse("v2.1.169"), Some(Version(2, 1, 169)));
        assert_eq!(Version::parse("garbage"), None);
        assert!(Version(2, 1, 300) > RECOMMENDED_VERSION);
        assert!(Version(2, 1, 100) < MIN_VERSION);
        assert_eq!(Version(2, 1, 290).to_string(), "2.1.290");
    }

    #[test]
    fn args_follow_the_recommended_invocation() {
        let schema = json!({"type": "object"});
        let args = run_args(&request(&schema));
        let pos = |a: &str| args.iter().position(|x| x == a).unwrap();
        assert_eq!(args[pos("--model") + 1], "claude-opus-5-5");
        assert_eq!(args[pos("--effort") + 1], "medium");
        assert_eq!(args[pos("--tools") + 1], "");
        assert_eq!(args[pos("--max-turns") + 1], "8");
        assert_eq!(args[pos("--json-schema") + 1], schema.to_string());
        for flag in [
            "-p",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--safe-mode",
            "--setting-sources=",
            "--no-session-persistence",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag}");
        }
        assert!(!args.iter().any(|a| a == "--bare" || a.contains("budget")));
        assert!(
            !args.iter().any(|a| a.contains("[P1 TARGET]")),
            "the prompt never goes on the command line"
        );
    }

    #[test]
    fn success_and_array_results() {
        let s = json!({"segments": []});
        let (r, usage) = parse_result(&success(s.clone(), 0.012892), "", Some(0), "x");
        assert_eq!(r.unwrap(), s);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].model, "claude-opus-5-5");
        assert_eq!(usage[0].cache_write_tokens, 1153);
        assert_eq!(usage[0].prompt_tokens(), 1155);
        assert_eq!(usage[0].cost_usd, Some(0.012892));

        // A Stop hook can turn the output into an array of events.
        let arr = format!(
            "[{{\"type\":\"system\",\"subtype\":\"init\"}},{}]",
            success(s.clone(), 0.01)
        );
        assert_eq!(parse_result(&arr, "", Some(0), "x").0.unwrap(), s);

        // Success without structured_output is not a success.
        let mut v: Value = serde_json::from_str(&success(s, 0.01)).unwrap();
        v.as_object_mut().unwrap().remove("structured_output");
        let (r, _) = parse_result(&v.to_string(), "", Some(0), "x");
        assert!(matches!(r, Err(LlmError::Transient(..))), "{r:?}");
    }

    fn error_result(fields: Value) -> String {
        let mut v = json!({
            "type": "result", "subtype": "success", "is_error": true,
            "terminal_reason": "api_error", "api_error_status": null,
            "stop_reason": "stop_sequence", "num_turns": 1, "total_cost_usd": 0,
            "usage": {"input_tokens": 0}, "modelUsage": {}
        });
        for (k, val) in fields.as_object().unwrap() {
            v[k] = val.clone();
        }
        v.to_string()
    }

    #[test]
    fn errors_are_classified() {
        let classify_out = |out: String| parse_result(&out, "", Some(1), "claude-opus-5-5");

        // Observed when not signed in.
        let (r, usage) = classify_out(error_result(
            json!({"result": "Not logged in · Please run /login"}),
        ));
        let Err(LlmError::Auth(m)) = r else {
            panic!("{r:?}")
        };
        assert!(m.contains("claude auth login"));
        assert!(usage.is_empty());

        // Usage limit (reported shape).
        let (r, _) = classify_out(error_result(json!({
            "api_error_status": 429, "api_error": "usage_limit_reached",
            "result": "You've hit your weekly limit · resets Mon 12:00am"
        })));
        let Err(LlmError::UsageLimit(m)) = r else {
            panic!("{r:?}")
        };
        assert!(m.contains("resets Mon 12:00am"));
        assert!(LlmError::UsageLimit(m).stops_pass());

        // Output cap: observed with stop_reason stop_sequence, not max_tokens.
        let (r, usage) = classify_out(error_result(json!({
            "api_error": "max_output_tokens", "num_turns": 4, "total_cost_usd": 0.00060724,
            "result": "API Error: Claude's response exceeded the 40 output token maximum.",
            "modelUsage": {"claude-haiku-5-5": {"inputTokens": 8, "outputTokens": 160,
                "cacheReadInputTokens": 2484, "cacheCreationInputTokens": 2508, "costUSD": 0.00060724}}
        })));
        assert!(matches!(r, Err(LlmError::TooLong)), "{r:?}");
        assert_eq!(
            usage[0].output_tokens, 160,
            "the spent tokens are still counted"
        );

        // An API key leaked in and was rejected.
        let (r, _) = classify_out(error_result(json!({
            "api_error_status": 401, "api_error_origin": "first_party",
            "result": "Invalid API key · Fix external API key"
        })));
        let Err(LlmError::Auth(m)) = r else {
            panic!("{r:?}")
        };
        assert!(m.contains("instead of your subscription"), "{m}");

        let (r, _) = classify_out(error_result(json!({
            "result": "Failed to authenticate: OAuth session expired and could not be refreshed"
        })));
        assert!(matches!(r, Err(LlmError::Auth(_))));
        // The documented text mentions /login, but signing in again does
        // not help: it must suggest Sonnet, not `claude auth login`.
        let (r, _) = classify_out(error_result(json!({
            "result": "Claude Opus is not available with the Claude Pro plan. If you have updated your subscription plan recently, run /logout and /login for the plan to take effect."
        })));
        assert!(
            matches!(r, Err(LlmError::Unavailable(ref m)) if m.contains("Sonnet") && !m.contains("not signed in")),
            "{r:?}"
        );
        // Usage-limit wording variants: a typographic apostrophe, and the
        // older "usage limit reached|<reset>" form.
        for text in [
            "You\u{2019}ve hit your weekly limit · resets Mon 12:00am",
            "Claude AI usage limit reached|1793664000",
        ] {
            let (r, _) = classify_out(error_result(json!({"result": text})));
            assert!(matches!(r, Err(LlmError::UsageLimit(_))), "{text}: {r:?}");
        }
        let (r, _) = classify_out(error_result(json!({"result": "Prompt is too long"})));
        assert!(matches!(r, Err(LlmError::TooLong)));
        let (r, _) = classify_out(error_result(json!({
            "result": "API Error: Server is temporarily limiting requests (not your usage limit)"
        })));
        assert!(matches!(r, Err(LlmError::Transient(..))), "{r:?}");
        let (r, _) = classify_out(error_result(json!({
            "subtype": "error_max_structured_output_retries", "errors": ["schema mismatch"]
        })));
        assert!(matches!(r, Err(LlmError::Transient(_, ref m)) if m.contains("schema mismatch")));
        let (r, _) = classify_out(error_result(json!({
            "subtype": "error_max_budget_usd", "terminal_reason": "budget_exhausted",
            "errors": ["Reached maximum budget ($0.00001)"]
        })));
        assert!(matches!(r, Err(LlmError::Fatal(..))));
        // Messages name Claude Code, not the API.
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.starts_with("Claude Code error: error_max_budget_usd: ")
                && !msg.contains("API error"),
            "{msg}"
        );
        let (r, _) = classify_out(error_result(json!({
            "stop_reason": "refusal", "result": "declined"
        })));
        assert!(matches!(r, Err(LlmError::Fatal(_, ref m)) if m.contains("declined")));

        // No result object at all.
        let (r, _) = parse_result("", "error: unknown option '--safe-mode'\n", Some(1), "m");
        assert!(matches!(r, Err(LlmError::Unavailable(ref m)) if m.contains("claude update")));
        let (r, _) = parse_result("", "Error: --json-schema is not valid JSON", Some(1), "m");
        assert!(
            matches!(r, Err(LlmError::Unavailable(ref m)) if m.contains("--json-schema is not valid JSON"))
        );
        let (r, _) = parse_result("", "", Some(1), "m");
        assert!(
            matches!(r, Err(LlmError::Unavailable(ref m)) if m.contains("code 1 without a result") && m.contains("usage limit"))
        );
    }

    #[test]
    fn status_and_notes() {
        let s = AuthStatus::parse(
            r#"{"loggedIn":true,"authMethod":"oauth_token","apiProvider":"firstParty"}"#,
        )
        .unwrap();
        assert!(s.uses_subscription());
        let key = AuthStatus::parse(
            r#"{"loggedIn":true,"authMethod":"api_key","apiKeySource":"ANTHROPIC_API_KEY"}"#,
        )
        .unwrap();
        assert!(!key.uses_subscription());
        let notes = assess(
            Some(Ok(Version(2, 1, 310))),
            Some(Ok(key)),
            None,
            &["ANTHROPIC_API_KEY"],
        );
        assert!(
            notes.iter().any(|n| n.level == NoteLevel::Warn
                && n.text.contains("billed to the API, not your plan"))
        );
        assert!(
            notes
                .iter()
                .any(|n| n.level == NoteLevel::Info && n.text.contains("ANTHROPIC_API_KEY is set"))
        );

        let none = AuthStatus::parse(r#"{"loggedIn":false,"authMethod":"none"}"#).unwrap();
        let plain = AuthStatus {
            logged_in: true,
            auth_method: "oauth_token".into(),
            ..Default::default()
        };
        let notes = assess(
            Some(Ok(Version(2, 1, 200))),
            Some(Ok(none)),
            Some(plain),
            &[],
        );
        assert_eq!(
            notes[0].level,
            NoteLevel::Warn,
            "below the recommended version"
        );
        assert!(notes[1].text.contains("claude auth login"));
        assert!(notes[2].text.contains("settings file"));

        let old = assess(Some(Ok(Version(2, 1, 100))), None, None, &[]);
        assert_eq!(old[0].level, NoteLevel::Error);
        assert!(old[0].text.contains("too old"));
    }

    #[test]
    fn fake_binary_success_goes_through_stdin_with_a_scrubbed_env() {
        let fake = FakeClaude::new();
        let s = json!({"segments": [{"paragraph": 1}]});
        fake.set("out.json", &success(s.clone(), 0.02));
        let mut inv = fake.invocation();
        inv.extra_env = vec![
            ("ANTHROPIC_API_KEY".into(), "sk-ant-leak".into()),
            ("CLAUDE_CODE_EFFORT_LEVEL".into(), "max".into()),
            (
                "ANTHROPIC_BASE_URL".into(),
                "https://gateway.invalid".into(),
            ),
            ("CLAUDE_CONFIG_DIR".into(), "/keep/me".into()),
        ];
        let schema = json!({"type": "object"});
        let req = request(&schema);
        let (r, usage) = rt().block_on(inv.run(&req));
        assert_eq!(r.unwrap(), s);
        assert_eq!(usage[0].cost_usd, Some(0.02));

        assert_eq!(fake.read("stdin.0"), req.prompt, "prompt on stdin");
        let argv = fake.argv(0);
        assert_eq!(argv, run_args(&req), "exactly the documented flags");
        assert!(!argv.iter().any(|a| a.contains("[P1 TARGET]")));
        let env = fake.read("env.0");
        let has = |line: &str| env.lines().any(|l| l == line);
        let has_key = |k: &str| env.lines().any(|l| l.starts_with(&format!("{k}=")));
        assert!(
            !has_key("ANTHROPIC_API_KEY"),
            "the API key must not reach the child"
        );
        assert!(!has_key("CLAUDE_CODE_EFFORT_LEVEL"));
        assert!(!has_key("ANTHROPIC_BASE_URL"));
        assert!(!has_key("CLAUDECODE"));
        assert!(
            has("CLAUDE_CONFIG_DIR=/keep/me"),
            "the login location is kept"
        );
        assert!(has("CLAUDE_CODE_MAX_OUTPUT_TOKENS=32000"));
        assert!(has("CLAUDE_CODE_DISABLE_FILE_MENTIONS=1"));
        assert!(has("CLAUDE_CODE_DISABLE_ATTACHMENTS=1"));
        assert_eq!(
            fake.read("cwd.0").trim(),
            fake.cwd().canonicalize().unwrap().display().to_string()
        );
    }

    #[test]
    fn fake_binary_failures() {
        let schema = json!({"type": "object"});
        let run = |fake: &FakeClaude| rt().block_on(fake.invocation().run(&request(&schema))).0;

        let fake = FakeClaude::new();
        fake.set(
            "out.json",
            &error_result(json!({"result": "Not logged in · Please run /login"})),
        );
        fake.set("exit", "1");
        assert!(matches!(run(&fake), Err(LlmError::Auth(_))));

        fake.set(
            "out.json",
            &error_result(
                json!({"api_error_status": 429, "api_error": "usage_limit_reached",
                "result": "You've hit your session limit · resets 3:45pm"}),
            ),
        );
        assert!(matches!(run(&fake), Err(LlmError::UsageLimit(ref m)) if m.contains("3:45pm")));

        fake.set(
            "out.json",
            &error_result(json!({"api_error": "max_output_tokens",
                "result": "API Error: Claude's response exceeded the 32000 output token maximum."})),
        );
        assert!(matches!(run(&fake), Err(LlmError::TooLong)));

        fake.set(
            "out.json",
            &error_result(json!({"api_error_status": 401,
                "result": "Invalid API key · Fix external API key"})),
        );
        assert!(matches!(run(&fake), Err(LlmError::Auth(ref m)) if m.contains("API key")));

        fake.set("out.json", "");
        fake.set("err.txt", "Error: something broke at startup\n");
        assert!(
            matches!(run(&fake), Err(LlmError::Unavailable(ref m)) if m.contains("something broke"))
        );

        fake.set("err.txt", "");
        assert!(
            matches!(run(&fake), Err(LlmError::Unavailable(ref m)) if m.contains("without a result"))
        );

        let gone = Invocation::new(fake.dir.path().join("missing"), fake.cwd());
        let (r, _) = rt().block_on(gone.run(&request(&schema)));
        assert!(matches!(r, Err(LlmError::Unavailable(ref m)) if m.contains("not installed")));
    }

    #[test]
    fn runs_only_in_the_app_owned_directory() {
        let schema = json!({"type": "object"});
        let fake = FakeClaude::new();
        fake.set("out.json", &success(json!({"segments": []}), 0.01));

        // A missing working directory is created, not replaced by the
        // directory the app was started from.
        let fresh = fake.dir.path().join("lib/claude-cwd");
        let inv = Invocation::new(fake.bin(), fresh.clone());
        assert!(rt().block_on(inv.run(&request(&schema))).0.is_ok());
        assert_eq!(
            fake.read("cwd.0").trim(),
            fresh.canonicalize().unwrap().display().to_string()
        );

        // One that cannot be created stops the run before Claude Code starts.
        let blocker = fake.dir.path().join("a-file");
        std::fs::write(&blocker, "").unwrap();
        let inv = Invocation::new(fake.bin(), blocker.join("claude-cwd"));
        let (r, _) = rt().block_on(inv.run(&request(&schema)));
        assert!(
            matches!(r, Err(LlmError::Unavailable(ref m)) if m.contains("working directory")),
            "{r:?}"
        );
        assert!(rt().block_on(inv.version()).is_err());
        assert_eq!(fake.calls(), 1, "no second run");
    }

    #[test]
    fn configured_paths_expand_the_home_directory() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                expand_home(Path::new("~/.local/bin/claude")),
                PathBuf::from(home).join(".local/bin/claude")
            );
        }
        assert_eq!(
            expand_home(Path::new("/opt/claude")),
            PathBuf::from("/opt/claude")
        );
        assert_eq!(expand_home(Path::new("~x/y")), PathBuf::from("~x/y"));
    }

    #[test]
    fn fake_binary_detection() {
        let fake = FakeClaude::new();
        let d = rt().block_on(detect(Some(&fake.bin()), &fake.cwd()));
        assert_eq!(d.version, Some(Version(2, 1, 310)));
        assert!(d.ready(), "{:?}", d.notes);
        // The isolation flag goes before the subcommand.
        let argv = split_nul(&fake.read("auth_argv"));
        assert_eq!(argv, ["--setting-sources=", "auth", "status"]);

        fake.set("version", "2.1.120 (Claude Code)\n");
        fake.set(
            "auth.json",
            r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#,
        );
        fake.set("auth_exit", "1");
        let d = rt().block_on(detect(Some(&fake.bin()), &fake.cwd()));
        assert!(!d.ready());
        assert!(d.notes.iter().any(|n| n.text.contains("too old")));
        assert!(d.notes.iter().any(|n| n.text.contains("Not signed in")));
        assert!(
            rt().block_on(fake.invocation().version()).unwrap() < MIN_VERSION,
            "version too old"
        );

        let missing = fake.dir.path().join("nope");
        let d = rt().block_on(detect(Some(&missing), &fake.cwd()));
        assert!(d.binary.is_none());
        assert!(d.notes[0].text.contains("not an executable"));
    }
}
