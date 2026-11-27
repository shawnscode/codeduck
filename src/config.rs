use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::i18n;
use crate::model::DifftDisplayMode;

#[derive(Debug, Clone, Copy, Default)]
pub struct UserSettingsMask {
    pub ui_language: bool,
    pub review_language: bool,
    pub ai_provider: bool,
    pub display_mode: bool,
    pub ignore_comments: bool,
    pub cache_ai_results: bool,
    pub cache_history: bool,
    pub reuse_session: bool,
}

impl UserSettingsMask {
    pub fn all() -> Self {
        Self {
            ui_language: true,
            review_language: true,
            ai_provider: true,
            display_mode: true,
            ignore_comments: true,
            cache_ai_results: true,
            cache_history: true,
            reuse_session: true,
        }
    }

    pub fn any(self) -> bool {
        self.ui_language
            || self.review_language
            || self.ai_provider
            || self.display_mode
            || self.ignore_comments
            || self.cache_ai_results
            || self.cache_history
            || self.reuse_session
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomAiConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectUiConfig {
    pub language: Option<String>,
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
    pub ui: ProjectUiConfig,
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
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// UI localization language code (e.g. "en", "zh-CN", or any file in `locales/`).
    #[serde(default = "default_language")]
    pub ui_language: String,
    /// Tracks whether UI language was explicitly configured independently of `[ai].language`.
    #[serde(skip)]
    pub ui_language_explicit: bool,
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
    /// Reuse local AI CLI sessions across turns/restarts and persist IDs in `~/.codeduck/sessions.toml`.
    #[serde(default = "default_true")]
    pub reuse_session: bool,
    /// Persisted AI session IDs per provider, loaded from `~/.codeduck/sessions.toml`.
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
    "en".to_string()
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
            ui_language: default_language(),
            ui_language_explicit: false,
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
    ui_language: Option<String>,
    language: Option<String>,
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

pub fn normalize_optional_setting(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("default")
        || trimmed.eq_ignore_ascii_case("clear")
    {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn validate_cli_value<'a>(field: &str, val: &'a str) -> Result<&'a str> {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{} cannot be empty", field);
    }
    if trimmed.starts_with('-') {
        anyhow::bail!("{} cannot start with `-` (got `{}`)", field, trimmed);
    }
    if trimmed.chars().any(|c| c.is_control()) {
        anyhow::bail!("{} contains invalid control characters", field);
    }
    Ok(trimmed)
}

pub fn validate_effort(val: &str) -> Result<String> {
    let trimmed = validate_cli_value("effort", val)?;
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "low" | "medium" | "high" | "xhigh" | "max" => Ok(lower),
        _ => anyhow::bail!(
            "Unsupported effort `{}` (expected low, medium, high, xhigh, or max)",
            trimmed
        ),
    }
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

    /// Switch AI provider and clear incompatible model overrides from previous providers.
    pub fn switch_provider(&mut self, new_provider: &str) {
        let trimmed = new_provider.trim();
        if trimmed.is_empty() {
            return;
        }
        if !trimmed.eq_ignore_ascii_case(&self.ai_provider) {
            self.ai_model = None;
            self.ai_fast_model = None;
        }
        self.ai_provider = trimmed.to_string();
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
            let mut flat_provider_changed = false;
            if let Some(p) = flat.ai_provider {
                let t = p.trim();
                if !t.is_empty() {
                    if !t.eq_ignore_ascii_case(&self.ai_provider) {
                        flat_provider_changed = true;
                    }
                    self.ai_provider = t.to_string();
                }
            }
            if flat_provider_changed {
                if flat.ai_model.is_none() {
                    self.ai_model = None;
                }
                if flat.ai_fast_model.is_none() {
                    self.ai_fast_model = None;
                }
            }
            if let Some(m) = flat.ai_model {
                self.ai_model = normalize_optional_setting(&m);
            }
            if let Some(e) = flat.ai_effort {
                self.ai_effort = normalize_optional_setting(&e);
            }
            if let Some(fm) = flat.ai_fast_model {
                self.ai_fast_model = normalize_optional_setting(&fm);
            }
            if let Some(fe) = flat.ai_fast_effort {
                self.ai_fast_effort = normalize_optional_setting(&fe);
            }
            if let Some(ui_lang) = flat.ui_language {
                if let Some(loc) = i18n::resolve_locale(&ui_lang) {
                    self.ui_language = loc;
                    self.ui_language_explicit = true;
                }
            }
            if let Some(top_lang) = flat.language {
                if let Some(loc) = i18n::resolve_locale(&top_lang) {
                    self.ui_language = loc;
                    self.ui_language_explicit = true;
                }
            }
            if let Some(lang) = flat.review_language {
                if !lang.trim().is_empty() {
                    if !self.ui_language_explicit {
                        if let Some(loc) = i18n::resolve_locale(&lang) {
                            self.ui_language = loc;
                        }
                    }
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

        // 2. Apply structured `[ai]` / `[diff]` / `[[custom_ai]]` sections (trusted global config).
        let proj: ProjectCodeduckConfig = toml::from_str(raw)
            .with_context(|| format!("Failed to parse config file {}", path_hint.display()))?;
        self.merge_project_config(proj, true);
        Ok(())
    }

    fn merge_project_config(&mut self, proj: ProjectCodeduckConfig, trusted: bool) {
        let new_provider = proj.ai.provider.or(proj.provider);
        let provider_changed = if let Some(ref p) = new_provider {
            let t = p.trim();
            if !t.is_empty() && !t.eq_ignore_ascii_case(&self.ai_provider) {
                self.ai_provider = t.to_string();
                true
            } else {
                false
            }
        } else {
            false
        };

        let model_override = proj.ai.model.or(proj.model);
        let effort_override = proj.ai.effort.or(proj.effort);
        if provider_changed {
            if model_override.is_none() {
                self.ai_model = None;
            }
            if proj.ai.fast_model.is_none() {
                self.ai_fast_model = None;
            }
        }

        if let Some(m) = model_override {
            self.ai_model = normalize_optional_setting(&m);
        }
        if let Some(e) = effort_override {
            self.ai_effort = normalize_optional_setting(&e);
        }
        if let Some(fm) = proj.ai.fast_model {
            self.ai_fast_model = normalize_optional_setting(&fm);
        }
        if let Some(fe) = proj.ai.fast_effort {
            self.ai_fast_effort = normalize_optional_setting(&fe);
        }
        let ui_lang_override = proj.ui.language.or(proj.language);
        if let Some(ui_lang) = ui_lang_override {
            if let Some(loc) = i18n::resolve_locale(&ui_lang) {
                self.ui_language = loc;
                self.ui_language_explicit = true;
            }
        }
        if let Some(lang) = proj.ai.language {
            if !lang.trim().is_empty() {
                if !self.ui_language_explicit {
                    if let Some(loc) = i18n::resolve_locale(&lang) {
                        self.ui_language = loc;
                    }
                }
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
        if let Some(ch) = proj.diff.cache_history {
            self.cache_history = ch;
        }

        // Security boundary: untrusted project `.codeduck` files must never override executable commands.
        if trusted {
            if let Some(db) = proj.diff.difft_bin {
                if !db.trim().is_empty() {
                    self.difft_bin = db;
                }
            }
            for custom in proj.custom_ai {
                if let Some(existing) = self.custom_ai.iter_mut().find(|c| c.name == custom.name) {
                    *existing = custom;
                } else {
                    self.custom_ai.push(custom);
                }
            }
        } else if proj.diff.difft_bin.is_some() || !proj.custom_ai.is_empty() {
            eprintln!(
                "Security notice: ignoring `diff.difft_bin` / `[[custom_ai]]` in untrusted project `.codeduck`; configure executables in ~/.codeduck/configs.toml."
            );
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
            self.merge_project_config(proj, false);
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

    /// Persist user-selected preferences (UI language, AI review settings, diff view options)
    /// into `~/.codeduck/configs.toml` (or `explicit_path`) while preserving existing comments
    /// and unrelated sections such as `[ignore]` and `[[custom_ai]]`.
    pub fn save_user_settings(&self, explicit_path: Option<&Path>) -> Result<PathBuf> {
        self.save_user_settings_mask(explicit_path, UserSettingsMask::all())
    }

    /// Persist only user settings flagged in `mask` into `~/.codeduck/configs.toml` (or `explicit_path`),
    /// avoiding leaking unedited project-level `.codeduck` overrides into the user's global file.
    pub fn save_user_settings_mask(
        &self,
        explicit_path: Option<&Path>,
        mask: UserSettingsMask,
    ) -> Result<PathBuf> {
        let target = explicit_path
            .map(PathBuf::from)
            .or_else(Self::config_path)
            .context("Could not determine codeduck config file path")?;

        let mut doc = if target.is_file() {
            let existing = std::fs::read_to_string(&target)
                .with_context(|| format!("Failed to read {}", target.display()))?;
            existing.parse::<DocumentMut>().with_context(|| {
                format!("Failed to parse existing TOML in {}", target.display())
            })?
        } else {
            let mut d = DocumentMut::new();
            d.decor_mut()
                .set_prefix("# codeduck user configuration (~/.codeduck/configs.toml)\n\n");
            d
        };

        if mask.ui_language {
            ensure_table(&mut doc, "ui")?
                .insert("language", Item::Value(Value::from(self.ui_language.as_str())));
        }

        if mask.ai_provider || mask.review_language || mask.cache_ai_results || mask.reuse_session {
            let ai_tbl = ensure_table(&mut doc, "ai")?;
            if mask.ai_provider {
                ai_tbl.insert("provider", Item::Value(Value::from(self.ai_provider.as_str())));
            }
            if mask.review_language {
                ai_tbl.insert(
                    "language",
                    Item::Value(Value::from(self.review_language.as_str())),
                );
            }
            if mask.cache_ai_results {
                ai_tbl.insert(
                    "cache_ai_results",
                    Item::Value(Value::from(self.cache_ai_results)),
                );
            }
            if mask.reuse_session {
                ai_tbl.insert("reuse_session", Item::Value(Value::from(self.reuse_session)));
            }
        }

        if mask.display_mode || mask.ignore_comments || mask.cache_history {
            let diff_tbl = ensure_table(&mut doc, "diff")?;
            if mask.display_mode {
                diff_tbl.insert(
                    "display_mode",
                    Item::Value(Value::from(self.display_mode.as_cli_arg())),
                );
            }
            if mask.ignore_comments {
                diff_tbl.insert(
                    "ignore_comments",
                    Item::Value(Value::from(self.ignore_comments)),
                );
            }
            if mask.cache_history {
                diff_tbl.insert("cache_history", Item::Value(Value::from(self.cache_history)));
            }
        }

        atomic_write_file(&target, &doc.to_string())?;
        Ok(target)
    }
}

fn ensure_table<'a>(
    doc: &'a mut DocumentMut,
    name: &str,
) -> Result<&'a mut dyn toml_edit::TableLike> {
    if !doc.contains_key(name) {
        doc.insert(name, Item::Table(Table::new()));
    }
    doc.get_mut(name)
        .and_then(|item| item.as_table_like_mut())
        .with_context(|| format!("Expected [{}] to be a TOML table in configs.toml", name))
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
    use std::io::Write;
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temp file in {}", parent.display()))?;
    tmp.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write temporary file for {}", target.display()))?;
    tmp.persist(target)
        .map_err(|e| anyhow::anyhow!("Failed to replace {}: {}", target.display(), e.error))?;
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
        match existing.parse::<DocumentMut>() {
            Ok(d) => d,
            Err(err) => {
                let backup = sfile.with_extension("toml.bak");
                let _ = std::fs::copy(sfile, &backup);
                anyhow::bail!(
                    "Failed to parse {}; original preserved and backed up to {}: {}",
                    sfile.display(),
                    backup.display(),
                    err
                );
            }
        }
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
