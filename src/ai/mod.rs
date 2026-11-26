pub mod cli_providers;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
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

    let mut fell_back = false;
    if let Some(sid) = saved_sid {
        let short_sid: String = sid.chars().take(8).collect();
        emit_progress(AiProgressUpdate::Stage(format!(
            "复用会话 {} ({})...",
            short_sid,
            provider.id()
        )));
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
        emit_progress(AiProgressUpdate::Stage(format!(
            "会话失效，重建 {} 会话...",
            provider.id()
        )));
    } else {
        emit_progress(AiProgressUpdate::Stage(format!(
            "新建 {} 会话...",
            provider.id()
        )));
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
    if cfg.cache_ai_results && !bypass_cache {
        emit_progress(AiProgressUpdate::Stage("检查缓存...".to_string()));
        if let Some(cache_dir) = AppConfig::cache_dir() {
            let cache_file = cache_dir.join(format!("{}.json", hash));
            if cache_file.exists() {
                if let Ok(raw) = std::fs::read_to_string(&cache_file) {
                    if let Ok(cached) = serde_json::from_str::<AiReviewResult>(&raw) {
                        if has_meaningful_review_content(&cached) {
                            emit_progress(AiProgressUpdate::Stage(
                                "命中缓存，直接加载".to_string(),
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

    emit_progress(AiProgressUpdate::Stage(format!(
        "整理差异 ({} 个文件)...",
        rendered_diffs.len()
    )));
    let prompt = build_review_prompt(bundle, rendered_diffs, &cfg.review_language);
    emit_progress(AiProgressUpdate::Stage(format!(
        "提交分析 ({:.1} KB -> {})...",
        prompt.len() as f64 / 1024.0,
        provider.id()
    )));
    let out = invoke_with_session(provider, &prompt, &bundle.repo_root, true, cfg, false)?;
    emit_progress(AiProgressUpdate::Stage(
        "解析返回结果...".to_string(),
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
    emit_progress(AiProgressUpdate::Stage(format!(
        "完成：{} 组，{} 条提示",
        parsed.groups.len(),
        parsed.findings.len()
    )));

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
    if cfg.cache_ai_results && !bypass_cache {
        emit_progress(AiProgressUpdate::Stage("检查缓存...".to_string()));
        if let Some(cache_dir) = AppConfig::cache_dir() {
            let cache_file = cache_dir.join(format!("{}.json", hash));
            if cache_file.exists() {
                if let Ok(raw) = std::fs::read_to_string(&cache_file) {
                    if let Ok(cached) = serde_json::from_str::<AiReviewResult>(&raw) {
                        if has_meaningful_review_content(&cached) {
                            emit_progress(AiProgressUpdate::Stage(
                                "命中缓存，直接加载".to_string(),
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
    emit_progress(AiProgressUpdate::Stage(format!(
        "整理差异 ({} 个文件)...",
        rendered_diffs.len()
    )));
    let group_prompt = build_grouping_prompt(bundle, rendered_diffs, &fast_cfg.review_language);
    let fast_label = fast_cfg
        .ai_model
        .as_deref()
        .unwrap_or_else(|| fast_provider.id());
    emit_progress(AiProgressUpdate::Stage(format!(
        "快速分组 ({:.1} KB -> {})...",
        group_prompt.len() as f64 / 1024.0,
        fast_label
    )));
    let out1 = invoke_with_session(
        fast_provider,
        &group_prompt,
        &bundle.repo_root,
        true,
        &fast_cfg,
        false,
    )?;
    emit_progress(AiProgressUpdate::Stage("解析分组...".to_string()));
    let mut stage1 =
        parse_ai_review_json(&out1.text, deep_provider.display_name(), rendered_diffs)
            .with_context(|| {
                format!(
                    "Failed to parse Stage 1 grouping JSON from {}",
                    fast_provider.display_name()
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
    emit_progress(AiProgressUpdate::Stage("深入分析核心风险...".to_string()));
    let findings_prompt =
        build_findings_prompt(bundle, rendered_diffs, &cfg.review_language, &stage1);
    let out2 = invoke_with_session(
        deep_provider,
        &findings_prompt,
        &bundle.repo_root,
        true,
        &deep_cfg,
        true,
    )?;
    emit_progress(AiProgressUpdate::Stage("合并风险分析结果...".to_string()));
    let stage2 = parse_ai_review_json(&out2.text, deep_provider.display_name(), rendered_diffs)
        .with_context(|| {
            format!(
                "Failed to parse Stage 2 findings JSON from {}",
                deep_provider.display_name()
            )
        })?;

    stage1.overall_risk = stage1.overall_risk.min(stage2.overall_risk);
    if !stage2.files.is_empty() {
        stage1.files = stage2.files;
    }
    if !stage2.hunks.is_empty() {
        stage1.hunks = stage2.hunks;
    }
    if !stage2.findings.is_empty() {
        stage1.findings = stage2.findings;
    }

    emit_progress(AiProgressUpdate::Stage(format!(
        "完成：{} 组，{} 条提示",
        stage1.groups.len(),
        stage1.findings.len()
    )));
    save_ai_review_cache(&hash, cfg.cache_ai_results, &stage1);

    Ok(AnalyzeDiffOutput {
        review: stage1,
        session_id: out2.session_id,
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

    prompt.push_str(
        r#"
--- REQUIRED OUTPUT FORMAT ---
Return ONLY a single valid JSON object (no markdown commentary outside JSON):
{
  "summary": "2-3 sentence summary of the overall change intent and impact",
  "overall_risk": "critical | high | medium | low | info",
  "review_strategy": "Recommended review order in 1 short sentence",
  "groups": [
    {
      "id": "g1",
      "title": "简明重点标题 (<= 16字)",
      "description": "一句话核心目的 (<= 45字)",
      "category": "feature | bugfix | refactor | security | performance | test | config | docs | chore",
      "risk_level": "critical | high | medium | low | info",
      "review_order": 1,
      "files": ["exact/file/path.rs"],
      "key_points": ["Key thing to verify"]
    }
  ]
}

Rules:
1. Every changed file MUST appear in at least one group in `groups` using its exact relative path.
2. Order `groups` by `review_order` (1 = review first). Keep titles short and descriptions concise.
3. Output valid JSON only.
"#,
    );
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
         You may inspect workspace files via read-only tools ONLY if necessary to verify suspicious bugs, callers, or edge cases.\n\
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

    prompt.push_str(
        r#"
--- REQUIRED OUTPUT FORMAT ---
Return ONLY a single valid JSON object (no markdown commentary outside JSON) with this exact structure:
{
  "summary": "2-4 sentence executive summary of the overall change intent and impact",
  "overall_risk": "critical | high | medium | low | info",
  "review_strategy": "Recommended order and focus areas for the human reviewer",
  "groups": [
    {
      "id": "g1",
      "title": "简明重点标题 (<= 16字，如: 世界地图动态配置)",
      "description": "一句话简短描述这一组改动的核心目的 (<= 45字)",
      "category": "feature | bugfix | refactor | security | performance | test | config | docs | chore",
      "risk_level": "critical | high | medium | low | info",
      "review_order": 1,
      "files": ["exact/file/path.rs"],
      "key_points": ["Key thing to verify while reviewing this group"]
    }
  ],
  "files": [
    {
      "path": "exact/file/path.rs",
      "category": "feature | bugfix | refactor | security | performance | test | config | docs | chore",
      "risk_level": "critical | high | medium | low | info",
      "summary": "One-sentence summary of changes in this file",
      "review_focus": ["Specific check for this file"]
    }
  ],
  "hunks": [
    {
      "file": "exact/file/path.rs",
      "chunk_index": 1,
      "start_line": 40,
      "end_line": 55,
      "title": "Short title for this diff paragraph/hunk",
      "analysis": "Concise explanation of what this specific diff chunk does, intent, edge cases, and what to double-check",
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
1. Every changed file MUST appear in at least one group in `groups` using its exact relative path from the list above.
2. Group files semantically by review focus and order `groups` by `review_order` (1 = review first). Keep each group `title` short and scannable, and `description` concise (one short sentence).
3. Include a `hunks` entry for each major `@@ Chunk N` in the diffs so the reviewer sees paragraph-level analysis while scrolling through code.
4. Only cite lines and symbols visible in the provided diff; do not invent unshown code.
5. Output valid JSON only.
"#,
    );

    prompt
}

#[derive(Debug, Deserialize)]
struct RawAiReview {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    overall_risk: String,
    #[serde(default)]
    review_strategy: String,
    #[serde(default)]
    groups: Vec<RawGroup>,
    #[serde(default)]
    files: Vec<RawFile>,
    #[serde(default)]
    hunks: Vec<RawHunk>,
    #[serde(default)]
    findings: Vec<RawFinding>,
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    risk_level: String,
    #[serde(default)]
    review_order: u32,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    key_points: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(default)]
    path: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    risk_level: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    review_focus: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawHunk {
    #[serde(default)]
    file: String,
    #[serde(default)]
    chunk_index: Option<usize>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    analysis: String,
    #[serde(default)]
    risk_level: String,
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    #[serde(default)]
    id: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    suggestion: Option<String>,
}

fn scan_balanced_json_object(s: &str) -> Option<&str> {
    for (start, ch) in s.char_indices() {
        if ch != '{' {
            continue;
        }
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
        for (rel_idx, c) in s[start..].char_indices() {
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
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &s[start..start + rel_idx + c.len_utf8()];
                        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                            return Some(candidate);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub fn extract_json_block(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
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

fn find_review_json_object(s: &str) -> Option<&str> {
    for (start, ch) in s.char_indices() {
        if ch != '{' {
            continue;
        }
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
        for (rel_idx, c) in s[start..].char_indices() {
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
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &s[start..start + rel_idx + c.len_utf8()];
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(candidate) {
                            if is_review_json_object(&val) {
                                return Some(candidate);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub fn parse_ai_review_json(
    raw_output: &str,
    provider_name: &str,
    _rendered_diffs: &[FileDiffRendered],
) -> Result<AiReviewResult> {
    let json_slice = match extract_json_block(raw_output) {
        Some(slice)
            if serde_json::from_str::<serde_json::Value>(slice)
                .map(|v| is_review_json_object(&v))
                .unwrap_or(false) =>
        {
            slice
        }
        _ => find_review_json_object(raw_output)
            .context("No valid review JSON object found in AI provider response")?,
    };

    let raw: RawAiReview = serde_json::from_str(json_slice)
        .context("Invalid JSON schema in AI provider response")?;

    let mut groups: Vec<ReviewGroup> = raw
        .groups
        .into_iter()
        .enumerate()
        .map(|(i, g)| ReviewGroup {
            id: if g.id.is_empty() {
                format!("g{}", i + 1)
            } else {
                g.id
            },
            title: if g.title.is_empty() {
                format!("Group {}", i + 1)
            } else {
                g.title
            },
            description: g.description,
            category: if g.category.is_empty() {
                "other".to_string()
            } else {
                g.category
            },
            risk_level: RiskLevel::from_str_loose(&g.risk_level),
            review_order: if g.review_order == 0 {
                (i + 1) as u32
            } else {
                g.review_order
            },
            files: g.files,
            key_points: g.key_points,
        })
        .collect();

    groups.sort_by_key(|g| g.review_order);

    let files: Vec<FileAnalysis> = raw
        .files
        .into_iter()
        .map(|f| FileAnalysis {
            path: f.path,
            category: if f.category.is_empty() {
                "other".to_string()
            } else {
                f.category
            },
            risk_level: RiskLevel::from_str_loose(&f.risk_level),
            summary: f.summary,
            review_focus: f.review_focus,
        })
        .collect();

    let hunks: Vec<HunkAnalysis> = raw
        .hunks
        .into_iter()
        .map(|h| HunkAnalysis {
            file: h.file,
            chunk_index: h.chunk_index,
            start_line: h.start_line,
            end_line: h.end_line,
            title: h.title,
            analysis: h.analysis,
            risk_level: RiskLevel::from_str_loose(&h.risk_level),
        })
        .collect();

    let mut findings: Vec<ReviewFinding> = raw
        .findings
        .into_iter()
        .enumerate()
        .map(|(i, f)| ReviewFinding {
            id: if f.id.is_empty() {
                format!("f{}", i + 1)
            } else {
                f.id
            },
            file: f.file,
            line: f.line,
            severity: Severity::from_str_loose(&f.severity),
            title: f.title,
            description: f.description,
            suggestion: f.suggestion,
        })
        .collect();

    findings.sort_by_key(|f| f.severity);

    Ok(AiReviewResult {
        provider_name: provider_name.to_string(),
        summary: raw.summary,
        overall_risk: RiskLevel::from_str_loose(&raw.overall_risk),
        review_strategy: raw.review_strategy,
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

    emit_progress(AiProgressUpdate::Stage(format!(
        "生成提交说明 ({} 个文件)...",
        selected_paths.len()
    )));
    let out = invoke_with_session(provider, &prompt, &bundle.repo_root, false, cfg, false)?;
    emit_progress(AiProgressUpdate::Stage(
        "整理提交说明...".to_string(),
    ));
    let cleaned = normalize_single_sentence_commit_msg(&out.text);
    Ok(AiPromptOutput {
        text: cleaned,
        session_id: out.session_id,
    })
}
