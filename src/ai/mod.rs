pub mod cli_providers;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::config::AppConfig;
use crate::model::{
    AiReviewResult, FileAnalysis, FileDiffRendered, HunkAnalysis, ReviewFinding, ReviewGroup,
    RiskLevel, Severity,
};
use crate::vcs::DiffBundle;

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub id: String,
    pub display_name: String,
    pub binary: String,
}

#[derive(Debug, Clone)]
pub struct AiPromptOutput {
    pub text: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AiProgressUpdate {
    Stage(String),
    ThinkingDelta(String),
    TextDelta(String),
    LogLine(String),
}

pub type AiProgressCallback = Arc<dyn Fn(AiProgressUpdate) + Send + Sync>;

pub trait AiProvider: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn run_prompt(&self, prompt: &str, work_dir: &Path, json_mode: bool) -> Result<String> {
        Ok(self
            .run_prompt_with_session(prompt, work_dir, json_mode, None, false)?
            .text)
    }
    fn run_prompt_with_session(
        &self,
        prompt: &str,
        work_dir: &Path,
        json_mode: bool,
        session_id: Option<&str>,
        persist_session: bool,
    ) -> Result<AiPromptOutput>;
}

pub fn detect_available_providers(cfg: &AppConfig) -> Vec<ProviderDescriptor> {
    let mut list = Vec::new();
    for (id, name, bin) in [
        ("agy", "Antigravity CLI (agy Local Sub)", "agy"),
        ("codex", "Codex CLI (Local Sub)", "codex"),
        ("claude", "Claude Code (Local Sub)", "claude"),
        ("gemini", "Gemini CLI (Local Sub)", "gemini"),
    ] {
        if cli_providers::binary_exists(bin) {
            list.push(ProviderDescriptor {
                id: id.to_string(),
                display_name: name.to_string(),
                binary: bin.to_string(),
            });
        }
    }
    for c in &cfg.custom_ai {
        if cli_providers::binary_exists(&c.command) {
            list.push(ProviderDescriptor {
                id: c.name.clone(),
                display_name: format!("{} (Custom CLI)", c.name),
                binary: c.command.clone(),
            });
        }
    }
    list
}

pub fn resolve_provider(
    preferred: &str,
    cfg: &AppConfig,
) -> Option<Box<dyn AiProvider>> {
    let pref = preferred.trim().to_ascii_lowercase();
    if pref == "none" || pref == "off" {
        return None;
    }

    if pref == "auto" {
        // Prefer providers with detected local auth credentials first
        let mut candidates = Vec::new();
        for id in ["agy", "codex", "claude", "gemini"] {
            if cli_providers::binary_exists(id) && cli_providers::has_auth_hint(id) {
                candidates.push(id);
            }
        }
        for id in ["agy", "codex", "claude", "gemini"] {
            if !candidates.contains(&id) && cli_providers::binary_exists(id) {
                candidates.push(id);
            }
        }
        for candidate in candidates {
            if let Some(p) = build_builtin_provider(candidate, cfg) {
                return Some(p);
            }
        }
        if let Some(first_custom) = cfg.custom_ai.first() {
            if cli_providers::binary_exists(&first_custom.command) {
                return Some(Box::new(cli_providers::CustomCliProvider {
                    name: first_custom.name.clone(),
                    command: first_custom.command.clone(),
                    args: first_custom.args.clone(),
                    timeout_secs: cfg.ai_timeout_secs,
                }));
            }
        }
        return None;
    }

    if let Some(p) = build_builtin_provider(&pref, cfg) {
        return Some(p);
    }

    for c in &cfg.custom_ai {
        if c.name.eq_ignore_ascii_case(preferred) && cli_providers::binary_exists(&c.command) {
            return Some(Box::new(cli_providers::CustomCliProvider {
                name: c.name.clone(),
                command: c.command.clone(),
                args: c.args.clone(),
                timeout_secs: cfg.ai_timeout_secs,
            }));
        }
    }

    None
}

fn build_builtin_provider(id: &str, cfg: &AppConfig) -> Option<Box<dyn AiProvider>> {
    match id {
        "agy" | "antigravity" if cli_providers::binary_exists("agy") => {
            Some(Box::new(cli_providers::AgyCliProvider {
                model: cfg.ai_model.clone(),
                effort: cfg.ai_effort.clone(),
                timeout_secs: cfg.ai_timeout_secs,
            }))
        }
        "claude" if cli_providers::binary_exists("claude") => {
            Some(Box::new(cli_providers::ClaudeCliProvider {
                model: cfg.ai_model.clone(),
                effort: cfg.ai_effort.clone(),
                timeout_secs: cfg.ai_timeout_secs,
            }))
        }
        "codex" if cli_providers::binary_exists("codex") => {
            Some(Box::new(cli_providers::CodexCliProvider {
                model: cfg.ai_model.clone(),
                effort: cfg.ai_effort.clone(),
                timeout_secs: cfg.ai_timeout_secs,
            }))
        }
        "gemini" if cli_providers::binary_exists("gemini") => {
            Some(Box::new(cli_providers::GeminiCliProvider {
                model: cfg.ai_model.clone(),
                timeout_secs: cfg.ai_timeout_secs,
            }))
        }
        _ => None,
    }
}

pub fn compute_diff_hash(
    provider_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    fast_model: Option<&str>,
    fast_effort: Option<&str>,
    mode_tag: &str,
    language: &str,
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"schema:v3\n");
    hasher.update(mode_tag.as_bytes());
    hasher.update(b"|");
    hasher.update(provider_id.as_bytes());
    hasher.update(b"|");
    hasher.update(model.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(effort.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(fast_model.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(fast_effort.unwrap_or("").as_bytes());
    hasher.update(b"\n");
    hasher.update(language.as_bytes());
    hasher.update(b"\n");
    hasher.update(bundle.target.to_string().as_bytes());
    hasher.update(b"\n");
    if let Some(ci) = &bundle.commit_info {
        hasher.update(ci.revision.as_bytes());
        hasher.update(b"|");
        hasher.update(ci.author.as_bytes());
        hasher.update(b"|");
        hasher.update(ci.message.as_bytes());
        hasher.update(b"\n");
    }
    // Hash full raw change payloads so edits beyond snippet truncation lines invalidate disk cache
    for c in &bundle.changes {
        hasher.update(c.path.as_bytes());
        hasher.update(b"|");
        hasher.update(c.old_path.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"|");
        hasher.update(c.kind.short_badge().as_bytes());
        hasher.update(b"|");
        hasher.update(if c.is_binary { b"1" } else { b"0" });
        hasher.update(b"|");
        if let Some(old_bytes) = &c.old_content {
            hasher.update((old_bytes.len() as u64).to_le_bytes());
            hasher.update(old_bytes);
        }
        hasher.update(b"|");
        if let Some(new_bytes) = &c.new_content {
            hasher.update((new_bytes.len() as u64).to_le_bytes());
            hasher.update(new_bytes);
        }
        hasher.update(b"|");
        if let Some(props) = &c.prop_changes {
            hasher.update(props.as_bytes());
        }
        hasher.update(b"\n");
    }
    for d in rendered_diffs {
        hasher.update(d.path.as_bytes());
        hasher.update(b":");
        hasher.update(d.ai_summary_diff.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

pub(crate) fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

/// Cap a prompt cleanly to `max_bytes` while preserving any trailing `--- REQUIRED OUTPUT FORMAT ---` section.
pub(crate) fn bound_prompt_preserving_tail(prompt: &str, max_bytes: usize) -> String {
    if prompt.len() <= max_bytes {
        return prompt.to_string();
    }
    const TAIL_MARKER: &str = "\n--- REQUIRED OUTPUT FORMAT ---";
    if let Some(pos) = prompt.rfind(TAIL_MARKER) {
        let tail = &prompt[pos..];
        let notice = "\n... [diff context truncated to fit prompt budget] ...\n";
        if tail.len() + notice.len() + 256 <= max_bytes {
            let head_budget = max_bytes - tail.len() - notice.len();
            let head = truncate_utf8_bytes(&prompt[..pos], head_budget);
            return format!("{}{}{}", head, notice, tail);
        }
    }
    truncate_utf8_bytes(prompt, max_bytes).to_string()
}

static PROVIDER_SESSION_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

thread_local! {
    static ACTIVE_CANCEL_FLAG: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    static ACTIVE_PROGRESS_CB: RefCell<Option<AiProgressCallback>> = const { RefCell::new(None) };
    static ACTIVE_ALLOW_TOOLS: Cell<bool> = const { Cell::new(false) };
}

pub fn run_with_cancel_flag<T>(flag: Arc<AtomicBool>, f: impl FnOnce() -> T) -> T {
    run_with_ai_hooks(flag, None, f)
}

pub fn run_with_ai_hooks<T>(
    flag: Arc<AtomicBool>,
    progress_cb: Option<AiProgressCallback>,
    f: impl FnOnce() -> T,
) -> T {
    struct Guard {
        prev_cancel: Option<Arc<AtomicBool>>,
        prev_progress: Option<AiProgressCallback>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let prev_cancel = self.prev_cancel.take();
            let prev_progress = self.prev_progress.take();
            ACTIVE_CANCEL_FLAG.with(|cell| {
                *cell.borrow_mut() = prev_cancel;
            });
            ACTIVE_PROGRESS_CB.with(|cell| {
                *cell.borrow_mut() = prev_progress;
            });
        }
    }
    let prev_cancel = ACTIVE_CANCEL_FLAG.with(|cell| cell.borrow_mut().replace(flag));
    let prev_progress =
        ACTIVE_PROGRESS_CB.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), progress_cb));
    let _guard = Guard {
        prev_cancel,
        prev_progress,
    };
    f()
}

pub(crate) fn is_current_task_cancelled() -> bool {
    ACTIVE_CANCEL_FLAG.with(|cell| {
        cell.borrow()
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    })
}

pub(crate) fn has_progress_hook() -> bool {
    ACTIVE_PROGRESS_CB.with(|cell| cell.borrow().is_some())
}

fn run_with_allow_tools<T>(allow: bool, f: impl FnOnce() -> T) -> T {
    struct Guard(bool);
    impl Drop for Guard {
        fn drop(&mut self) {
            ACTIVE_ALLOW_TOOLS.with(|cell| cell.set(self.0));
        }
    }
    let prev = ACTIVE_ALLOW_TOOLS.with(|cell| cell.replace(allow));
    let _guard = Guard(prev);
    f()
}

pub(crate) fn current_allow_tools() -> bool {
    ACTIVE_ALLOW_TOOLS.with(|cell| cell.get())
}

pub(crate) fn emit_progress(update: AiProgressUpdate) {
    if is_current_task_cancelled() {
        return;
    }
    let cb = ACTIVE_PROGRESS_CB.with(|cell| cell.borrow().clone());
    if let Some(cb) = cb {
        cb(update);
    }
}

fn provider_session_lock(provider_id: &str) -> Arc<Mutex<()>> {
    let map = PROVIDER_SESSION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(provider_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn is_non_recoverable_cli_error(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err).to_ascii_lowercase();
    [
        "timed out",
        "cancelled",
        "canceled",
        "failed to spawn",
        "not logged in",
        "unauthorized",
        "authentication",
        "forbidden",
        "rate limit",
        "http 401",
        "http 403",
        "status 401",
        "status: 401",
        "status 403",
        "status: 403",
        "error 401",
        "error 403",
        "401 unauthorized",
        "403 forbidden",
        "e2big",
        "argument list too long",
    ]
    .iter()
    .any(|kw| msg.contains(kw))
}

/// Run prompt reusing an existing session if enabled, with automatic fallback to a fresh
/// session ONLY when resuming a stale/deleted session fails. Serialized per provider ID ONLY
/// when `reuse_session` is true so stateless calls never block behind stateful background runs.
fn invoke_with_session(
    provider: &dyn AiProvider,
    prompt: &str,
    work_dir: &Path,
    json_mode: bool,
    cfg: &AppConfig,
    allow_tools: bool,
) -> Result<AiPromptOutput> {
    run_with_allow_tools(allow_tools, || {
        invoke_with_session_inner(provider, prompt, work_dir, json_mode, cfg)
    })
}

fn invoke_with_session_inner(
    provider: &dyn AiProvider,
    prompt: &str,
    work_dir: &Path,
    json_mode: bool,
    cfg: &AppConfig,
) -> Result<AiPromptOutput> {
    if is_current_task_cancelled() {
        return Err(anyhow!("AI request cancelled"));
    }

    let session_lock = cfg
        .reuse_session
        .then(|| provider_session_lock(provider.id()));
    let _session_guard = session_lock
        .as_ref()
        .map(|lock| lock.lock().unwrap_or_else(|e| e.into_inner()));

    if is_current_task_cancelled() {
        return Err(anyhow!("AI request cancelled"));
    }

    let saved_sid = if cfg.reuse_session {
        cfg.ai_sessions.get(provider.id()).map(|s| s.as_str())
    } else {
        None
    };

    let locale = crate::i18n::current_thread_locale();
    let mut fell_back = false;
    if let Some(sid) = saved_sid {
        let short_sid: String = sid.chars().take(8).collect();
        emit_progress(AiProgressUpdate::Stage(
            rust_i18n::t!(
                "ai.reusing_session",
                locale = locale.as_str(),
                sid = short_sid,
                provider = provider.id()
            )
            .into_owned(),
        ));
        match provider.run_prompt_with_session(
            prompt,
            work_dir,
            json_mode,
            Some(sid),
            cfg.reuse_session,
        ) {
            Ok(out) if !out.text.trim().is_empty() => {
                return Ok(AiPromptOutput {
                    text: out.text,
                    session_id: out.session_id.or_else(|| Some(sid.to_string())),
                });
            }
            Ok(_) => {
                fell_back = true;
            }
            Err(err) if is_non_recoverable_cli_error(&err) => {
                return Err(err);
            }
            Err(_) => {
                fell_back = true;
            }
        }
    }

    if is_current_task_cancelled() {
        return Err(anyhow!("AI request cancelled"));
    }

    if fell_back {
        emit_progress(AiProgressUpdate::Stage(
            rust_i18n::t!(
                "ai.session_expired",
                locale = locale.as_str(),
                provider = provider.id()
            )
            .into_owned(),
        ));
    } else {
        emit_progress(AiProgressUpdate::Stage(
            rust_i18n::t!(
                "ai.starting_session",
                locale = locale.as_str(),
                provider = provider.id()
            )
            .into_owned(),
        ));
    }

    provider.run_prompt_with_session(prompt, work_dir, json_mode, None, cfg.reuse_session)
}

#[derive(Debug, Clone)]
pub struct AnalyzeDiffOutput {
    pub review: AiReviewResult,
    pub session_id: Option<String>,
}

pub fn analyze_diff(
    provider: &dyn AiProvider,
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
    cfg: &AppConfig,
    bypass_cache: bool,
) -> Result<AiReviewResult> {
    Ok(analyze_diff_with_session(provider, bundle, rendered_diffs, cfg, bypass_cache)?.review)
}

pub fn analyze_diff_with_session(
    provider: &dyn AiProvider,
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
    cfg: &AppConfig,
    bypass_cache: bool,
) -> Result<AnalyzeDiffOutput> {
    let hash = compute_diff_hash(
        provider.id(),
        cfg.ai_model.as_deref(),
        cfg.ai_effort.as_deref(),
        None,
        None,
        "single",
        &cfg.review_language,
        bundle,
        rendered_diffs,
    );
    let locale = crate::i18n::current_thread_locale();
    if cfg.cache_ai_results && !bypass_cache {
        emit_progress(AiProgressUpdate::Stage(
            rust_i18n::t!("ai.checking_cache", locale = locale.as_str()).into_owned(),
        ));
        if let Some(cache_dir) = AppConfig::cache_dir() {
            let cache_file = cache_dir.join(format!("{}.json", hash));
            if cache_file.exists() {
                if let Ok(raw) = std::fs::read_to_string(&cache_file) {
                    if let Ok(cached) = serde_json::from_str::<AiReviewResult>(&raw) {
                        if has_meaningful_review_content(&cached) {
                            emit_progress(AiProgressUpdate::Stage(
                                rust_i18n::t!("ai.cache_hit", locale = locale.as_str())
                                    .into_owned(),
                            ));
                            return Ok(AnalyzeDiffOutput {
                                review: cached,
                                session_id: cfg.ai_sessions.get(provider.id()).cloned(),
                            });
                        }
                    }
                }
            }
        }
    }

    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!(
            "ai.preparing_diffs",
            locale = locale.as_str(),
            count = rendered_diffs.len()
        )
        .into_owned(),
    ));
    let prompt = build_review_prompt(bundle, rendered_diffs, &cfg.review_language);
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!(
            "ai.submitting_review",
            locale = locale.as_str(),
            kb = format!("{:.1}", prompt.len() as f64 / 1024.0),
            provider = provider.id()
        )
        .into_owned(),
    ));
    let out = invoke_with_session(provider, &prompt, &bundle.repo_root, true, cfg, false)?;
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!("ai.parsing_response", locale = locale.as_str()).into_owned(),
    ));
    let mut parsed = parse_ai_review_json(&out.text, provider.display_name(), rendered_diffs)
        .with_context(|| {
            format!(
                "Failed to parse structured JSON review from {}",
                provider.display_name()
            )
        })?;
    if !has_meaningful_review_content(&parsed) {
        anyhow::bail!(
            "AI provider {} returned an empty review JSON object",
            provider.display_name()
        );
    }

    ensure_all_files_grouped(&mut parsed, rendered_diffs);
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!(
            "ai.done_summary",
            locale = locale.as_str(),
            groups = parsed.groups.len(),
            findings = parsed.findings.len()
        )
        .into_owned(),
    ));

    save_ai_review_cache(&hash, cfg.cache_ai_results, &parsed);

    Ok(AnalyzeDiffOutput {
        review: parsed,
        session_id: out.session_id,
    })
}

/// Two-stage review optimized for TUI responsiveness:
/// Stage 1 runs a fast stateless diff-only pass (`fast_provider` + `fast_config()`) for
/// `groups + summary` and triggers `on_groups_ready`.
/// Stage 2 runs a fresh deep session for core `findings` + key file/hunk notes and caches the result.
pub fn analyze_diff_two_stage<F>(
    fast_provider: &dyn AiProvider,
    deep_provider: &dyn AiProvider,
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
    cfg: &AppConfig,
    bypass_cache: bool,
    on_groups_ready: F,
) -> Result<AnalyzeDiffOutput>
where
    F: FnOnce(&AiReviewResult, Option<&str>),
{
    let fast_cfg = cfg.fast_config();
    let hash = compute_diff_hash(
        deep_provider.id(),
        cfg.ai_model.as_deref(),
        cfg.ai_effort.as_deref(),
        fast_cfg.ai_model.as_deref(),
        fast_cfg.ai_effort.as_deref(),
        &format!("two_stage:{}", fast_provider.id()),
        &cfg.review_language,
        bundle,
        rendered_diffs,
    );
    let locale = crate::i18n::current_thread_locale();
    if cfg.cache_ai_results && !bypass_cache {
        emit_progress(AiProgressUpdate::Stage(
            rust_i18n::t!("ai.checking_cache", locale = locale.as_str()).into_owned(),
        ));
        if let Some(cache_dir) = AppConfig::cache_dir() {
            let cache_file = cache_dir.join(format!("{}.json", hash));
            if cache_file.exists() {
                if let Ok(raw) = std::fs::read_to_string(&cache_file) {
                    if let Ok(cached) = serde_json::from_str::<AiReviewResult>(&raw) {
                        if has_meaningful_review_content(&cached) {
                            emit_progress(AiProgressUpdate::Stage(
                                rust_i18n::t!("ai.cache_hit", locale = locale.as_str())
                                    .into_owned(),
                            ));
                            return Ok(AnalyzeDiffOutput {
                                review: cached,
                                session_id: cfg.ai_sessions.get(deep_provider.id()).cloned(),
                            });
                        }
                    }
                }
            }
        }
    }

    // Stage 1: Stateless fast diff-only grouping + summary (no old session bloat)
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!(
            "ai.preparing_diffs",
            locale = locale.as_str(),
            count = rendered_diffs.len()
        )
        .into_owned(),
    ));
    let group_prompt = build_grouping_prompt(bundle, rendered_diffs, &fast_cfg.review_language);
    let fast_label = fast_cfg
        .ai_model
        .as_deref()
        .unwrap_or_else(|| fast_provider.id());
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!(
            "ai.fast_grouping",
            locale = locale.as_str(),
            kb = format!("{:.1}", group_prompt.len() as f64 / 1024.0),
            provider = fast_label
        )
        .into_owned(),
    ));
    let out1 = invoke_with_session(
        fast_provider,
        &group_prompt,
        &bundle.repo_root,
        true,
        &fast_cfg,
        false,
    )?;
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!("ai.parsing_groups", locale = locale.as_str()).into_owned(),
    ));
    let mut stage1 =
        parse_ai_review_json(&out1.text, fast_provider.display_name(), rendered_diffs)
            .with_context(|| {
                format!(
                    "Failed to parse Stage 1 grouping JSON from {}",
                    fast_provider.id()
                )
            })?;
    if stage1.summary.trim().is_empty() && stage1.groups.is_empty() {
        anyhow::bail!(
            "Stage 1 provider {} returned empty summary and groups",
            fast_provider.display_name()
        );
    }
    ensure_all_files_grouped(&mut stage1, rendered_diffs);

    // Do not overwrite persisted deep/followup session with Stage 1's stateless call
    on_groups_ready(&stage1, None);

    if is_current_task_cancelled() {
        return Err(anyhow!("AI request cancelled"));
    }

    // Stage 2: Fresh deep session for core findings + key file/hunk notes; persisted for followups
    let mut deep_cfg = cfg.clone();
    deep_cfg.ai_sessions.clear();
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!("ai.analyzing_risks", locale = locale.as_str()).into_owned(),
    ));
    let findings_prompt =
        build_findings_prompt(bundle, rendered_diffs, &cfg.review_language, &stage1);
    let run_stage2 = |prompt_text: &str, allow_tools: bool| -> Result<(AiReviewResult, Option<String>)> {
        let out = invoke_with_session(
            deep_provider,
            prompt_text,
            &bundle.repo_root,
            true,
            &deep_cfg,
            allow_tools,
        )?;
        emit_progress(AiProgressUpdate::Stage(
            rust_i18n::t!("ai.merging_findings", locale = locale.as_str()).into_owned(),
        ));
        let parsed = parse_ai_review_json(&out.text, deep_provider.display_name(), rendered_diffs)
            .with_context(|| {
                let preview: String = out
                    .text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(80)
                    .collect();
                format!(
                    "Stage 2 JSON parse failed ({}): {}",
                    deep_provider.id(),
                    preview
                )
            })?;
        Ok((parsed, out.session_id))
    };

    let (stage2, stage2_sid) = match run_stage2(&findings_prompt, true) {
        Ok(res) => res,
        Err(first_err) if !is_current_task_cancelled() => {
            let msg = format!("{:#}", first_err);
            if msg.contains("returned no text") || msg.contains("Stage 2 JSON parse failed") {
                let no_tools_prompt = format!(
                    "{}\n\nIMPORTANT: Do NOT call any tools, inspect workspace files, or run commands. Produce the JSON response purely from the provided diffs above.",
                    findings_prompt
                );
                run_stage2(&no_tools_prompt, false).or(Err(first_err))?
            } else {
                return Err(first_err);
            }
        }
        Err(e) => return Err(e),
    };

    stage1.overall_risk = stage1.overall_risk.max_risk(stage2.overall_risk);
    if !stage2.files.is_empty() {
        stage1.files = stage2.files;
    }
    if !stage2.hunks.is_empty() {
        stage1.hunks = stage2.hunks;
    }
    if !stage2.findings.is_empty() {
        stage1.findings = stage2.findings;
    }

    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!(
            "ai.done_summary",
            locale = locale.as_str(),
            groups = stage1.groups.len(),
            findings = stage1.findings.len()
        )
        .into_owned(),
    ));
    save_ai_review_cache(&hash, cfg.cache_ai_results, &stage1);

    Ok(AnalyzeDiffOutput {
        review: stage1,
        session_id: stage2_sid,
    })
}

fn save_ai_review_cache(hash: &str, enabled: bool, review: &AiReviewResult) {
    if !enabled {
        return;
    }
    if let Some(cache_dir) = AppConfig::cache_dir() {
        let cache_file = cache_dir.join(format!("{}.json", hash));
        if let Ok(json_bytes) = serde_json::to_vec_pretty(review) {
            let _ = crate::vcs::atomic_write_cache(
                &cache_file,
                &json_bytes,
                256,
                128 * 1024 * 1024,
            );
        }
    }
}

pub fn ask_followup(
    provider: &dyn AiProvider,
    bundle: &DiffBundle,
    file_diff: Option<&FileDiffRendered>,
    question: &str,
    cfg: &AppConfig,
) -> Result<AiPromptOutput> {
    let mut prompt = format!(
        "You are a senior code reviewer assisting in a terminal code review session.\n\
         Respond concisely in {} using clean plain text / markdown (strictly read-only: do NOT edit files or run state-changing commands; you may use read-only file/search tools if needed to verify surrounding context).\n\
         VCS: {}, Target: {}\n",
        cfg.review_language, bundle.vcs, bundle.target
    );
    if let Some(info) = &bundle.commit_info {
        prompt.push_str(&format!(
            "Commit {}: {} (by {})\n",
            info.revision, info.message, info.author
        ));
    }
    if let Some(fd) = file_diff {
        let snippet: String = fd.ai_summary_diff.chars().take(18_000).collect();
        prompt.push_str(&format!("\nFocused file structural diff (via Difftastic):\n{}\n", snippet));
    }
    prompt.push_str(&format!("\nReviewer question:\n{}\n", question));

    invoke_with_session(provider, &prompt, &bundle.repo_root, false, cfg, true)
}

fn append_diff_context(
    prompt: &mut String,
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
) {
    if let Some(ci) = &bundle.commit_info {
        let msg = truncate_utf8_bytes(&ci.message, 4_000);
        prompt.push_str(&format!(
            "Commit Revision: {}\nAuthor: {}\nDate: {}\nCommit Message:\n{}\n\n",
            ci.revision, ci.author, ci.date, msg
        ));
    }

    prompt.push_str(&format!("Changed Files ({} total):\n", bundle.changes.len()));
    const MAX_FILES_LIST_BYTES: usize = 12_000;
    let mut files_bytes = 0usize;
    let mut listed = 0usize;
    for c in &bundle.changes {
        let line = format!("- [{}] {}\n", c.kind.short_badge(), c.path);
        if files_bytes + line.len() > MAX_FILES_LIST_BYTES {
            let omitted = bundle.changes.len().saturating_sub(listed);
            prompt.push_str(&format!("- ... ({} more files omitted)\n", omitted));
            break;
        }
        files_bytes += line.len();
        listed += 1;
        prompt.push_str(&line);
    }

    prompt.push_str("\n--- STRUCTURAL DIFFS (generated by Difftastic) ---\n");
    const MAX_PROMPT_DIFF_BYTES: usize = 64_000;
    let mut used_bytes = 0usize;

    for (idx, d) in rendered_diffs.iter().enumerate() {
        if used_bytes >= MAX_PROMPT_DIFF_BYTES {
            let omitted = rendered_diffs.len().saturating_sub(idx);
            prompt.push_str(&format!(
                "\n... ({} remaining file diffs omitted due to prompt size budget) ...\n",
                omitted
            ));
            break;
        }
        let remaining = MAX_PROMPT_DIFF_BYTES.saturating_sub(used_bytes);
        let max_slice = remaining.min(12_000);
        let chunk: String = if d.ai_summary_diff.len() > max_slice {
            let mut s = truncate_utf8_bytes(&d.ai_summary_diff, max_slice).to_string();
            s.push_str("\n... [truncated] ...\n");
            s
        } else {
            d.ai_summary_diff.clone()
        };
        used_bytes += chunk.len();
        prompt.push_str(&chunk);
        prompt.push('\n');
    }
}

pub fn build_grouping_prompt(
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
    language: &str,
) -> String {
    let mut prompt = format!(
        "You are a senior software engineer performing Stage 1 (Fast Grouping & Summary) of a Code Review.\n\
         Analyze ONLY the provided Difftastic structural diffs below and return a compact JSON object.\n\
         Do NOT call any tools, read external files, run commands, or edit files.\n\
         Do NOT generate per-file, per-hunk, or detailed findings yet (focus strictly on accurate semantic groups + summary so response stays fast).\n\
         Write all human-readable descriptions in language: {}.\n\n\
         Repository VCS: {}\n\
         Diff Target: {}\n",
        language, bundle.vcs, bundle.target
    );
    append_diff_context(&mut prompt, bundle, rendered_diffs);

    let (title_hint, desc_hint) = if language.trim().to_ascii_lowercase().starts_with("zh") {
        (
            "简明重点标题 (<= 16字)",
            "一句话核心目的 (<= 45字)",
        )
    } else {
        (
            "Concise focused title (<= 6 words)",
            "One-sentence core purpose (<= 15 words)",
        )
    };
    prompt.push_str(&format!(
        r#"
--- REQUIRED OUTPUT FORMAT ---
Return ONLY a single valid JSON object (no markdown commentary outside JSON):
{{
  "summary": "2-3 sentence summary of the overall change intent and impact",
  "overall_risk": "critical | high | medium | low | info",
  "review_strategy": "Recommended review order in 1 short sentence",
  "groups": [
    {{
      "id": "g1",
      "title": "{title_hint}",
      "description": "{desc_hint}",
      "category": "feature | bugfix | refactor | security | performance | test | config | docs | chore",
      "risk_level": "critical | high | medium | low | info",
      "review_order": 1,
      "files": ["exact/file/path.rs"],
      "key_points": ["Key thing to verify"]
    }}
  ]
}}

Rules:
1. Every changed file MUST appear in at least one group in `groups` using its exact relative path.
2. Order `groups` by `review_order` (1 = review first). Keep titles short and descriptions concise.
3. Output valid JSON only.
"#
    ));
    prompt
}

pub fn build_findings_prompt(
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
    language: &str,
    grouping: &AiReviewResult,
) -> String {
    let mut prompt = format!(
        "You are a senior software engineer performing Stage 2 (Deep Core Findings & Key Analysis) of a Code Review.\n\
         Strictly READ-ONLY: do NOT edit files, stage/commit changes, or run state-changing commands.\n\
         You may inspect workspace files ONLY via read-only file/search tools (e.g. view_file/read_file/grep_search) if necessary to verify suspicious bugs, callers, or edge cases; NEVER call run_command, browser tools, write/edit tools, or ask_question.\n\
         Focus on concrete bugs, concurrency/security issues, edge cases, and high-value hunk/file notes. Omit trivial or obvious boilerplate.\n\
         Write all human-readable descriptions in language: {}.\n\n\
         Repository VCS: {}\n\
         Diff Target: {}\n",
        language, bundle.vcs, bundle.target
    );

    if !grouping.groups.is_empty() {
        prompt.push_str("\nStage 1 Groups (already displayed to user; do NOT repeat `groups`):\n");
        const MAX_STAGE1_GROUP_BYTES: usize = 8_000;
        let mut group_bytes = 0usize;
        for (idx, g) in grouping.groups.iter().enumerate() {
            let line = format!("- {}: {} ({})\n", g.id, g.title, g.files.join(", "));
            let bounded_line = truncate_utf8_bytes(&line, 1_000);
            if group_bytes + bounded_line.len() > MAX_STAGE1_GROUP_BYTES {
                prompt.push_str(&format!(
                    "- ... ({} more Stage 1 groups omitted)\n",
                    grouping.groups.len().saturating_sub(idx)
                ));
                break;
            }
            group_bytes += bounded_line.len();
            prompt.push_str(bounded_line);
            if !bounded_line.ends_with('\n') {
                prompt.push('\n');
            }
        }
        prompt.push('\n');
    }

    append_diff_context(&mut prompt, bundle, rendered_diffs);

    prompt.push_str(
        r#"
--- REQUIRED OUTPUT FORMAT ---
Return ONLY a single valid JSON object (no markdown commentary outside JSON):
{
  "overall_risk": "critical | high | medium | low | info",
  "files": [
    {
      "path": "exact/file/path.rs",
      "category": "feature | bugfix | refactor | security | performance | test | config | docs | chore",
      "risk_level": "critical | high | medium | low | info",
      "summary": "Concise 1-sentence summary for non-trivial files",
      "review_focus": ["Specific check for this file"]
    }
  ],
  "hunks": [
    {
      "file": "exact/file/path.rs",
      "chunk_index": 1,
      "start_line": 40,
      "end_line": 55,
      "title": "Short title for key hunk",
      "analysis": "Concise explanation of subtle logic, edge cases, or traps (skip trivial chunks)",
      "risk_level": "critical | high | medium | low | info"
    }
  ],
  "findings": [
    {
      "id": "f1",
      "file": "exact/file/path.rs",
      "line": 42,
      "severity": "critical | warning | suggestion | info | praise",
      "title": "Short issue or observation title",
      "description": "Clear explanation of the bug, edge case, concurrency/security risk, or praise",
      "suggestion": "Concrete fix or verification step (optional)"
    }
  ]
}

Rules:
1. Prioritize high-signal `findings` (real bugs, regressions, edge cases, security/perf issues) and key `hunks`; omit boilerplate.
2. Only cite files, lines, and symbols directly observed in the diffs or inspected via read-only tools.
3. Output valid JSON only.
"#,
    );
    prompt
}

pub fn build_review_prompt(
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
    language: &str,
) -> String {
    let mut prompt = format!(
        "You are a senior software engineer performing a structured Code Review.\n\
         Analyze ONLY the provided Difftastic structural diffs below to group changes and summarize findings in a single fast response.\n\
         Do NOT call any tools, read external files, run commands, or edit files.\n\
         Write all human-readable descriptions in language: {}.\n\n\
         Repository VCS: {}\n\
         Diff Target: {}\n",
        language, bundle.vcs, bundle.target
    );

    append_diff_context(&mut prompt, bundle, rendered_diffs);

    let (title_hint, desc_hint) = if language.trim().to_ascii_lowercase().starts_with("zh") {
        (
            "简明重点标题 (<= 16字，如: 世界地图动态配置)",
            "一句话简短描述这一组改动的核心目的 (<= 45字)",
        )
    } else {
        (
            "Concise focused title (<= 6 words, e.g. Dynamic world map config)",
            "One-sentence description of this group's core purpose (<= 15 words)",
        )
    };
    prompt.push_str(&format!(
        r#"
--- REQUIRED OUTPUT FORMAT ---
Return ONLY a single valid JSON object (no markdown commentary outside JSON) with this exact structure:
{{
  "summary": "2-4 sentence executive summary of the overall change intent and impact",
  "overall_risk": "critical | high | medium | low | info",
  "review_strategy": "Recommended order and focus areas for the human reviewer",
  "groups": [
    {{
      "id": "g1",
      "title": "{title_hint}",
      "description": "{desc_hint}",
      "category": "feature | bugfix | refactor | security | performance | test | config | docs | chore",
      "risk_level": "critical | high | medium | low | info",
      "review_order": 1,
      "files": ["exact/file/path.rs"],
      "key_points": ["Key thing to verify while reviewing this group"]
    }}
  ],
  "files": [
    {{
      "path": "exact/file/path.rs",
      "category": "feature | bugfix | refactor | security | performance | test | config | docs | chore",
      "risk_level": "critical | high | medium | low | info",
      "summary": "One-sentence summary of changes in this file",
      "review_focus": ["Specific check for this file"]
    }}
  ],
  "hunks": [
    {{
      "file": "exact/file/path.rs",
      "chunk_index": 1,
      "start_line": 40,
      "end_line": 55,
      "title": "Short title for this diff paragraph/hunk",
      "analysis": "Concise explanation of what this specific diff chunk does, intent, edge cases, and what to double-check",
      "risk_level": "critical | high | medium | low | info"
    }}
  ],
  "findings": [
    {{
      "id": "f1",
      "file": "exact/file/path.rs",
      "line": 42,
      "severity": "critical | warning | suggestion | info | praise",
      "title": "Short issue or observation title",
      "description": "Clear explanation of the bug, edge case, concurrency/security risk, or praise",
      "suggestion": "Concrete fix or verification step (optional)"
    }}
  ]
}}

Rules:
1. Every changed file MUST appear in at least one group in `groups` using its exact relative path from the list above.
2. Group files semantically by review focus and order `groups` by `review_order` (1 = review first). Keep each group `title` short and scannable, and `description` concise (one short sentence).
3. Include a `hunks` entry for each major `@@ Chunk N` in the diffs so the reviewer sees paragraph-level analysis while scrolling through code.
4. Only cite lines and symbols visible in the provided diff; do not invent unshown code.
5. Output valid JSON only.
"#
    ));

    prompt
}

/// Iterate candidate balanced `{ ... }` slices inside arbitrary text in a single linear pass,
/// avoiding O(N²) re-scanning on both nested JSON objects and unclosed `{` sequences.
struct JsonObjectScanner<'a> {
    s: &'a str,
    pos: usize,
    pending: std::collections::VecDeque<(&'a str, serde_json::Value)>,
    remaining_inner_budget: usize,
}

impl<'a> JsonObjectScanner<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            s,
            pos: 0,
            pending: std::collections::VecDeque::new(),
            remaining_inner_budget: s.len().saturating_mul(4).max(8192),
        }
    }

    fn queue_valid_inner_spans(
        &mut self,
        inner_valid: Vec<(usize, usize)>,
        fragment_len: usize,
    ) {
        // Bound cumulative bytes parsed per fragment and across the whole input so `d` sequential
        // invalid fragments cannot re-spend full-input budget (`Θ(d³)`), while using `continue`
        // so oversized invalid wrappers do not skip smaller valid siblings later in the fragment.
        let mut frag_budget = fragment_len
            .saturating_mul(2)
            .max(512)
            .min(self.remaining_inner_budget);
        let mut kept: Vec<(usize, usize, serde_json::Value)> = Vec::new();
        for (is, ie) in inner_valid {
            let span_len = ie.saturating_sub(is);
            if span_len > frag_budget || span_len > self.remaining_inner_budget {
                continue;
            }
            frag_budget -= span_len;
            self.remaining_inner_budget -= span_len;
            if let Some(val) = parse_json_lenient(&self.s[is..ie]) {
                while kept
                    .last()
                    .is_some_and(|(ks, ke, _)| *ks >= is && *ke <= ie)
                {
                    kept.pop();
                }
                kept.push((is, ie, val));
            }
        }
        for (is, ie, val) in kept {
            self.pending.push_back((&self.s[is..ie], val));
        }
    }

    fn next_object(&mut self) -> Option<(&'a str, serde_json::Value)> {
        if let Some(item) = self.pending.pop_front() {
            return Some(item);
        }
        while self.pos < self.s.len() {
            let rel_open = self.s[self.pos..].find('{')?;
            let start = self.pos + rel_open;
            let mut stack = vec![start];
            let mut in_string = false;
            let mut escape = false;
            let mut inner_valid: Vec<(usize, usize)> = Vec::new();
            let mut top_result: Option<(usize, serde_json::Value)> = None;
            let mut balanced_end: Option<usize> = None;

            for (rel_idx, c) in self.s[start + 1..].char_indices() {
                let abs_idx = start + 1 + rel_idx;
                if in_string {
                    if escape {
                        escape = false;
                    } else if c == '\\' {
                        escape = true;
                    } else if c == '"' {
                        in_string = false;
                    }
                    continue;
                }
                match c {
                    '"' => in_string = true,
                    '{' => stack.push(abs_idx),
                    '}' => {
                        if let Some(open_idx) = stack.pop() {
                            let end_idx = abs_idx + c.len_utf8();
                            if stack.is_empty() {
                                balanced_end = Some(end_idx);
                                if let Some(val) = parse_json_lenient(&self.s[open_idx..end_idx]) {
                                    top_result = Some((end_idx, val));
                                }
                                break;
                            } else {
                                inner_valid.push((open_idx, end_idx));
                            }
                        }
                    }
                    _ => {}
                }
            }

            if let Some((end_idx, val)) = top_result {
                self.pos = end_idx;
                return Some((&self.s[start..end_idx], val));
            }
            if let Some(end_idx) = balanced_end {
                // Balanced top-level braces failed JSON parse; jump strictly past `end_idx` and
                // inspect the already-delimited inner spans within the fragment's byte budget.
                self.pos = end_idx;
                self.queue_valid_inner_spans(inner_valid, end_idx.saturating_sub(start));
                if let Some(item) = self.pending.pop_front() {
                    return Some(item);
                }
                continue;
            }
            // Hit EOF with unclosed `{` on stack: consume to end in one pass while surfacing
            // any balanced inner JSON objects cleanly.
            let rem_len = self.s.len().saturating_sub(start);
            self.pos = self.s.len();
            self.queue_valid_inner_spans(inner_valid, rem_len);
            if let Some(item) = self.pending.pop_front() {
                return Some(item);
            }
        }
        None
    }
}

fn scan_balanced_json_object(s: &str) -> Option<&str> {
    JsonObjectScanner::new(s).next_object().map(|(slice, _)| slice)
}

/// Remove trailing commas before `}` or `]` outside JSON string literals.
fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    let mut in_string = false;
    let mut escape = false;

    while let Some((idx, c)) = chars.next() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' {
            let rest = &s[idx + 1..];
            let next_non_ws = rest.chars().find(|ch| !ch.is_ascii_whitespace());
            if !matches!(next_non_ws, Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_json_lenient(s: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        return Some(v);
    }
    if !s.contains(',') {
        return None;
    }
    let cleaned = strip_trailing_commas(s);
    serde_json::from_str::<serde_json::Value>(&cleaned).ok()
}

pub fn extract_json_block(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') && parse_json_lenient(trimmed).is_some() {
        return Some(trimmed);
    }

    // Look for fenced code blocks (case-insensitive ```json or plain ```), preferring outer fences
    // and validating balanced JSON objects so nested backticks inside string fields do not truncate.
    let lower = trimmed.to_ascii_lowercase();
    for marker in ["```json", "```"] {
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(marker) {
            let after_marker = search_from + rel + marker.len();
            let slice = trimmed[after_marker..].trim_start();
            if let Some(last_fence) = slice.rfind("```") {
                let inner = slice[..last_fence].trim();
                if let Some(obj) = scan_balanced_json_object(inner) {
                    return Some(obj);
                }
            }
            if let Some(obj) = scan_balanced_json_object(slice) {
                return Some(obj);
            }
            search_from = after_marker;
        }
    }

    if let Some(obj) = scan_balanced_json_object(trimmed) {
        return Some(obj);
    }

    let first = trimmed.find('{')?;
    let last = trimmed.rfind('}')?;
    if last > first {
        Some(&trimmed[first..=last])
    } else {
        None
    }
}

fn has_meaningful_review_content(review: &AiReviewResult) -> bool {
    !review.summary.trim().is_empty()
        || !review.groups.is_empty()
        || !review.files.is_empty()
        || !review.hunks.is_empty()
        || !review.findings.is_empty()
}

fn is_review_json_object(val: &serde_json::Value) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    const EXPECTED_KEYS: [&str; 7] = [
        "summary",
        "overall_risk",
        "review_strategy",
        "groups",
        "files",
        "hunks",
        "findings",
    ];
    EXPECTED_KEYS.iter().any(|k| obj.contains_key(*k))
}

fn unwrap_review_object<'a>(val: &'a serde_json::Value) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    if is_review_json_object(val) {
        return val.as_object();
    }
    // Search wrapper keys recursively (e.g. `data.result`) as well as any nested review objects.
    let mut best: Option<(usize, &'a serde_json::Map<String, serde_json::Value>)> = None;
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(val);
    while let Some(cur) = queue.pop_front() {
        match cur {
            serde_json::Value::Object(obj) => {
                if is_review_json_object(cur) {
                    let score = review_object_richness_score(obj, 0);
                    if best.is_none_or(|(b, _)| score > b) {
                        best = Some((score, obj));
                    }
                    continue;
                }
                for key in ["review", "data", "result", "response"] {
                    if let Some(inner) = obj.get(key) {
                        queue.push_back(inner);
                    }
                }
                for (k, v) in obj {
                    if !matches!(k.as_str(), "review" | "data" | "result" | "response") {
                        queue.push_back(v);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    queue.push_back(item);
                }
            }
            _ => {}
        }
    }
    best.map(|(_, obj)| obj)
}

fn review_object_richness_score(obj: &serde_json::Map<String, serde_json::Value>, byte_len: usize) -> usize {
    let count_arr = |k: &str| -> usize {
        obj.get(k).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0)
    };
    let structural_items =
        count_arr("findings") + count_arr("hunks") + count_arr("files") + count_arr("groups");
    let key_hits = ["summary", "overall_risk", "groups", "files", "hunks", "findings"]
        .iter()
        .filter(|k| obj.contains_key(**k))
        .count();
    structural_items * 10_000 + key_hits * 1_000 + byte_len.min(999)
}

fn find_best_review_json_value(s: &str) -> Option<serde_json::Value> {
    if let Some(val) = parse_json_lenient(s.trim()) {
        if let Some(obj) = unwrap_review_object(&val) {
            return Some(serde_json::Value::Object(obj.clone()));
        }
    }

    let mut best: Option<(usize, serde_json::Value)> = None;
    let mut scanner = JsonObjectScanner::new(s);
    while let Some((candidate, val)) = scanner.next_object() {
        if let Some(obj) = unwrap_review_object(&val) {
            let score = review_object_richness_score(obj, candidate.len());
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score >= *best_score)
            {
                best = Some((score, serde_json::Value::Object(obj.clone())));
            }
        }
    }
    best.map(|(_, v)| v)
}

fn val_str(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn val_opt_str(v: Option<&serde_json::Value>) -> Option<String> {
    match v {
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| s.clone())
        }
        Some(serde_json::Value::Array(arr)) => {
            let parts: Vec<String> = arr
                .iter()
                .map(|x| val_str(Some(x)))
                .filter(|s| !s.trim().is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn val_str_vec(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .map(|x| val_str(Some(x)))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn val_opt_u32(v: Option<&serde_json::Value>) -> Option<u32> {
    match v {
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|u| u32::try_from(u).ok())
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u32)),
        Some(serde_json::Value::String(s)) => {
            let digits: String = s
                .trim()
                .trim_start_matches(['L', 'l', '#'])
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            (!digits.is_empty()).then(|| digits.parse::<u32>().ok()).flatten()
        }
        _ => None,
    }
}

fn val_opt_usize(v: Option<&serde_json::Value>) -> Option<usize> {
    val_opt_u32(v).map(|n| n as usize)
}

fn val_array<'a>(v: Option<&'a serde_json::Value>) -> &'a [serde_json::Value] {
    v.and_then(|x| x.as_array()).map(Vec::as_slice).unwrap_or(&[])
}

pub fn parse_ai_review_json(
    raw_output: &str,
    provider_name: &str,
    _rendered_diffs: &[FileDiffRendered],
) -> Result<AiReviewResult> {
    let root_val = find_best_review_json_value(raw_output)
        .context("No valid review JSON object found in AI provider response")?;

    let root = root_val
        .as_object()
        .context("Invalid JSON schema in AI provider response")?;

    let mut groups: Vec<ReviewGroup> = val_array(root.get("groups"))
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let g = item.as_object()?;
            let id = val_str(g.get("id"));
            let title = val_str(g.get("title"));
            let category = val_str(g.get("category"));
            let order = val_opt_u32(g.get("review_order")).unwrap_or(0);
            Some(ReviewGroup {
                id: if id.is_empty() {
                    format!("g{}", i + 1)
                } else {
                    id
                },
                title: if title.is_empty() {
                    format!("Group {}", i + 1)
                } else {
                    title
                },
                description: val_str(g.get("description")),
                category: if category.is_empty() {
                    "other".to_string()
                } else {
                    category
                },
                risk_level: RiskLevel::from_str_loose(&val_str(g.get("risk_level"))),
                review_order: if order == 0 { (i + 1) as u32 } else { order },
                files: val_str_vec(g.get("files")),
                key_points: val_str_vec(g.get("key_points")),
            })
        })
        .collect();

    groups.sort_by_key(|g| g.review_order);

    let files: Vec<FileAnalysis> = val_array(root.get("files"))
        .iter()
        .filter_map(|item| {
            let f = item.as_object()?;
            let path = val_str(f.get("path").or_else(|| f.get("file")));
            let category = val_str(f.get("category"));
            Some(FileAnalysis {
                path,
                category: if category.is_empty() {
                    "other".to_string()
                } else {
                    category
                },
                risk_level: RiskLevel::from_str_loose(&val_str(f.get("risk_level"))),
                summary: val_str(f.get("summary").or_else(|| f.get("description"))),
                review_focus: val_str_vec(f.get("review_focus")),
            })
        })
        .collect();

    let hunks: Vec<HunkAnalysis> = val_array(root.get("hunks"))
        .iter()
        .filter_map(|item| {
            let h = item.as_object()?;
            Some(HunkAnalysis {
                file: val_str(h.get("file").or_else(|| h.get("path"))),
                chunk_index: val_opt_usize(h.get("chunk_index")),
                start_line: val_opt_u32(h.get("start_line").or_else(|| h.get("line"))),
                end_line: val_opt_u32(h.get("end_line")),
                title: val_str(h.get("title")),
                analysis: val_str(
                    h.get("analysis")
                        .or_else(|| h.get("description"))
                        .or_else(|| h.get("rationale")),
                ),
                risk_level: RiskLevel::from_str_loose(&val_str(h.get("risk_level"))),
            })
        })
        .collect();

    let mut findings: Vec<ReviewFinding> = val_array(root.get("findings"))
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let f = item.as_object()?;
            let id = val_str(f.get("id"));
            Some(ReviewFinding {
                id: if id.is_empty() {
                    format!("f{}", i + 1)
                } else {
                    id
                },
                file: val_str(f.get("file").or_else(|| f.get("path"))),
                line: val_opt_u32(f.get("line").or_else(|| f.get("start_line"))),
                severity: Severity::from_str_loose(&val_str(f.get("severity"))),
                title: val_str(f.get("title")),
                description: val_str(
                    f.get("description")
                        .or_else(|| f.get("message"))
                        .or_else(|| f.get("detail")),
                ),
                suggestion: val_opt_str(f.get("suggestion")),
            })
        })
        .collect();

    findings.sort_by_key(|f| f.severity);

    Ok(AiReviewResult {
        provider_name: provider_name.to_string(),
        summary: val_str(root.get("summary")),
        overall_risk: RiskLevel::from_str_loose(&val_str(root.get("overall_risk"))),
        review_strategy: val_str(root.get("review_strategy")),
        groups,
        files,
        hunks,
        findings,
    })
}

fn ensure_all_files_grouped(result: &mut AiReviewResult, rendered_diffs: &[FileDiffRendered]) {
    let mut covered: HashSet<String> = HashSet::new();
    for g in &result.groups {
        for f in &g.files {
            covered.insert(f.clone());
        }
    }
    let missing: Vec<String> = rendered_diffs
        .iter()
        .map(|d| d.path.clone())
        .filter(|p| !covered.contains(p))
        .collect();

    if !missing.is_empty() {
        let next_order = result
            .groups
            .iter()
            .map(|g| g.review_order)
            .max()
            .unwrap_or(0)
            + 1;
        result.groups.push(ReviewGroup {
            id: format!("g{}", result.groups.len() + 1),
            title: "Other Changes / Uncategorized".to_string(),
            description: "Files not explicitly assigned to a semantic group by the AI response."
                .to_string(),
            category: "other".to_string(),
            risk_level: RiskLevel::Low,
            review_order: next_order,
            files: missing,
            key_points: vec![],
        });
    }
}

pub fn build_fallback_commit_message(
    bundle: &DiffBundle,
    selected_paths: &[String],
    ai_review: Option<&AiReviewResult>,
) -> String {
    let selected: HashSet<&str> = selected_paths.iter().map(|s| s.as_str()).collect();
    if selected_paths.is_empty() {
        return String::new();
    }

    if let Some(ai) = ai_review {
        if !ai.groups.is_empty() {
            let active_groups: Vec<&ReviewGroup> = ai
                .groups
                .iter()
                .filter(|g| g.files.iter().any(|f| selected.contains(f.as_str())))
                .collect();
            let ascii_titles: Vec<&str> = active_groups
                .iter()
                .take(3)
                .map(|g| g.title.trim())
                .filter(|t| !t.is_empty() && t.is_ascii())
                .collect();
            if !ascii_titles.is_empty() && ascii_titles.len() == active_groups.len().min(3) {
                if active_groups.len() == 1 {
                    let g = active_groups[0];
                    let desc = g.description.split(['\n', '.']).next().unwrap_or("").trim();
                    if !desc.is_empty() && desc.is_ascii() && desc.chars().count() <= 48 {
                        return format!("{}: {}", g.title.trim(), desc);
                    }
                    return g.title.trim().to_string();
                }
                if active_groups.len() > 3 {
                    return format!("Update {}, etc.", ascii_titles.join(", "));
                }
                return format!("Update {}", ascii_titles.join(", "));
            }
        }
    }

    let first = bundle
        .changes
        .iter()
        .find(|c| selected.contains(c.path.as_str()));
    match (selected_paths.len(), first) {
        (1, Some(c)) => {
            let name = std::path::Path::new(&c.path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&c.path);
            let verb = match c.kind {
                crate::model::ChangeKind::Added => "Add",
                crate::model::ChangeKind::Deleted => "Remove",
                crate::model::ChangeKind::Renamed { .. } => "Rename",
                _ => "Update",
            };
            format!("{} {}", verb, name)
        }
        (_, Some(c)) => {
            let name = std::path::Path::new(&c.path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&c.path);
            format!("Update {} and related changes", name)
        }
        _ => "Update selected changes".to_string(),
    }
}

fn strip_trailing_file_count(s: &str) -> &str {
    let trimmed = s.trim_end();
    for (open, close) in [('（', '）'), ('(', ')')] {
        if let Some(without_close) = trimmed.strip_suffix(close) {
            if let Some(pos) = without_close.rfind(open) {
                let inner = without_close[pos + open.len_utf8()..].trim();
                let is_count = (inner.ends_with("个文件") || inner.ends_with("files") || inner.ends_with("file"))
                    && inner.chars().next().is_some_and(|c| c.is_ascii_digit());
                if is_count {
                    return without_close[..pos].trim_end();
                }
            }
        }
    }
    trimmed
}

fn normalize_single_sentence_commit_msg(raw: &str) -> String {
    let stripped = raw
        .trim()
        .strip_prefix("```")
        .and_then(|s| s.strip_suffix("```"))
        .map(|s| s.trim())
        .unwrap_or_else(|| raw.trim());

    for line in stripped.lines() {
        let mut s = line.trim();
        if s.is_empty() || s.starts_with("```") {
            continue;
        }
        for prefix in ["- ", "* ", "• "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim();
            }
        }
        s = s.trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '“' || c == '”');
        if !s.is_empty() {
            let single = s.split_whitespace().collect::<Vec<_>>().join(" ");
            return strip_trailing_file_count(&single).to_string();
        }
    }
    String::new()
}

pub fn generate_commit_message(
    provider: &dyn AiProvider,
    bundle: &DiffBundle,
    rendered_diffs: &[FileDiffRendered],
    selected_paths: &[String],
    ai_review: Option<&AiReviewResult>,
    cfg: &AppConfig,
) -> Result<AiPromptOutput> {
    let selected: HashSet<&str> = selected_paths.iter().map(|s| s.as_str()).collect();
    let mut prompt = format!(
        "You are a senior software engineer writing a commit message.\n\
         Write the commit message strictly in English (even if the review language or group descriptions are non-English).\n\
         Strict requirements:\n\
         - Output ONLY ONE single concise English sentence on a single line summarizing ONLY the selected changes below.\n\
         - Keep it concise (ideally <= 72 ASCII characters).\n\
         - Do NOT include Chinese or non-English characters.\n\
         - Do NOT mention file counts or append suffixes like `(N files)` or `（N个文件）`.\n\
         - Do NOT call any tools, read external files, or run commands; summarize purely from the provided diffs and groups.\n\
         - NO line breaks, NO bullet points, NO prefixes like `feat:` unless natural, NO markdown fences, NO quotes, NO extra commentary.\n\n\
         VCS: {}\n\
         Selected files to commit:\n",
        bundle.vcs
    );

    for p in selected_paths {
        prompt.push_str(&format!("- {}\n", p));
    }

    if let Some(ai) = ai_review {
        let active_groups: Vec<&ReviewGroup> = ai
            .groups
            .iter()
            .filter(|g| g.files.iter().any(|f| selected.contains(f.as_str())))
            .collect();
        if !active_groups.is_empty() {
            prompt.push_str("\nSelected AI Category Groups:\n");
            for g in active_groups {
                prompt.push_str(&format!("- {}: {}\n", g.title, g.description));
            }
        }
    }

    prompt.push_str("\nStructural Diffs for selected files:\n");
    let mut budget = 24_000usize;
    for d in rendered_diffs {
        if !selected.contains(d.path.as_str()) || budget == 0 {
            continue;
        }
        let chunk = truncate_utf8_bytes(&d.ai_summary_diff, budget.min(4_000));
        budget = budget.saturating_sub(chunk.len());
        prompt.push_str(chunk);
        prompt.push('\n');
    }

    let locale = crate::i18n::current_thread_locale();
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!(
            "ai.generating_commit_msg",
            locale = locale.as_str(),
            count = selected_paths.len()
        )
        .into_owned(),
    ));
    let out = invoke_with_session(provider, &prompt, &bundle.repo_root, false, cfg, false)?;
    emit_progress(AiProgressUpdate::Stage(
        rust_i18n::t!("ai.formatting_commit_msg", locale = locale.as_str()).into_owned(),
    ));
    let cleaned = normalize_single_sentence_commit_msg(&out.text);
    Ok(AiPromptOutput {
        text: cleaned,
        session_id: out.session_id,
    })
}
