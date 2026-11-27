use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

use crate::ignore::IgnoreMatcher;
use crate::model::{ChangeKind, CommitInfo, DiffTarget, FileChange, VcsKind};
use crate::vcs::{DiffBundle, VcsBackend, looks_binary};

pub struct GitBackend {
    repo_root: PathBuf,
    scope_dir: PathBuf,
}

impl GitBackend {
    pub fn open(dir: &Path) -> Result<Self> {
        let scope_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&scope_dir)
            .output()
            .context("Failed to execute `git rev-parse`")?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("Not a Git repository: {}", err.trim());
        }

        let root = String::from_utf8(output.stdout)?.trim().to_string();
        let root_path = PathBuf::from(root);
        let repo_root = std::fs::canonicalize(&root_path).unwrap_or(root_path);
        Ok(Self {
            repo_root,
            scope_dir,
        })
    }

    fn run_git(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.run_git_in(&self.repo_root, args)
    }

    fn run_git_in(&self, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .args(["-c", "core.quotepath=false"])
            .args(args)
            .current_dir(dir)
            .output()
            .with_context(|| format!("Failed to run `git {}`", args.join(" ")))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("`git {}` failed: {}", args.join(" "), err.trim());
        }
        Ok(output.stdout)
    }

    fn git_show_optional_in(&self, dir: &Path, spec: &str) -> Option<Vec<u8>> {
        let output = Command::new("git")
            .args(["-c", "core.quotepath=false", "show", spec])
            .current_dir(dir)
            .output()
            .ok()?;
        if output.status.success() {
            Some(output.stdout)
        } else {
            None
        }
    }

    fn has_head_in(&self, dir: &Path) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn resolve_head_in(&self, dir: &Path) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(dir)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!sha.is_empty()).then_some(sha)
    }

    fn is_zero_sha(sha: &str) -> bool {
        !sha.is_empty() && sha.bytes().all(|b| b == b'0')
    }

    fn gitlink_placeholder(sha: &str) -> Vec<u8> {
        format!("Submodule commit {}\n", sha).into_bytes()
    }

    fn prefix_path(prefix: &str, inner: &str) -> String {
        if prefix.is_empty() {
            inner.to_string()
        } else {
            format!("{}/{}", prefix, inner)
        }
    }

    fn parse_submodule_status_line(&self, line: &str) -> Option<String> {
        // Format: `<state><sha> <path>[ (<describe>)]`
        // Leading `-` marks an uninitialized submodule (not checked out on disk).
        let first = line.chars().next()?;
        if first == '-' {
            return None;
        }
        let after_state = &line[first.len_utf8()..];
        let sep_idx = after_state.find(|c: char| c.is_ascii_whitespace())?;
        let rest = after_state[sep_idx..].trim();
        if rest.is_empty() {
            return None;
        }
        let path_str = if rest.ends_with(')') {
            if let Some(paren_idx) = rest.rfind(" (") {
                let before = &rest[..paren_idx];
                if self.repo_root.join(before).exists() || !self.repo_root.join(rest).exists() {
                    before
                } else {
                    rest
                }
            } else {
                rest
            }
        } else {
            rest
        };
        let norm = path_str
            .trim()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .replace('\\', "/");
        (!norm.is_empty()).then_some(norm)
    }

    /// Discover initialized submodule prefixes (including nested `--recursive` submodules)
    /// relative to `self.repo_root`, constrained to `self.scope_dir` and `ignore`.
    fn initialized_submodules(&self, ignore: Option<&IgnoreMatcher>) -> Vec<String> {
        if !self.repo_root.join(".gitmodules").is_file() {
            return Vec::new();
        }
        let Ok(out) = self.run_git(&["submodule", "status", "--recursive"]) else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out);
        let mut subs = Vec::new();
        let mut seen = HashSet::new();
        let scoped = self.scope_dir != self.repo_root;

        for line in text.lines() {
            let Some(rel) = self.parse_submodule_status_line(line) else {
                continue;
            };
            let sub_dir = self.repo_root.join(&rel);
            // Verify `.git` exists (file or dir) so git commands run inside `sub_dir`
            // never walk back up into the superproject `.git`.
            if !sub_dir.exists() || !sub_dir.join(".git").exists() {
                continue;
            }
            if scoped {
                let sub_canon =
                    std::fs::canonicalize(&sub_dir).unwrap_or_else(|_| sub_dir.clone());
                if !sub_canon.starts_with(&self.scope_dir) {
                    continue;
                }
            }
            if ignore.is_some_and(|m| m.is_ignored(&self.repo_root, &rel)) {
                continue;
            }
            if seen.insert(rel.clone()) {
                subs.push(rel);
            }
        }
        subs
    }

    fn validate_rev(rev: &str) -> Result<&str> {
        let trimmed = rev.trim();
        if trimmed.is_empty() {
            bail!("Git revision cannot be empty");
        }
        if trimmed.starts_with('-') || trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
            bail!("Invalid git revision `{}`", trimmed);
        }
        Ok(trimmed)
    }

    fn collect_uncommitted(&self, ignore: Option<&IgnoreMatcher>) -> Result<DiffBundle> {
        let subs = self.initialized_submodules(ignore);
        let child_subs_for = |parent_prefix: &str| -> HashSet<String> {
            if parent_prefix.is_empty() {
                subs.iter().cloned().collect()
            } else {
                let pfx = format!("{}/", parent_prefix);
                subs.iter()
                    .filter_map(|s| s.strip_prefix(&pfx).map(str::to_string))
                    .collect()
            }
        };

        let mut changes = Vec::new();
        let mut seen = HashSet::new();

        let root_children = child_subs_for("");
        self.collect_uncommitted_in(
            &self.repo_root,
            &self.scope_dir,
            "",
            &root_children,
            ignore,
            &mut changes,
            &mut seen,
        )?;

        for sub in &subs {
            let sub_dir = self.repo_root.join(sub);
            if !sub_dir.join(".git").exists() {
                continue;
            }
            let sub_children = child_subs_for(sub);
            // Scan errors inside a single submodule should not hide main-repo changes.
            let _ = self.collect_uncommitted_in(
                &sub_dir,
                &sub_dir,
                sub,
                &sub_children,
                ignore,
                &mut changes,
                &mut seen,
            );
        }

        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(DiffBundle {
            vcs: VcsKind::Git,
            repo_root: self.repo_root.clone(),
            path_base_dir: self.repo_root.clone(),
            target: DiffTarget::Uncommitted,
            commit_info: None,
            changes,
        })
    }

    fn collect_uncommitted_in(
        &self,
        repo_dir: &Path,
        scan_dir: &Path,
        prefix: &str,
        skip_dirty_subs: &HashSet<String>,
        ignore: Option<&IgnoreMatcher>,
        changes: &mut Vec<FileChange>,
        seen: &mut HashSet<String>,
    ) -> Result<()> {
        let has_head = self.has_head_in(repo_dir);

        if has_head {
            let out = self.run_git_in(
                scan_dir,
                &["diff", "--raw", "--no-abbrev", "-M", "HEAD", "--", "."],
            )?;
            let text = String::from_utf8_lossy(&out);
            for line in text.lines() {
                if let Some(fc) = self.parse_raw_diff_line(
                    repo_dir,
                    prefix,
                    line,
                    Some("HEAD"),
                    None,
                    skip_dirty_subs,
                    ignore,
                )? {
                    if seen.insert(fc.path.clone()) {
                        changes.push(fc);
                    }
                }
            }
        }

        // Also check untracked or staged files on unborn HEAD via NUL-delimited `git status --porcelain -z -uall`
        let status_out =
            self.run_git_in(scan_dir, &["status", "--porcelain", "-z", "-uall", "--", "."])?;
        let mut iter = status_out.split(|&b| b == 0).filter(|s| !s.is_empty());
        while let Some(entry_bytes) = iter.next() {
            if entry_bytes.len() < 4 {
                continue;
            }
            let entry = String::from_utf8_lossy(entry_bytes);
            let mut chars = entry.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            let _sp = chars.next();
            let inner_path: String = chars.collect();
            // In `-z` format, R/C entries emit a second NUL-terminated token for orig_path; consume it.
            if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
                let _ = iter.next();
            }
            if inner_path.is_empty() {
                continue;
            }
            let path = Self::prefix_path(prefix, &inner_path);
            if seen.contains(&path) {
                continue;
            }
            if (x == '?' && y == '?') || !has_head {
                if inner_path == ".codeduck" || inner_path.ends_with("/.codeduck") {
                    continue;
                }
                if ignore.is_some_and(|m| m.is_ignored(&self.repo_root, &path)) {
                    continue;
                }
                let abs = repo_dir.join(&inner_path);
                if abs.is_dir() {
                    continue;
                }
                let new_content = std::fs::read(&abs).ok();
                let is_binary = new_content.as_deref().is_some_and(looks_binary);
                let new_size = new_content.as_ref().map(|b| b.len());
                seen.insert(path.clone());
                changes.push(FileChange {
                    path,
                    old_path: None,
                    kind: ChangeKind::Added,
                    is_binary,
                    old_content: None,
                    new_content,
                    old_size: None,
                    new_size,
                    prop_changes: None,
                });
            }
        }
        Ok(())
    }

    fn parse_raw_diff_line(
        &self,
        repo_dir: &Path,
        prefix: &str,
        line: &str,
        old_rev: Option<&str>,
        new_rev: Option<&str>,
        skip_dirty_subs: &HashSet<String>,
        ignore: Option<&IgnoreMatcher>,
    ) -> Result<Option<FileChange>> {
        let Some((meta, rest)) = line.split_once('\t') else {
            return Ok(None);
        };
        let meta_fields: Vec<&str> = meta.trim_start_matches(':').split_whitespace().collect();
        if meta_fields.len() < 5 {
            return Ok(None);
        }
        let (old_mode, new_mode, old_sha, new_sha, status) = (
            meta_fields[0],
            meta_fields[1],
            meta_fields[2],
            meta_fields[3],
            meta_fields[4],
        );

        let parts: Vec<&str> = rest.split('\t').collect();
        if parts.is_empty() {
            return Ok(None);
        }

        let (kind, inner_old_path, inner_path) = if status.starts_with('R') && parts.len() >= 2 {
            let from = Self::prefix_path(prefix, parts[0]);
            (
                ChangeKind::Renamed { from },
                Some(parts[0].to_string()),
                parts[1].to_string(),
            )
        } else if status.starts_with('C') && parts.len() >= 2 {
            (ChangeKind::Added, None, parts[1].to_string())
        } else if status.starts_with('A') {
            (ChangeKind::Added, None, parts[0].to_string())
        } else if status.starts_with('D') {
            (ChangeKind::Deleted, None, parts[0].to_string())
        } else {
            (ChangeKind::Modified, None, parts[0].to_string())
        };

        let path = Self::prefix_path(prefix, &inner_path);
        let old_path = inner_old_path.as_deref().map(|p| Self::prefix_path(prefix, p));

        if ignore.is_some_and(|m| m.is_ignored(&self.repo_root, &path)) {
            return Ok(None);
        }

        let is_gitlink = old_mode == "160000" || new_mode == "160000";
        let eff_new_sha = if is_gitlink && new_mode == "160000" {
            if new_rev.is_some() || !Self::is_zero_sha(new_sha) {
                Some(new_sha.to_string())
            } else {
                let sub_dir = repo_dir.join(&inner_path);
                self.resolve_head_in(&sub_dir)
            }
        } else {
            None
        };

        // When scanning uncommitted changes inside initialized submodules, a dirty worktree whose
        // checked-out commit pointer has not changed shows up in the parent as a 160000 entry with
        // matching SHAs; skip that redundant parent entry since its inner files are expanded directly.
        if new_rev.is_none()
            && old_mode == "160000"
            && new_mode == "160000"
            && skip_dirty_subs.contains(&inner_path)
            && !Self::is_zero_sha(old_sha)
            && eff_new_sha.as_deref() == Some(old_sha)
        {
            return Ok(None);
        }

        let old_content = match (&kind, old_rev) {
            (ChangeKind::Added, _) => None,
            _ if old_mode == "160000" && !Self::is_zero_sha(old_sha) => {
                Some(Self::gitlink_placeholder(old_sha))
            }
            (_, Some(rev)) => {
                let p = inner_old_path.as_deref().unwrap_or(&inner_path);
                self.git_show_optional_in(repo_dir, &format!("{}:{}", rev, p))
            }
            (_, None) => None,
        };

        let new_content = match (&kind, new_rev) {
            (ChangeKind::Deleted, _) => None,
            _ if new_mode == "160000" => {
                eff_new_sha
                    .as_deref()
                    .filter(|s| !Self::is_zero_sha(s))
                    .map(Self::gitlink_placeholder)
            }
            (_, Some(rev)) => self.git_show_optional_in(repo_dir, &format!("{}:{}", rev, inner_path)),
            (_, None) => std::fs::read(repo_dir.join(&inner_path)).ok(),
        };

        let is_binary = old_content.as_deref().is_some_and(looks_binary)
            || new_content.as_deref().is_some_and(looks_binary);
        let old_size = old_content.as_ref().map(|b| b.len());
        let new_size = new_content.as_ref().map(|b| b.len());

        Ok(Some(FileChange {
            path,
            old_path,
            kind,
            is_binary,
            old_content,
            new_content,
            old_size,
            new_size,
            prop_changes: None,
        }))
    }

    fn fetch_commit_info(&self, rev: &str) -> Option<CommitInfo> {
        let valid_rev = Self::validate_rev(rev).ok()?;
        let out = self
            .run_git(&["show", "-s", "--format=%H%n%an%n%aI%n%B", valid_rev])
            .ok()?;
        let s = String::from_utf8_lossy(&out);
        let mut lines = s.lines();
        let revision = lines.next()?.chars().take(12).collect();
        let author = lines.next().unwrap_or("unknown").to_string();
        let date = lines.next().unwrap_or("").to_string();
        let message = lines.collect::<Vec<_>>().join("\n").trim().to_string();
        Some(CommitInfo {
            revision,
            author,
            date,
            message,
            changed_paths_count: None,
        })
    }
}

impl VcsBackend for GitBackend {
    fn kind(&self) -> VcsKind {
        VcsKind::Git
    }

    fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn scope_dir(&self) -> &Path {
        &self.scope_dir
    }

    fn collect_changes_filtered(
        &self,
        target: &DiffTarget,
        ignore: Option<&IgnoreMatcher>,
    ) -> Result<DiffBundle> {
        let bundle = match target {
            DiffTarget::Uncommitted => self.collect_uncommitted(ignore),
            DiffTarget::Commit(rev) => {
                let valid_rev = Self::validate_rev(rev)?;
                let info = self.fetch_commit_info(valid_rev);
                let out = self.run_git(&[
                    "diff-tree",
                    "--no-commit-id",
                    "--raw",
                    "--no-abbrev",
                    "-r",
                    "-M",
                    "--first-parent",
                    "-m",
                    "--root",
                    valid_rev,
                ])?;
                let text = String::from_utf8_lossy(&out);
                let parent_rev = format!("{}^", valid_rev);
                let has_parent = self.run_git(&["rev-parse", "--verify", &parent_rev]).is_ok();
                let empty_subs = HashSet::new();
                let mut changes = Vec::new();
                for line in text.lines() {
                    if let Some(fc) = self.parse_raw_diff_line(
                        &self.repo_root,
                        "",
                        line,
                        if has_parent { Some(parent_rev.as_str()) } else { None },
                        Some(valid_rev),
                        &empty_subs,
                        ignore,
                    )? {
                        changes.push(fc);
                    }
                }
                changes.sort_by(|a, b| a.path.cmp(&b.path));
                Ok(DiffBundle {
                    vcs: VcsKind::Git,
                    repo_root: self.repo_root.clone(),
                    path_base_dir: self.repo_root.clone(),
                    target: target.clone(),
                    commit_info: info,
                    changes,
                })
            }
            DiffTarget::Range { from, to } => {
                let valid_from = Self::validate_rev(from)?;
                let valid_to = Self::validate_rev(to)?;
                let out = self.run_git(&[
                    "diff",
                    "--raw",
                    "--no-abbrev",
                    "-M",
                    valid_from,
                    valid_to,
                ])?;
                let text = String::from_utf8_lossy(&out);
                let empty_subs = HashSet::new();
                let mut changes = Vec::new();
                for line in text.lines() {
                    if let Some(fc) = self.parse_raw_diff_line(
                        &self.repo_root,
                        "",
                        line,
                        Some(valid_from),
                        Some(valid_to),
                        &empty_subs,
                        ignore,
                    )? {
                        changes.push(fc);
                    }
                }
                changes.sort_by(|a, b| a.path.cmp(&b.path));
                Ok(DiffBundle {
                    vcs: VcsKind::Git,
                    repo_root: self.repo_root.clone(),
                    path_base_dir: self.repo_root.clone(),
                    target: target.clone(),
                    commit_info: None,
                    changes,
                })
            }
        }?;
        Ok(bundle)
    }

    fn recent_commits(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        let limit_arg = format!("-n{}", limit);
        let out = self.run_git(&["log", &limit_arg, "--format=%h%x1f%an%x1f%aI%x1f%s%x1e"])?;
        let text = String::from_utf8_lossy(&out);
        let mut commits = Vec::new();
        for rec in text.split('\x1e') {
            let rec = rec.trim();
            if rec.is_empty() {
                continue;
            }
            let fields: Vec<&str> = rec.split('\x1f').collect();
            if fields.len() >= 4 {
                commits.push(CommitInfo {
                    revision: fields[0].to_string(),
                    author: fields[1].to_string(),
                    date: fields[2].to_string(),
                    message: fields[3].to_string(),
                    changed_paths_count: None,
                });
            }
        }
        Ok(commits)
    }

    fn latest_revision(&self) -> Result<Option<String>> {
        let Ok(out) = self.run_git(&["rev-parse", "--verify", "HEAD"]) else {
            return Ok(None);
        };
        let sha = String::from_utf8_lossy(&out).trim().to_string();
        if sha.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sha))
        }
    }

    fn resolve_immutable_target(&self, target: &DiffTarget) -> Option<DiffTarget> {
        let resolve_sha = |rev: &str| -> Option<String> {
            let valid = Self::validate_rev(rev).ok()?;
            let spec = format!("{}^{{commit}}", valid);
            let out = self.run_git(&["rev-parse", "--verify", &spec]).ok()?;
            let sha = String::from_utf8_lossy(&out).trim().to_string();
            (!sha.is_empty()).then_some(sha)
        };
        match target {
            DiffTarget::Uncommitted => None,
            DiffTarget::Commit(rev) => resolve_sha(rev).map(DiffTarget::Commit),
            DiffTarget::Range { from, to } => Some(DiffTarget::Range {
                from: resolve_sha(from)?,
                to: resolve_sha(to)?,
            }),
        }
    }

    fn external_prefixes(&self) -> Vec<String> {
        self.initialized_submodules(None)
    }

    fn commit_files(&self, paths: &[String], message: &str) -> Result<String> {
        let outcome = self.commit_files_detailed(paths, message);
        if let Some(err) = outcome.error {
            if outcome.summary.is_empty() {
                bail!("{}", err);
            } else {
                let locale = crate::i18n::current_thread_locale();
                bail!(
                    "{}",
                    rust_i18n::t!(
                        "vcs.partial_success",
                        locale = locale.as_str(),
                        summary = outcome.summary,
                        err = err
                    )
                );
            }
        }
        Ok(outcome.summary)
    }

    fn commit_files_detailed(
        &self,
        paths: &[String],
        message: &str,
    ) -> crate::vcs::CommitOutcome {
        self.commit_files_with_external_message(paths, message, None)
    }

    fn commit_files_with_external_message(
        &self,
        paths: &[String],
        message: &str,
        external_message: Option<&str>,
    ) -> crate::vcs::CommitOutcome {
        let make_err = |err: String| crate::vcs::CommitOutcome {
            committed_paths: Vec::new(),
            failed_paths: paths.to_vec(),
            summary: String::new(),
            error: Some(err),
        };
        if paths.is_empty() {
            return make_err("No files selected to commit".to_string());
        }

        let selected_set: HashSet<&str> = paths.iter().map(|s| s.as_str()).collect();
        let rename_old_map = self.collect_rename_old_map(&selected_set);
        let mut groups = self.group_paths_by_submodule(paths);
        let has_ext = groups.keys().any(|k| !k.is_empty());
        // In Git, committing inside a submodule advances its HEAD and dirty-updates its parent
        // gitlink pointer, so main/parent repo always needs a commit message whenever `has_ext`.
        let needs_main = groups.contains_key("") || has_ext;
        let eff_ext_msg = external_message
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| message.trim());

        if needs_main && message.trim().is_empty() {
            return make_err("Commit message cannot be empty".to_string());
        }
        if has_ext && eff_ext_msg.is_empty() {
            return make_err("External commit message cannot be empty".to_string());
        }

        let write_temp = |content: &str| -> Result<NamedTempFile> {
            let mut f = NamedTempFile::new().context("Failed to create temp commit message file")?;
            f.write_all(content.as_bytes())
                .context("Failed to write temp commit message file")?;
            Ok(f)
        };
        let main_msg_file = match write_temp(if message.trim().is_empty() { eff_ext_msg } else { message }) {
            Ok(f) => f,
            Err(e) => return make_err(format!("{:#}", e)),
        };
        let ext_msg_file = match write_temp(eff_ext_msg) {
            Ok(f) => f,
            Err(e) => return make_err(format!("{:#}", e)),
        };
        let main_msg_path = main_msg_file.path().to_string_lossy().to_string();
        let ext_msg_path = ext_msg_file.path().to_string_lossy().to_string();

        let mut subs_desc = self.initialized_submodules(None);
        subs_desc.sort_by_key(|b| std::cmp::Reverse(b.len()));

        let multi_or_ext = groups.len() > 1 || has_ext;
        let mut pending: HashSet<String> = groups.keys().cloned().collect();
        let mut extra_gitlinks_by_parent: BTreeMap<String, Vec<String>> = BTreeMap::new();

        let mut outputs = Vec::new();
        let mut errors = Vec::new();
        let mut committed_paths = Vec::new();
        let mut failed_paths = Vec::new();
        let locale = crate::i18n::current_thread_locale();

        // Process deepest submodules first so parent repos can fold updated gitlinks atomically.
        while let Some(prefix) = pending
            .iter()
            .max_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .cloned()
        {
            pending.remove(&prefix);
            let entries = groups.remove(&prefix).unwrap_or_default();
            let extra_gitlinks = extra_gitlinks_by_parent.remove(&prefix).unwrap_or_default();
            let repo_dir = self.repo_dir_for_prefix(&prefix);
            let label = if prefix.is_empty() {
                rust_i18n::t!("vcs.main_repo", locale = locale.as_str()).into_owned()
            } else {
                format!("external[{}]", prefix)
            };

            let mut inner_paths: Vec<String> = Vec::new();
            for (inner_p, orig_p) in &entries {
                if !inner_paths.contains(inner_p) {
                    inner_paths.push(inner_p.clone());
                }
                // Include paired old paths for any renamed files so `git add` & `git commit` handle deletions too.
                if let Some(old_full) = rename_old_map.get(orig_p) {
                    let old_inner = Self::strip_repo_prefix(&prefix, old_full);
                    if !inner_paths.contains(&old_inner) {
                        inner_paths.push(old_inner);
                    }
                }
            }
            for gl in extra_gitlinks {
                let changed_in_parent = !self.has_head_in(&repo_dir)
                    || self
                        .run_git_in(&repo_dir, &["status", "--porcelain", "--", &gl])
                        .map(|o| !String::from_utf8_lossy(&o).trim().is_empty())
                        .unwrap_or(false);
                if changed_in_parent && !inner_paths.contains(&gl) {
                    inner_paths.push(gl);
                }
            }
            if inner_paths.is_empty() {
                continue;
            }

            let msg_path = if prefix.is_empty() {
                &main_msg_path
            } else {
                &ext_msg_path
            };
            match self.commit_files_in_repo(&repo_dir, &inner_paths, msg_path) {
                Ok(summary) => {
                    for (_, orig) in entries {
                        committed_paths.push(orig);
                    }
                    if multi_or_ext {
                        outputs.push(format!("[{}] {}", label, summary));
                    } else {
                        outputs.push(summary);
                    }
                    // Queue parent gitlink update after a submodule commit succeeds.
                    if !prefix.is_empty() {
                        let (parent_prefix, gitlink_inner) =
                            Self::parent_repo_of_submodule(&subs_desc, &prefix);
                        extra_gitlinks_by_parent
                            .entry(parent_prefix.clone())
                            .or_default()
                            .push(gitlink_inner);
                        pending.insert(parent_prefix);
                    }
                }
                Err(e) => {
                    for (_, orig) in entries {
                        failed_paths.push(orig);
                    }
                    errors.push(
                        rust_i18n::t!(
                            "vcs.git_commit_failed",
                            locale = locale.as_str(),
                            label = label,
                            err = format!("{:#}", e)
                        )
                        .into_owned(),
                    );
                }
            }
        }

        crate::vcs::CommitOutcome {
            committed_paths,
            failed_paths,
            summary: outputs.join(" | "),
            error: (!errors.is_empty()).then(|| errors.join(" ; ")),
        }
    }

    fn revert_files(&self, paths: &[String]) -> Result<String> {
        let outcome = self.revert_files_detailed(paths);
        if let Some(err) = outcome.error {
            if outcome.summary.is_empty() {
                bail!("{}", err);
            } else {
                let locale = crate::i18n::current_thread_locale();
                bail!(
                    "{}",
                    rust_i18n::t!(
                        "vcs.partial_success",
                        locale = locale.as_str(),
                        summary = outcome.summary,
                        err = err
                    )
                );
            }
        }
        Ok(outcome.summary)
    }

    fn revert_files_detailed(&self, paths: &[String]) -> crate::vcs::RevertOutcome {
        let make_err = |err: String| crate::vcs::RevertOutcome {
            reverted_paths: Vec::new(),
            failed_paths: paths.to_vec(),
            summary: String::new(),
            error: Some(err),
        };
        if paths.is_empty() {
            return make_err("No files selected to revert".to_string());
        }

        let selected_set: HashSet<&str> = paths.iter().map(|s| s.as_str()).collect();
        let rename_old_map = self.collect_rename_old_map(&selected_set);
        let groups = self.group_paths_by_submodule(paths);
        let multi_or_ext = groups.len() > 1 || groups.keys().any(|k| !k.is_empty());

        let mut outputs = Vec::new();
        let mut errors = Vec::new();
        let mut reverted_paths = Vec::new();
        let mut failed_paths = Vec::new();
        let locale = crate::i18n::current_thread_locale();

        for (prefix, entries) in groups {
            let repo_dir = self.repo_dir_for_prefix(&prefix);
            let label = if prefix.is_empty() {
                rust_i18n::t!("vcs.main_repo", locale = locale.as_str()).into_owned()
            } else {
                format!("external[{}]", prefix)
            };
            let mut inner_paths = Vec::new();
            let mut local_rename_map = HashMap::new();
            for (inner_p, orig_p) in &entries {
                inner_paths.push(inner_p.clone());
                if let Some(old_full) = rename_old_map.get(orig_p) {
                    local_rename_map
                        .insert(inner_p.clone(), Self::strip_repo_prefix(&prefix, old_full));
                }
            }

            match self.revert_files_in_repo(&repo_dir, &inner_paths, &local_rename_map) {
                Ok(()) => {
                    let cnt = entries.len();
                    for (_, orig) in entries {
                        reverted_paths.push(orig);
                    }
                    if multi_or_ext {
                        outputs.push(
                            rust_i18n::t!(
                                "vcs.git_reverted_repo",
                                locale = locale.as_str(),
                                label = label,
                                count = cnt
                            )
                            .into_owned(),
                        );
                    } else {
                        outputs.push(
                            rust_i18n::t!(
                                "vcs.git_reverted",
                                locale = locale.as_str(),
                                count = cnt
                            )
                            .into_owned(),
                        );
                    }
                }
                Err(e) => {
                    for (_, orig) in entries {
                        failed_paths.push(orig);
                    }
                    errors.push(
                        rust_i18n::t!(
                            "vcs.git_revert_failed",
                            locale = locale.as_str(),
                            label = label,
                            err = format!("{:#}", e)
                        )
                        .into_owned(),
                    );
                }
            }
        }

        crate::vcs::RevertOutcome {
            reverted_paths,
            failed_paths,
            summary: outputs.join(" | "),
            error: (!errors.is_empty()).then(|| errors.join(" ; ")),
        }
    }
}

impl GitBackend {
    fn group_paths_by_submodule(&self, paths: &[String]) -> BTreeMap<String, Vec<(String, String)>> {
        let mut subs = self.initialized_submodules(None);
        subs.sort_by_key(|b| std::cmp::Reverse(b.len()));

        let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for p in paths {
            let norm = p.trim_start_matches("./").to_string();
            // Only paths *inside* a submodule (`<sub_prefix>/...`) run inside that submodule repo.
            // A bare `<sub_prefix>` gitlink entry itself is committed/reverted in its parent repo.
            if let Some(sub) = subs
                .iter()
                .find(|s| norm.starts_with(&format!("{}/", s)))
            {
                let inner = norm
                    .strip_prefix(&format!("{}/", sub))
                    .unwrap_or(&norm)
                    .to_string();
                groups.entry(sub.clone()).or_default().push((inner, p.clone()));
            } else {
                groups.entry(String::new()).or_default().push((norm, p.clone()));
            }
        }
        groups
    }

    fn collect_rename_old_map(&self, selected: &HashSet<&str>) -> HashMap<String, String> {
        let mut map = HashMap::new();
        if let Ok(bundle) = self.collect_uncommitted(None) {
            for ch in bundle.changes {
                if selected.contains(ch.path.as_str()) {
                    if let Some(old_p) = ch.old_path {
                        map.insert(ch.path, old_p);
                    }
                }
            }
        }
        map
    }

    fn strip_repo_prefix(prefix: &str, full_path: &str) -> String {
        let norm = full_path.trim_start_matches("./");
        if prefix.is_empty() {
            norm.to_string()
        } else {
            norm.strip_prefix(&format!("{}/", prefix))
                .unwrap_or(norm)
                .to_string()
        }
    }

    fn parent_repo_of_submodule(subs_desc: &[String], sub_prefix: &str) -> (String, String) {
        if let Some(parent_sub) = subs_desc
            .iter()
            .find(|s| sub_prefix.starts_with(&format!("{}/", s)))
        {
            let inner = sub_prefix
                .strip_prefix(&format!("{}/", parent_sub))
                .unwrap_or(sub_prefix)
                .to_string();
            (parent_sub.clone(), inner)
        } else {
            (String::new(), sub_prefix.to_string())
        }
    }

    fn repo_dir_for_prefix(&self, prefix: &str) -> PathBuf {
        if prefix.is_empty() {
            self.repo_root.clone()
        } else {
            self.repo_root.join(prefix)
        }
    }

    fn commit_files_in_repo(
        &self,
        repo_dir: &Path,
        inner_paths: &[String],
        msg_path: &str,
    ) -> Result<String> {
        let add_paths: Vec<String> = inner_paths
            .iter()
            .filter(|p| {
                repo_dir.join(p).exists()
                    || self
                        .run_git_in(repo_dir, &["ls-files", "--error-unmatch", "--", p])
                        .is_ok()
            })
            .cloned()
            .collect();

        if !add_paths.is_empty() {
            let mut add_cmd = Command::new("git");
            add_cmd
                .args(["-c", "core.quotepath=false", "add", "-A", "--"])
                .current_dir(repo_dir);
            for p in &add_paths {
                add_cmd.arg(p);
            }
            let add_out = add_cmd.output().context("Failed to execute `git add`")?;
            if !add_out.status.success() {
                let err = String::from_utf8_lossy(&add_out.stderr);
                bail!("`git add` failed: {}", err.trim());
            }
        }

        let mut commit_cmd = Command::new("git");
        commit_cmd
            .args(["-c", "core.quotepath=false", "commit", "-F", msg_path, "--"])
            .current_dir(repo_dir);
        for p in inner_paths {
            commit_cmd.arg(p);
        }
        let out = commit_cmd.output().context("Failed to execute `git commit`")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            bail!(
                "`git commit` failed: {}{}",
                err.trim(),
                if err.trim().is_empty() {
                    stdout.trim()
                } else {
                    ""
                }
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("Git commit completed.")
            .trim()
            .to_string())
    }

    fn revert_files_in_repo(
        &self,
        repo_dir: &Path,
        inner_paths: &[String],
        rename_old_map: &HashMap<String, String>,
    ) -> Result<()> {
        let has_head = self.has_head_in(repo_dir);
        let mut head_restore_paths: Vec<String> = Vec::new();
        let mut remove_new_paths: Vec<String> = Vec::new();

        for p in inner_paths {
            let in_head = if has_head {
                self.run_git_in(repo_dir, &["ls-tree", "-r", "--name-only", "HEAD", "--", p])
                    .map(|out| !String::from_utf8_lossy(&out).trim().is_empty())
                    .unwrap_or(false)
            } else {
                false
            };

            if in_head {
                if !head_restore_paths.contains(p) {
                    head_restore_paths.push(p.clone());
                }
            } else {
                if !remove_new_paths.contains(p) {
                    remove_new_paths.push(p.clone());
                }
                if let Some(old_p) = rename_old_map.get(p) {
                    if !head_restore_paths.contains(old_p) {
                        head_restore_paths.push(old_p.clone());
                    }
                }
            }
        }

        if !remove_new_paths.is_empty() {
            if has_head {
                let mut reset_cmd = Command::new("git");
                reset_cmd
                    .args(["-c", "core.quotepath=false", "reset", "-q", "HEAD", "--"])
                    .current_dir(repo_dir);
                for p in &remove_new_paths {
                    reset_cmd.arg(p);
                }
                let _ = reset_cmd.output();
            } else {
                let mut rm_cmd = Command::new("git");
                rm_cmd
                    .args([
                        "-c",
                        "core.quotepath=false",
                        "rm",
                        "-f",
                        "--cached",
                        "--ignore-unmatch",
                        "--",
                    ])
                    .current_dir(repo_dir);
                for p in &remove_new_paths {
                    rm_cmd.arg(p);
                }
                let _ = rm_cmd.output();
            }

            for p in &remove_new_paths {
                let rel = Path::new(p);
                if rel.is_absolute()
                    || rel
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    bail!("Unsafe path to revert: {}", p);
                }
                let abs = repo_dir.join(rel);
                if abs.is_file() || abs.is_symlink() {
                    std::fs::remove_file(&abs).with_context(|| {
                        format!("Failed to remove added/untracked file {}", abs.display())
                    })?;
                }
            }
        }

        if !head_restore_paths.is_empty() {
            let mut restore_cmd = Command::new("git");
            restore_cmd
                .args([
                    "-c",
                    "core.quotepath=false",
                    "restore",
                    "--source=HEAD",
                    "--staged",
                    "--worktree",
                    "--",
                ])
                .current_dir(repo_dir);
            for p in &head_restore_paths {
                restore_cmd.arg(p);
            }
            let out = restore_cmd
                .output()
                .context("Failed to execute `git restore`")?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                bail!("`git restore` failed: {}", err.trim());
            }
        }
        Ok(())
    }
}
