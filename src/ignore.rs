use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::config::{ProjectCodeduckConfig, directory_chain};

pub const IGNORE_FILE_NAME: &str = ".codeduck";

#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    /// Ordered from lowest precedence (repo root) to highest precedence (scope_dir).
    layers: Vec<Gitignore>,
    primary_file: PathBuf,
    fingerprint: String,
}

impl IgnoreMatcher {
    /// Load ignore rules from global `~/.codeduck/configs.toml` (lowest precedence)
    /// followed by project `.codeduck` files along the directory chain from `vcs_root` to `scope_dir`.
    /// Supports TOML `[ignore].patterns` as well as legacy plain-text `.gitignore` format.
    pub fn load(vcs_root: &Path, scope_dir: &Path) -> Self {
        let global_cfg = crate::config::AppConfig::config_path();
        Self::load_with_global(vcs_root, scope_dir, global_cfg.as_deref())
    }

    pub fn load_with_global(
        vcs_root: &Path,
        scope_dir: &Path,
        global_config_file: Option<&Path>,
    ) -> Self {
        let vcs_root_canon =
            std::fs::canonicalize(vcs_root).unwrap_or_else(|_| vcs_root.to_path_buf());
        let scope_canon =
            std::fs::canonicalize(scope_dir).unwrap_or_else(|_| scope_dir.to_path_buf());
        let dirs_to_check = directory_chain(vcs_root, scope_dir);

        let mut layers = Vec::new();
        let mut hasher = Sha256::new();

        if let Some(gpath) = global_config_file {
            if gpath.is_file() {
                if let Ok(raw) = std::fs::read_to_string(gpath) {
                    if let Ok(proj) = toml::from_str::<ProjectCodeduckConfig>(&raw) {
                        if !proj.ignore.patterns.is_empty() {
                            let mut builder = GitignoreBuilder::new(&vcs_root_canon);
                            hasher.update(b"global:");
                            hasher.update(gpath.to_string_lossy().as_bytes());
                            hasher.update(b"\n");
                            for pat in &proj.ignore.patterns {
                                hasher.update(pat.as_bytes());
                                hasher.update(b"\n");
                                let _ = builder.add_line(None, pat);
                            }
                            if let Ok(gi) = builder.build() {
                                layers.push(gi);
                            }
                        }
                    }
                }
            }
        }

        for dir in &dirs_to_check {
            let file = dir.join(IGNORE_FILE_NAME);
            if !file.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&file) else {
                continue;
            };

            let mut builder = GitignoreBuilder::new(dir);
            match toml::from_str::<ProjectCodeduckConfig>(&raw) {
                Ok(proj) => {
                    hasher.update(dir.to_string_lossy().as_bytes());
                    hasher.update(b"\n");
                    for pat in &proj.ignore.patterns {
                        hasher.update(pat.as_bytes());
                        hasher.update(b"\n");
                        let _ = builder.add_line(None, pat);
                    }
                    if let Ok(gi) = builder.build() {
                        layers.push(gi);
                    }
                }
                Err(err) => {
                    let looks_like_toml = raw.lines().any(|l| {
                        let t = l.trim();
                        t == "[ai]" || t == "[diff]" || t == "[ignore]"
                    });
                    if looks_like_toml {
                        eprintln!(
                            "Warning: failed to parse TOML in {}: {} (ignore rules skipped)",
                            file.display(),
                            err
                        );
                    } else {
                        // Backward compatibility: plain-text .gitignore-style .codeduck
                        hasher.update(dir.to_string_lossy().as_bytes());
                        hasher.update(b"\n");
                        hasher.update(raw.as_bytes());
                        hasher.update(b"\n");
                        if builder.add(&file).is_none() {
                            if let Ok(gi) = builder.build() {
                                layers.push(gi);
                            }
                        }
                    }
                }
            }
        }

        let fingerprint = format!("{:x}", hasher.finalize());
        Self {
            layers,
            primary_file: scope_canon.join(IGNORE_FILE_NAME),
            fingerprint,
        }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn primary_file(&self) -> &Path {
        &self.primary_file
    }

    fn is_ignored_canon(&self, base_canon: &Path, rel_path: &str) -> bool {
        if self.layers.is_empty() {
            return false;
        }
        let abs = base_canon.join(rel_path);

        let mut ignored = false;
        for gi in &self.layers {
            match gi.matched_path_or_any_parents(&abs, false) {
                ignore::Match::Ignore(_) => ignored = true,
                ignore::Match::Whitelist(_) => ignored = false,
                ignore::Match::None => {}
            }
        }
        ignored
    }

    pub fn is_ignored(&self, base_dir: &Path, rel_path: &str) -> bool {
        if self.layers.is_empty() {
            return false;
        }
        let base_canon =
            std::fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf());
        self.is_ignored_canon(&base_canon, rel_path)
    }

    /// Append an ignore pattern to `primary_file` (`.codeduck`), preserving TOML structure & comments.
    /// Creates a new TOML `.codeduck` if it does not exist yet.
    pub fn append_pattern(&self, pattern: &str) -> Result<bool> {
        let clean = pattern.trim();
        if clean.is_empty() {
            return Ok(false);
        }

        if !self.primary_file.exists() {
            let mut arr = Array::new();
            arr.push(Value::from(clean));
            let mut ignore_tbl = Table::new();
            ignore_tbl.insert("patterns", Item::Value(Value::Array(arr)));

            let mut doc = DocumentMut::new();
            doc.insert("ignore", Item::Table(ignore_tbl));
            let content = format!(
                "# codeduck 项目配置（支持 [ai] provider/model/effort 与 [ignore].patterns）\n{}",
                doc
            );
            std::fs::write(&self.primary_file, content).with_context(|| {
                format!("Failed to write {}", self.primary_file.display())
            })?;
            return Ok(true);
        }

        let existing = std::fs::read_to_string(&self.primary_file)
            .with_context(|| format!("Failed to read {}", self.primary_file.display()))?;

        // If valid TOML, update [ignore].patterns in-place preserving all other sections & comments
        if let Ok(mut doc) = existing.parse::<DocumentMut>() {
            if !doc.contains_key("ignore") {
                let mut tbl = Table::new();
                tbl.insert("patterns", Item::Value(Value::Array(Array::new())));
                doc.insert("ignore", Item::Table(tbl));
            }
            let ignore_item = doc
                .get_mut("ignore")
                .and_then(|i| i.as_table_like_mut())
                .context("Expected [ignore] to be a TOML table in .codeduck")?;

            if !ignore_item.contains_key("patterns") {
                ignore_item.insert("patterns", Item::Value(Value::Array(Array::new())));
            }
            let arr = ignore_item
                .get_mut("patterns")
                .and_then(|i| i.as_array_mut())
                .context("Expected ignore.patterns to be a TOML array in .codeduck")?;

            let already_exists = arr
                .iter()
                .filter_map(|v| v.as_str())
                .any(|s| s.trim() == clean);
            if already_exists {
                return Ok(false);
            }

            arr.push(Value::from(clean));
            std::fs::write(&self.primary_file, doc.to_string()).with_context(|| {
                format!("Failed to write {}", self.primary_file.display())
            })?;
            return Ok(true);
        }

        // Legacy plain-text .gitignore-style .codeduck
        if existing.lines().any(|l| l.trim() == clean) {
            return Ok(false);
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.primary_file)
            .with_context(|| {
                format!("Failed to open {} for writing", self.primary_file.display())
            })?;

        if !existing.is_empty() && !existing.ends_with('\n') {
            writeln!(file)?;
        }
        writeln!(file, "{}", clean)?;
        Ok(true)
    }
}
