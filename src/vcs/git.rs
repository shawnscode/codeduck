use anyhow::{Context, Result, bail};
use std::collections::HashSet;
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

    fn git_show_optional(&self, spec: &str) -> Option<Vec<u8>> {
        let output = Command::new("git")
            .args(["-c", "core.quotepath=false", "show", spec])
            .current_dir(&self.repo_root)
            .output()
            .ok()?;
        if output.status.success() {
            Some(output.stdout)
        } else {
            None
        }
    }

    fn has_head(&self) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&self.repo_root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn collect_uncommitted(&self, ignore: Option<&IgnoreMatcher>) -> Result<DiffBundle> {
        let has_head = self.has_head();
        let mut changes = Vec::new();
        let mut seen = HashSet::new();

        if has_head {
            let out = self.run_git_in(
                &self.scope_dir,
                &["diff", "--name-status", "-M", "HEAD", "--", "."],
            )?;
            let text = String::from_utf8_lossy(&out);
            for line in text.lines() {
                if let Some(fc) = self.parse_name_status_line(line, Some("HEAD"), None, ignore)? {
                    if seen.insert(fc.path.clone()) {
                        changes.push(fc);
                    }
                }
            }
        }

        // Also check untracked or staged files on unborn HEAD via git status --porcelain -uall scoped to `self.scope_dir`
        let status_out =
            self.run_git_in(&self.scope_dir, &["status", "--porcelain", "-uall", "--", "."])?;
        let status_text = String::from_utf8_lossy(&status_out);
        for line in status_text.lines() {
            if line.len() < 4 {
                continue;
            }
            let code = &line[..2];
            let path_part = line[3..].trim();
            let path = path_part
                .split(" -> ")
                .last()
                .unwrap_or(path_part)
                .trim_matches('"')
                .to_string();
            if path.is_empty() || seen.contains(&path) {
                continue;
            }
            if code == "??" || !has_head {
                if path == ".codeduck" || path.ends_with("/.codeduck") {
                    continue;
                }
                if ignore.is_some_and(|m| m.is_ignored(&self.repo_root, &path)) {
                    continue;
                }
                let abs = self.repo_root.join(&path);
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

    fn parse_name_status_line(
        &self,
        line: &str,
        old_rev: Option<&str>,
        new_rev: Option<&str>,
        ignore: Option<&IgnoreMatcher>,
    ) -> Result<Option<FileChange>> {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            return Ok(None);
        }
        let status = parts[0];
        let (kind, old_path, path) = if status.starts_with('R') && parts.len() >= 3 {
            (
                ChangeKind::Renamed {
                    from: parts[1].to_string(),
                },
                Some(parts[1].to_string()),
                parts[2].to_string(),
            )
        } else if status.starts_with('A') {
            (ChangeKind::Added, None, parts[1].to_string())
        } else if status.starts_with('D') {
            (ChangeKind::Deleted, None, parts[1].to_string())
        } else {
            (ChangeKind::Modified, None, parts[1].to_string())
        };

        if ignore.is_some_and(|m| m.is_ignored(&self.repo_root, &path)) {
            return Ok(None);
        }

        let old_content = match (&kind, old_rev) {
            (ChangeKind::Added, _) => None,
            (_, Some(rev)) => {
                let p = old_path.as_deref().unwrap_or(&path);
                self.git_show_optional(&format!("{}:{}", rev, p))
            }
            (_, None) => None,
        };

        let new_content = match (&kind, new_rev) {
            (ChangeKind::Deleted, _) => None,
            (_, Some(rev)) => self.git_show_optional(&format!("{}:{}", rev, path)),
            (_, None) => std::fs::read(self.repo_root.join(&path)).ok(),
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
        let out = self
            .run_git(&["show", "-s", "--format=%H%n%an%n%aI%n%B", rev])
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
                let info = self.fetch_commit_info(rev);
                let out = self.run_git(&["diff-tree", "--no-commit-id", "--name-status", "-r", "-M", "--root", rev])?;
                let text = String::from_utf8_lossy(&out);
                let parent_rev = format!("{}^", rev);
                let has_parent = self.run_git(&["rev-parse", "--verify", &parent_rev]).is_ok();
                let mut changes = Vec::new();
                for line in text.lines() {
                    if let Some(fc) = self.parse_name_status_line(
                        line,
                        if has_parent { Some(parent_rev.as_str()) } else { None },
                        Some(rev),
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
                let out = self.run_git(&["diff", "--name-status", "-M", from, to])?;
                let text = String::from_utf8_lossy(&out);
                let mut changes = Vec::new();
                for line in text.lines() {
                    if let Some(fc) = self.parse_name_status_line(line, Some(from), Some(to), ignore)? {
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
            let spec = format!("{}^{{commit}}", rev.trim());
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

    fn commit_files(&self, paths: &[String], message: &str) -> Result<String> {
        if paths.is_empty() {
            bail!("No files selected to commit");
        }
        if message.trim().is_empty() {
            bail!("Commit message cannot be empty");
        }

        // Include paired old paths for any renamed files so `git add` & `git commit` handle deletions too.
        let mut expanded_paths = paths.to_vec();
        let selected_set: HashSet<&str> = paths.iter().map(|s| s.as_str()).collect();
        if let Ok(bundle) = self.collect_uncommitted(None) {
            for ch in bundle.changes {
                if selected_set.contains(ch.path.as_str()) {
                    if let Some(old_p) = ch.old_path {
                        if !expanded_paths.contains(&old_p) {
                            expanded_paths.push(old_p);
                        }
                    }
                }
            }
        }

        let add_paths: Vec<String> = expanded_paths
            .iter()
            .filter(|p| {
                self.repo_root.join(p).exists()
                    || self
                        .run_git(&["ls-files", "--error-unmatch", "--", p])
                        .is_ok()
            })
            .cloned()
            .collect();

        if !add_paths.is_empty() {
            let mut add_cmd = Command::new("git");
            add_cmd
                .args(["-c", "core.quotepath=false", "add", "-A", "--"])
                .current_dir(&self.repo_root);
            for p in &add_paths {
                add_cmd.arg(p);
            }
            let add_out = add_cmd.output().context("Failed to execute `git add`")?;
            if !add_out.status.success() {
                let err = String::from_utf8_lossy(&add_out.stderr);
                bail!("`git add` failed: {}", err.trim());
            }
        }

        let mut msg_file = NamedTempFile::new().context("Failed to create temp commit message file")?;
        msg_file.write_all(message.as_bytes())?;
        let msg_path = msg_file.path().to_string_lossy().to_string();

        let mut commit_cmd = Command::new("git");
        commit_cmd
            .args(["-c", "core.quotepath=false", "commit", "-F", &msg_path, "--"])
            .current_dir(&self.repo_root);
        for p in &expanded_paths {
            commit_cmd.arg(p);
        }
        let out = commit_cmd.output().context("Failed to execute `git commit`")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            bail!(
                "`git commit` failed: {}{}",
                err.trim(),
                if err.trim().is_empty() { stdout.trim() } else { "" }
            );
        }
        let summary = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("Git commit completed.")
            .trim()
            .to_string();
        Ok(summary)
    }
}
