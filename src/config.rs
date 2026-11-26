use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::model::DifftDisplayMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomAiConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectAiConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub fast_model: Option<String>,
    pub fast_effort: Option<String>,
    pub language: Option<String>,
    pub timeout_secs: Option<u64>,
    pub cache_ai_results: Option<bool>,
    pub reuse_session: Option<bool>,
    /// Saved conversation/session IDs per AI provider (e.g. `agy = "uuid"`).
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectDiffConfig {
    pub display_mode: Option<DifftDisplayMode>,
    pub ignore_comments: Option<bool>,
    pub difft_bin: Option<String>,
    pub cache_history: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectIgnoreConfig {
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// Project-level `.codeduck` TOML configuration file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectCodeduckConfig {
    #[serde(default)]
    pub ai: ProjectAiConfig,
    #[serde(default)]
    pub diff: ProjectDiffConfig,
    #[serde(default)]
    pub ignore: ProjectIgnoreConfig,
    #[serde(default)]
    pub custom_ai: Vec<CustomAiConfig>,
    // Optional top-level shorthand keys
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Preferred AI provider: "auto", "agy", "codex", "claude", "gemini", "none", or custom name.
    #[serde(default = "default_ai_provider")]
    pub ai_provider: String,
    /// Optional model override passed to the underlying AI CLI.
    #[serde(default)]
    pub ai_model: Option<String>,
    /// Optional reasoning/thinking effort override (low, medium, high, xhigh, max).
    #[serde(default)]
    pub ai_effort: Option<String>,
    /// Fast model used for Stage 1 grouping and commit message generation.
    #[serde(default)]
    pub ai_fast_model: Option<String>,
    /// Fast reasoning/thinking effort used for Stage 1 grouping and commit message generation.
    #[serde(default)]
    pub ai_fast_effort: Option<String>,
    /// Output language for AI review summaries & findings.
    #[serde(default = "default_language")]
    pub review_language: String,
    /// Difftastic binary path.
    #[serde(default = "default_difft_bin")]
    pub difft_bin: String,
    /// Default difftastic display mode.
    #[serde(default)]
    pub display_mode: DifftDisplayMode,
    /// Enable disk cache for AI analysis keyed by diff hash.
    #[serde(default = "default_true")]
    pub cache_ai_results: bool,
    /// Enable disk cache for commit history and immutable commit diffs.
    #[serde(default = "default_true")]
    pub cache_history: bool,
    /// Ignore comments in difftastic structural diff.
    #[serde(default)]
    pub ignore_comments: bool,
    /// Timeout in seconds for each local AI CLI invocation.
    #[serde(default = "default_ai_timeout")]
    pub ai_timeout_secs: u64,
    /// Reuse local AI CLI sessions across turns/restarts and persist IDs in `.codeduck`.
    #[serde(default = "default_true")]
    pub reuse_session: bool,
    /// Persisted AI session IDs per provider, loaded from `.codeduck` `[ai.sessions]`.
    #[serde(default)]
    pub ai_sessions: BTreeMap<String, String>,
    /// Optional custom AI CLI commands.
    #[serde(default)]
    pub custom_ai: Vec<CustomAiConfig>,
}

fn default_ai_provider() -> String {
    "auto".to_string()
}

fn default_language() -> String {
    "zh-CN".to_string()
}

fn default_difft_bin() -> String {
    "difft".to_string()
}

fn default_true() -> bool {
    true
}

fn default_ai_timeout() -> u64 {
    90
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ai_provider: default_ai_provider(),
            ai_model: None,
            ai_effort: None,
            ai_fast_model: None,
            ai_fast_effort: None,
            review_language: default_language(),
            difft_bin: default_difft_bin(),
            display_mode: DifftDisplayMode::SideBySide,
            cache_ai_results: true,
            cache_history: true,
            ignore_comments: false,
            ai_timeout_secs: default_ai_timeout(),
            reuse_session: true,
            ai_sessions: BTreeMap::new(),
            custom_ai: Vec::new(),
        }
    }
}

pub fn directory_chain(vcs_root: &Path, scope_dir: &Path) -> Vec<PathBuf> {
    let vcs_root_canon =
        std::fs::canonicalize(vcs_root).unwrap_or_else(|_| vcs_root.to_path_buf());
    let scope_canon =
        std::fs::canonicalize(scope_dir).unwrap_or_else(|_| scope_dir.to_path_buf());

    let mut dirs_to_check: Vec<PathBuf> = Vec::new();
    if let Ok(rel) = scope_canon.strip_prefix(&vcs_root_canon) {
        let mut cur = vcs_root_canon.clone();
        dirs_to_check.push(cur.clone());
        for comp in rel.components() {
            cur = cur.join(comp);
            dirs_to_check.push(cur.clone());
        }
    } else {
        if vcs_root_canon != scope_canon {
            dirs_to_check.push(vcs_root_canon);
        }
        dirs_to_check.push(scope_canon);
    }
    dirs_to_check
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PartialFlatConfig {
    ai_provider: Option<String>,
    ai_model: Option<String>,
    ai_effort: Option<String>,
    ai_fast_model: Option<String>,
    ai_fast_effort: Option<String>,
    review_language: Option<String>,
    difft_bin: Option<String>,
    display_mode: Option<DifftDisplayMode>,
    cache_ai_results: Option<bool>,
    cache_history: Option<bool>,
    ignore_comments: Option<bool>,
    ai_timeout_secs: Option<u64>,
    reuse_session: Option<bool>,
    #[serde(default)]
    ai_sessions: BTreeMap<String, String>,
}

impl AppConfig {
    /// Unified user-level codeduck directory: `$CODEDUCK_HOME` or `~/.codeduck`.
    pub fn codeduck_home_dir() -> Option<PathBuf> {
        if let Ok(env_home) = std::env::var("CODEDUCK_HOME") {
            let trimmed = env_home.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
        dirs::home_dir().map(|h| h.join(".codeduck"))
    }

    /// Global general configuration file: `~/.codeduck/configs.toml`.
    pub fn config_path() -> Option<PathBuf> {
        Self::codeduck_home_dir().map(|d| d.join("configs.toml"))
    }

    fn legacy_config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("codeduck").join("config.toml"))
    }

    /// Unified cache directory: `~/.codeduck/cache`.
    pub fn cache_dir() -> Option<PathBuf> {
        Self::codeduck_home_dir().map(|d| d.join("cache"))
    }

    /// Global AI dialogue session store: `~/.codeduck/sessions.toml` (keyed per workspace path).
    pub fn sessions_path() -> Option<PathBuf> {
        Self::codeduck_home_dir().map(|d| d.join("sessions.toml"))
    }

    /// Derive a lightweight, stateless config for fast tasks (Stage 1 grouping & commit messages).
    /// Applies `ai_fast_model`/`ai_fast_effort` and disables `reuse_session` so bloated sessions
    /// never slow down diff-only passes.
    pub fn fast_config(&self) -> Self {
        let mut fast = self.clone();
        if let Some(fm) = &self.ai_fast_model {
            fast.ai_model = Some(fm.clone());
        }
        if let Some(fe) = &self.ai_fast_effort {
            fast.ai_effort = Some(fe.clone());
        } else if self.ai_fast_model.is_some() {
            fast.ai_effort = Some("low".to_string());
        }
        fast.reuse_session = false;
        fast.ai_sessions.clear();
        fast
    }

    fn merge_toml_str(&mut self, raw: &str, path_hint: &Path) -> Result<()> {
        // 1. Apply any legacy flat keys (`ai_provider = ...`, etc.) if present.
        if let Ok(flat) = toml::from_str::<PartialFlatConfig>(raw) {
            if let Some(p) = flat.ai_provider {
                if !p.trim().is_empty() {
                    self.ai_provider = p;
                }
            }
            if let Some(m) = flat.ai_model {
                if !m.trim().is_empty() {
                    self.ai_model = Some(m);
                }
            }
            if let Some(e) = flat.ai_effort {
                if !e.trim().is_empty() {
                    self.ai_effort = Some(e);
                }
            }
            if let Some(fm) = flat.ai_fast_model {
                if !fm.trim().is_empty() {
                    self.ai_fast_model = Some(fm);
                }
            }
            if let Some(fe) = flat.ai_fast_effort {
                if !fe.trim().is_empty() {
                    self.ai_fast_effort = Some(fe);
                }
            }
            if let Some(lang) = flat.review_language {
                if !lang.trim().is_empty() {
                    self.review_language = lang;
                }
            }
            if let Some(db) = flat.difft_bin {
                if !db.trim().is_empty() {
                    self.difft_bin = db;
                }
            }
            if let Some(dm) = flat.display_mode {
                self.display_mode = dm;
            }
            if let Some(c) = flat.cache_ai_results {
                self.cache_ai_results = c;
            }
            if let Some(ch) = flat.cache_history {
                self.cache_history = ch;
            }
            if let Some(ic) = flat.ignore_comments {
                self.ignore_comments = ic;
            }
            if let Some(t) = flat.ai_timeout_secs {
                self.ai_timeout_secs = t;
            }
            if let Some(rs) = flat.reuse_session {
                self.reuse_session = rs;
            }
            for (k, v) in flat.ai_sessions {
                if !v.trim().is_empty() {
                    self.ai_sessions.insert(k, v);
                }
            }
        }

        // 2. Apply structured `[ai]` / `[diff]` / `[[custom_ai]]` sections (`.codeduck` compatible).
        let proj: ProjectCodeduckConfig = toml::from_str(raw)
            .with_context(|| format!("Failed to parse config file {}", path_hint.display()))?;
        self.merge_project_config(proj);
        Ok(())
    }

    fn merge_project_config(&mut self, proj: ProjectCodeduckConfig) {
        if let Some(p) = proj.ai.provider.or(proj.provider) {
            if !p.trim().is_empty() {
                self.ai_provider = p;
            }
        }
        if let Some(m) = proj.ai.model.or(proj.model) {
            if !m.trim().is_empty() {
                self.ai_model = Some(m);
            }
        }
        if let Some(e) = proj.ai.effort.or(proj.effort) {
            if !e.trim().is_empty() {
                self.ai_effort = Some(e);
            }
        }
        if let Some(fm) = proj.ai.fast_model {
            if !fm.trim().is_empty() {
                self.ai_fast_model = Some(fm);
            }
        }
        if let Some(fe) = proj.ai.fast_effort {
            if !fe.trim().is_empty() {
                self.ai_fast_effort = Some(fe);
            }
        }
        if let Some(lang) = proj.ai.language {
            if !lang.trim().is_empty() {
                self.review_language = lang;
            }
        }
        if let Some(t) = proj.ai.timeout_secs {
            self.ai_timeout_secs = t;
        }
        if let Some(c) = proj.ai.cache_ai_results {
            self.cache_ai_results = c;
        }
        if let Some(rs) = proj.ai.reuse_session {
            self.reuse_session = rs;
        }
        for (k, v) in proj.ai.sessions {
            if !v.trim().is_empty() {
                self.ai_sessions.insert(k, v);
            }
        }
        if let Some(dm) = proj.diff.display_mode {
            self.display_mode = dm;
        }
        if let Some(ic) = proj.diff.ignore_comments {
            self.ignore_comments = ic;
        }
        if let Some(db) = proj.diff.difft_bin {
            if !db.trim().is_empty() {
                self.difft_bin = db;
            }
        }
        if let Some(ch) = proj.diff.cache_history {
            self.cache_history = ch;
        }
        for custom in proj.custom_ai {
            if let Some(existing) = self.custom_ai.iter_mut().find(|c| c.name == custom.name) {
                *existing = custom;
            } else {
                self.custom_ai.push(custom);
            }
        }
    }

    pub fn load(explicit_path: Option<&Path>) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(p) = explicit_path {
            if p.exists() {
                let raw = std::fs::read_to_string(p)
                    .with_context(|| format!("Failed to read config file {}", p.display()))?;
                cfg.merge_toml_str(&raw, p)?;
            }
            return Ok(cfg);
        }

        // Load legacy config first (lowest), then `~/.codeduck/configs.toml` on top.
        for candidate in [Self::legacy_config_path(), Self::config_path()]
            .into_iter()
            .flatten()
        {
            if candidate.is_file() {
                let raw = std::fs::read_to_string(&candidate)
                    .with_context(|| format!("Failed to read config file {}", candidate.display()))?;
                cfg.merge_toml_str(&raw, &candidate)?;
            }
        }
        Ok(cfg)
    }

    /// Load and merge project `.codeduck` TOML files from `vcs_root` down to `scope_dir`,
    /// followed by centralized dialogue sessions from `~/.codeduck/sessions.toml`.
    /// Any setting omitted by local `.codeduck` files retains its value from `configs.toml`.
    pub fn apply_project_overrides(&mut self, vcs_root: &Path, scope_dir: &Path) {
        self.apply_project_overrides_with_sessions(
            vcs_root,
            scope_dir,
            Self::sessions_path().as_deref(),
        );
    }

    pub fn apply_project_overrides_with_sessions(
        &mut self,
        vcs_root: &Path,
        scope_dir: &Path,
        sessions_file: Option<&Path>,
    ) {
        let chain = directory_chain(vcs_root, scope_dir);
        for dir in &chain {
            let file = dir.join(".codeduck");
            if !file.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&file) else {
                continue;
            };
            let proj = match toml::from_str::<ProjectCodeduckConfig>(&raw) {
                Ok(p) => p,
                Err(err) => {
                    let looks_like_toml = raw.lines().any(|l| {
                        let t = l.trim();
                        t == "[ai]" || t == "[diff]" || t == "[ignore]"
                    });
                    if looks_like_toml {
                        eprintln!(
                            "Warning: failed to parse TOML in {}: {}",
                            file.display(),
                            err
                        );
                    }
                    // Legacy plain-text .gitignore-style .codeduck has no TOML keys to merge
                    continue;
                }
            };
            self.merge_project_config(proj);
        }

        // Overlay unified user-level dialogue sessions (`~/.codeduck/sessions.toml`)
        if let Some(sfile) = sessions_file {
            if sfile.is_file() {
                if let Ok(raw) = std::fs::read_to_string(sfile) {
                    if let Ok(store) = toml::from_str::<GlobalSessionsFile>(&raw) {
                        for dir in &chain {
                            let key = dir.to_string_lossy();
                            if let Some(map) = store.projects.get(key.as_ref()) {
                                for (k, v) in map {
                                    if !v.trim().is_empty() {
                                        self.ai_sessions.insert(k.clone(), v.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GlobalSessionsFile {
    #[serde(default)]
    projects: BTreeMap<String, BTreeMap<String, String>>,
}

fn canonical_scope_key(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn atomic_write_file(target: &Path, content: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let tmp_name = format!(
        ".{}.tmp.{}",
        target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("sessions.toml"),
        std::process::id()
    );
    let tmp_path = target.with_file_name(tmp_name);
    std::fs::write(&tmp_path, content)
        .with_context(|| format!("Failed to write temporary file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, target)
        .with_context(|| format!("Failed to replace {}", target.display()))?;
    Ok(())
}

/// Persist (or clear when `session_id` is None) an AI provider's session ID centrally in
/// `~/.codeduck/sessions.toml` keyed by workspace path so project `.codeduck` files stay clean.
pub fn save_project_session(
    vcs_root: &Path,
    scope_dir: &Path,
    provider_id: &str,
    session_id: Option<&str>,
) -> Result<()> {
    save_project_session_at(
        vcs_root,
        scope_dir,
        provider_id,
        session_id,
        AppConfig::sessions_path().as_deref(),
    )
}

pub fn save_project_session_at(
    vcs_root: &Path,
    scope_dir: &Path,
    provider_id: &str,
    session_id: Option<&str>,
    sessions_file: Option<&Path>,
) -> Result<()> {
    if session_id.is_none() {
        // 1. Clear from unified `~/.codeduck/sessions.toml` along directory chain
        if let Some(sfile) = sessions_file {
            if sfile.is_file() {
                if let Ok(existing) = std::fs::read_to_string(sfile) {
                    if let Ok(mut doc) = existing.parse::<DocumentMut>() {
                        let mut changed = false;
                        let chain = directory_chain(vcs_root, scope_dir);
                        if let Some(projs) = doc.get_mut("projects").and_then(|i| i.as_table_like_mut()) {
                            for dir in &chain {
                                let key = dir.to_string_lossy();
                                let mut became_empty = false;
                                if let Some(scope_tbl) =
                                    projs.get_mut(key.as_ref()).and_then(|i| i.as_table_like_mut())
                                {
                                    if scope_tbl.remove(provider_id).is_some() {
                                        changed = true;
                                    }
                                    became_empty = scope_tbl.is_empty();
                                }
                                if became_empty {
                                    projs.remove(key.as_ref());
                                }
                            }
                        }
                        if changed {
                            atomic_write_file(sfile, &doc.to_string())?;
                        }
                    }
                }
            }
        }

        // 2. Also remove legacy `[ai.sessions].<provider_id>` from project `.codeduck` if present
        for dir in directory_chain(vcs_root, scope_dir) {
            let file = dir.join(".codeduck");
            if !file.is_file() {
                continue;
            }
            let Ok(existing) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(mut doc) = existing.parse::<DocumentMut>() else {
                continue;
            };
            let mut removed = false;
            if let Some(ai_tbl) = doc.get_mut("ai").and_then(|i| i.as_table_like_mut()) {
                if let Some(sess_tbl) = ai_tbl.get_mut("sessions").and_then(|i| i.as_table_like_mut()) {
                    removed = sess_tbl.remove(provider_id).is_some();
                }
            }
            if removed {
                std::fs::write(&file, doc.to_string())
                    .with_context(|| format!("Failed to write {}", file.display()))?;
            }
        }
        return Ok(());
    }

    let Some(sfile) = sessions_file else {
        return Ok(());
    };
    let sid = session_id.unwrap();
    let scope_key = canonical_scope_key(scope_dir);

    let mut doc = if sfile.is_file() {
        let existing = std::fs::read_to_string(sfile)
            .with_context(|| format!("Failed to read {}", sfile.display()))?;
        existing.parse::<DocumentMut>().unwrap_or_default()
    } else {
        let mut d = DocumentMut::new();
        d.decor_mut().set_prefix(
            "# codeduck 全局对话 session 记录（按工作区路径隔离，避免污染项目 `.codeduck`）\n\n",
        );
        d
    };

    if !doc.contains_key("projects") {
        doc.insert("projects", Item::Table(Table::new()));
    }
    let projs_tbl = doc
        .get_mut("projects")
        .and_then(|i| i.as_table_like_mut())
        .context("Expected [projects] to be a TOML table in sessions.toml")?;

    if !projs_tbl.contains_key(&scope_key) {
        projs_tbl.insert(&scope_key, Item::Table(Table::new()));
    }
    let scope_tbl = projs_tbl
        .get_mut(&scope_key)
        .and_then(|i| i.as_table_like_mut())
        .context("Expected project session entry to be a TOML table in sessions.toml")?;

    scope_tbl.insert(provider_id, Item::Value(Value::from(sid)));
    atomic_write_file(sfile, &doc.to_string())?;
    Ok(())
}
