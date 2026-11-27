use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

use crate::ignore::IgnoreMatcher;
use crate::model::{ChangeKind, CommitInfo, DiffTarget, FileChange, VcsKind};
use crate::vcs::{DiffBundle, VcsBackend, looks_binary};

pub struct SvnBackend {
    wc_root: PathBuf,
    scope_dir: PathBuf,
    wc_url: String,
    repo_root_url: String,
    repo_uuid: String,
}

impl SvnBackend {
    fn parse_info_repo_identity(xml: &str) -> (String, String, String, Option<PathBuf>) {
        let Ok(doc) = roxmltree::Document::parse(xml) else {
            return (String::new(), String::new(), String::new(), None);
        };
        let Some(entry) = doc.descendants().find(|n| n.has_tag_name("entry")) else {
            return (String::new(), String::new(), String::new(), None);
        };
        let wc_root = entry
            .descendants()
            .find(|n| n.has_tag_name("wcroot-abspath"))
            .and_then(|n| n.text())
            .map(PathBuf::from)
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
        let wc_url = entry
            .descendants()
            .find(|n| n.has_tag_name("url"))
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        let repo_root_url = entry
            .descendants()
            .find(|n| n.has_tag_name("root"))
            .and_then(|n| n.text())
            .unwrap_or(&wc_url)
            .trim_end_matches('/')
            .to_string();
        let repo_uuid = entry
            .descendants()
            .find(|n| n.has_tag_name("uuid"))
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim()
            .to_string();
        (wc_url, repo_root_url, repo_uuid, wc_root)
    }

    pub fn open(dir: &Path) -> Result<Self> {
        let scope_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let output = Command::new("svn")
            .args(["info", "--non-interactive", "--xml"])
            .current_dir(&scope_dir)
            .output()
            .context("Failed to execute `svn info --non-interactive --xml`")?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("Not an SVN working copy: {}", err.trim());
        }

        let xml = String::from_utf8(output.stdout).context("Invalid UTF-8 from `svn info --xml`")?;
        let (wc_url, repo_root_url, repo_uuid, wc_root_opt) = Self::parse_info_repo_identity(&xml);
        if wc_url.is_empty() && repo_root_url.is_empty() {
            bail!("Missing <entry> in `svn info --xml`");
        }
        let wc_root = wc_root_opt.unwrap_or_else(|| scope_dir.clone());

        Ok(Self {
            wc_root,
            scope_dir,
            wc_url,
            repo_root_url,
            repo_uuid,
        })
    }

    fn parse_log_xml(xml: &str) -> Result<Vec<CommitInfo>> {
        let doc = roxmltree::Document::parse(xml).context("Failed to parse `svn log --xml`")?;
        let mut commits = Vec::new();
        for logentry in doc.descendants().filter(|n| n.has_tag_name("logentry")) {
            let revision = logentry.attribute("revision").unwrap_or("").to_string();
            let author = logentry
                .children()
                .find(|n| n.has_tag_name("author"))
                .and_then(|n| n.text())
                .unwrap_or("unknown")
                .to_string();
            let date = logentry
                .children()
                .find(|n| n.has_tag_name("date"))
                .and_then(|n| n.text())
                .unwrap_or("")
                .to_string();
            let message = logentry
                .children()
                .find(|n| n.has_tag_name("msg"))
                .and_then(|n| n.text())
                .unwrap_or("")
                .trim()
                .to_string();
            let changed_paths_count = logentry
                .children()
                .find(|n| n.has_tag_name("paths"))
                .map(|p| p.children().filter(|c| c.has_tag_name("path")).count());

            commits.push(CommitInfo {
                revision,
                author,
                date,
                message,
                changed_paths_count,
            });
        }
        Ok(commits)
    }

    fn run_svn_in(&self, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = Command::new("svn");
        if let Some((subcmd, rest)) = args.split_first() {
            cmd.arg(subcmd);
            if !args.contains(&"--non-interactive") {
                cmd.arg("--non-interactive");
            }
            cmd.args(rest);
        }
        let output = cmd
            .current_dir(dir)
            .output()
            .with_context(|| format!("Failed to run `svn {}`", args.join(" ")))?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("`svn {}` failed: {}", args.join(" "), err.trim());
        }
        Ok(output.stdout)
    }

    fn run_svn(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.run_svn_in(&self.scope_dir, args)
    }

    fn svn_peg_target(target: &str) -> String {
        if target.contains('@') {
            format!("{}@", target)
        } else {
            target.to_string()
        }
    }

    fn svn_cat_at_peg(&self, target: &str, peg_rev: &str) -> Result<Vec<u8>> {
        // Append peg revision directly (SVN splits at the last `@`, so `@` in filenames stays literal).
        let peg_target = format!("{}@{}", target, peg_rev);
        let output = Command::new("svn")
            .args(["cat", "--non-interactive", "--", &peg_target])
            .current_dir(&self.scope_dir)
            .output()
            .with_context(|| format!("Failed to execute `svn cat` for {}", peg_target))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("`svn cat` failed for {}: {}", peg_target, err.trim());
        }
        Ok(output.stdout)
    }

    fn svn_cat_rev_optional(&self, rev: &str, path_or_url: &str) -> Option<Vec<u8>> {
        let peg = Self::svn_peg_target(path_or_url);
        let output = Command::new("svn")
            .args(["cat", "--non-interactive", "-r", rev, "--", &peg])
            .current_dir(&self.scope_dir)
            .output()
            .ok()?;
        if output.status.success() {
            Some(output.stdout)
        } else {
            None
        }
    }

    fn svn_prop_diff(&self, rev_args: &[&str], target: &str) -> Option<String> {
        let mut args = vec!["diff", "--non-interactive", "--depth", "empty"];
        args.extend_from_slice(rev_args);
        args.push("--");
        args.push(target);
        let output = Command::new("svn")
            .args(&args)
            .current_dir(&self.scope_dir)
            .output()
            .ok()?;
        if output.status.success() && !output.stdout.is_empty() {
            Some(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            None
        }
    }

    fn normalize_rev(rev: &str) -> String {
        rev.trim()
            .strip_prefix('r')
            .or_else(|| rev.trim().strip_prefix('R'))
            .unwrap_or(rev.trim())
            .to_string()
    }

    fn validate_rev(rev: &str) -> Result<String> {
        let cleaned = Self::normalize_rev(rev);
        if cleaned.is_empty() {
            bail!("SVN revision cannot be empty");
        }
        if cleaned.starts_with('-')
            || cleaned.contains('@')
            || cleaned.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            bail!("Invalid SVN revision `{}`", rev);
        }
        let valid = cleaned.chars().all(|c| c.is_ascii_digit())
            || matches!(cleaned.as_str(), "HEAD" | "BASE" | "COMMITTED" | "PREV")
            || (cleaned.starts_with('{') && cleaned.ends_with('}') && cleaned.len() > 2);
        if !valid {
            bail!("Invalid SVN revision `{}`", rev);
        }
        Ok(cleaned)
    }

    fn percent_decode_utf8(input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let (Some(h), Some(l)) = (
                    (bytes[i + 1] as char).to_digit(16),
                    (bytes[i + 2] as char).to_digit(16),
                ) {
                    out.push(((h << 4) | l) as u8);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn url_to_display_path(&self, raw: &str) -> String {
        let raw_trimmed = raw.trim();
        if let Some(rel) = raw_trimmed.strip_prefix(&self.wc_url) {
            return Self::percent_decode_utf8(rel.trim_start_matches('/'));
        }
        if let Some(rel) = raw_trimmed.strip_prefix(&self.repo_root_url) {
            return Self::percent_decode_utf8(rel.trim_start_matches('/'));
        }
        if let Ok(p) = Path::new(raw_trimmed).strip_prefix(&self.scope_dir) {
            return p.to_string_lossy().to_string();
        }
        if let Ok(p) = Path::new(raw_trimmed).strip_prefix(&self.wc_root) {
            return p.to_string_lossy().to_string();
        }
        raw_trimmed.to_string()
    }

    fn collect_uncommitted(&self, ignore: Option<&IgnoreMatcher>) -> Result<DiffBundle> {
        // Discover svn:externals directories under scope_dir (~30ms) so we scan both root & externals in parallel.
        let ext_dirs = self.discover_externals(ignore);

        let mut scan_targets: Vec<(String, PathBuf)> = vec![(String::new(), self.scope_dir.clone())];
        for ext_rel in ext_dirs {
            let ext_abs = self.scope_dir.join(&ext_rel);
            if ext_abs.is_dir() {
                scan_targets.push((ext_rel, ext_abs));
            }
        }

        let summarize_results: Vec<Option<(String, Vec<u8>)>> = std::thread::scope(|s| {
            let handles: Vec<_> = scan_targets
                .iter()
                .map(|(prefix, dir)| {
                    s.spawn(move || {
                        self.run_svn_in(dir, &["diff", "--summarize", "--xml", "."])
                            .ok()
                            .map(|bytes| (prefix.clone(), bytes))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().ok().flatten()).collect()
        });

        // If the primary root summarize call succeeded, parse all summarize XMLs (root + externals)
        if summarize_results.first().and_then(|r| r.as_ref()).is_some() {
            let mut raw_entries = Vec::new();
            let mut seen = HashSet::new();

            for item in summarize_results.into_iter().flatten() {
                let (prefix, bytes) = item;
                if let Ok(mut part) =
                    self.parse_uncommitted_summarize_entries(&prefix, &bytes, ignore, &mut seen)
                {
                    raw_entries.append(&mut part);
                }
            }

            let changes = self.fetch_uncommitted_contents(raw_entries);
            return Ok(DiffBundle {
                vcs: VcsKind::Svn,
                repo_root: self.wc_root.clone(),
                path_base_dir: self.scope_dir.clone(),
                target: DiffTarget::Uncommitted,
                commit_info: None,
                changes,
            });
        }

        // Fallback to `svn status -q --xml .`
        let xml_bytes = self.run_svn_in(&self.scope_dir, &["status", "-q", "--xml", "."])?;
        let xml = String::from_utf8(xml_bytes).context("Invalid UTF-8 from `svn status --xml`")?;
        let doc = roxmltree::Document::parse(&xml).context("Failed to parse `svn status --xml`")?;

        let mut raw_entries = Vec::new();
        let mut seen = HashSet::new();

        for entry in doc.descendants().filter(|n| n.has_tag_name("entry")) {
            let Some(raw_path) = entry.attribute("path") else {
                continue;
            };
            let rel_path = self.url_to_display_path(raw_path);
            if rel_path.is_empty()
                || rel_path == "."
                || rel_path.starts_with(".git/")
                || rel_path.contains("/.git/")
                || !seen.insert(rel_path.clone())
            {
                continue;
            }
            if ignore.is_some_and(|m| m.is_ignored(&self.scope_dir, &rel_path)) {
                continue;
            }

            let Some(wc_status) = entry.children().find(|n| n.has_tag_name("wc-status")) else {
                continue;
            };
            let item = wc_status.attribute("item").unwrap_or("none");
            let props = wc_status.attribute("props").unwrap_or("none");
            let copied = wc_status.attribute("copied") == Some("true");
            let moved_from = wc_status.attribute("moved-from").map(|s| self.url_to_display_path(s));

            let kind = match item {
                "added" => {
                    if let Some(from) = moved_from.clone() {
                        ChangeKind::Renamed { from }
                    } else {
                        ChangeKind::Added
                    }
                }
                "modified" | "conflicted" => ChangeKind::Modified,
                "deleted" | "missing" => ChangeKind::Deleted,
                "replaced" => ChangeKind::Replaced,
                "normal" if props != "none" => ChangeKind::PropertyOnly,
                _ => continue,
            };

            let abs_path = self.scope_dir.join(&rel_path);
            let is_dir = kind != ChangeKind::Deleted && abs_path.is_dir();
            if is_dir && props == "none" {
                continue;
            }

            raw_entries.push((rel_path, moved_from, kind, is_dir, props.to_string(), copied));
        }

        let changes = self.fetch_uncommitted_contents(raw_entries);
        Ok(DiffBundle {
            vcs: VcsKind::Svn,
            repo_root: self.wc_root.clone(),
            path_base_dir: self.scope_dir.clone(),
            target: DiffTarget::Uncommitted,
            commit_info: None,
            changes,
        })
    }

    fn discover_externals(&self, ignore: Option<&IgnoreMatcher>) -> Vec<String> {
        let Ok(xml_bytes) = self.run_svn_in(
            &self.scope_dir,
            &["propget", "svn:externals", "-R", "--xml", "."],
        ) else {
            return Vec::new();
        };
        let Ok(xml) = std::str::from_utf8(&xml_bytes) else {
            return Vec::new();
        };
        let Ok(doc) = roxmltree::Document::parse(xml) else {
            return Vec::new();
        };

        let mut externals = Vec::new();
        let mut seen = HashSet::new();

        for target in doc.descendants().filter(|n| n.has_tag_name("target")) {
            let Some(raw_target_path) = target.attribute("path") else {
                continue;
            };
            let target_rel = self.url_to_display_path(raw_target_path);
            let target_prefix = if target_rel == "." || target_rel.is_empty() {
                String::new()
            } else {
                target_rel.trim_end_matches('/').to_string()
            };

            for prop in target.children().filter(|n| n.has_tag_name("property")) {
                let Some(text) = prop.text() else {
                    continue;
                };
                for line in text.lines() {
                    if let Some(local) = parse_external_local_dir(line) {
                        let full_rel = if target_prefix.is_empty() {
                            local
                        } else {
                            format!("{}/{}", target_prefix, local)
                        };
                        if ignore.is_some_and(|m| m.is_ignored(&self.scope_dir, &full_rel)) {
                            continue;
                        }
                        if seen.insert(full_rel.clone()) {
                            externals.push(full_rel);
                        }
                    }
                }
            }
        }
        externals
    }

    fn parse_uncommitted_summarize_entries(
        &self,
        prefix: &str,
        xml_bytes: &[u8],
        ignore: Option<&IgnoreMatcher>,
        seen: &mut HashSet<String>,
    ) -> Result<Vec<(String, Option<String>, ChangeKind, bool, String, bool)>> {
        let xml = std::str::from_utf8(xml_bytes)
            .context("Invalid UTF-8 from `svn diff --summarize --xml`")?;
        let doc = roxmltree::Document::parse(xml)
            .context("Failed to parse `svn diff --summarize --xml`")?;

        let mut raw_entries = Vec::new();
        let norm_prefix = prefix.trim_matches('/');

        for path_node in doc.descendants().filter(|n| n.has_tag_name("path")) {
            let item = path_node.attribute("item").unwrap_or("modified");
            let props = path_node.attribute("props").unwrap_or("none");
            let node_kind = path_node.attribute("kind").unwrap_or("file");
            let Some(raw_path) = path_node.text() else {
                continue;
            };
            let local_rel = self.url_to_display_path(raw_path);
            let rel_path = if norm_prefix.is_empty() {
                local_rel
            } else if local_rel.is_empty() || local_rel == "." {
                norm_prefix.to_string()
            } else {
                format!("{}/{}", norm_prefix, local_rel)
            };

            if rel_path.is_empty()
                || rel_path == "."
                || rel_path.starts_with(".git/")
                || rel_path.contains("/.git/")
                || !seen.insert(rel_path.clone())
            {
                continue;
            }
            if ignore.is_some_and(|m| m.is_ignored(&self.scope_dir, &rel_path)) {
                continue;
            }

            let is_dir = node_kind == "dir";
            if is_dir && props == "none" {
                continue;
            }

            let kind = match item {
                "added" => ChangeKind::Added,
                "deleted" => ChangeKind::Deleted,
                "replaced" => ChangeKind::Replaced,
                "none" if props != "none" => ChangeKind::PropertyOnly,
                _ => ChangeKind::Modified,
            };

            raw_entries.push((rel_path, None, kind, is_dir, props.to_string(), false));
        }

        Ok(raw_entries)
    }

    fn fetch_uncommitted_contents(
        &self,
        raw_entries: Vec<(String, Option<String>, ChangeKind, bool, String, bool)>,
    ) -> Vec<FileChange> {
        let results = run_bounded(&raw_entries, 8, |(rel_path, moved_from, kind, is_dir, props, copied)| {
            let rel_path = rel_path.clone();
            let moved_from = moved_from.clone();
            let kind = kind.clone();
            let is_dir = *is_dir;
            let props = props.clone();
            let copied = *copied;

            let old_content = match &kind {
                ChangeKind::Modified | ChangeKind::Deleted | ChangeKind::Replaced => {
                    self.svn_cat_rev_optional("BASE", &rel_path)
                }
                ChangeKind::Added | ChangeKind::Renamed { .. } if copied => {
                    self.svn_cat_rev_optional("BASE", &rel_path)
                }
                _ => None,
            };

            let new_content = match &kind {
                ChangeKind::Added
                | ChangeKind::Modified
                | ChangeKind::Replaced
                | ChangeKind::Renamed { .. }
                    if !is_dir =>
                {
                    crate::vcs::read_worktree_svn_special_safe(
                        &self.scope_dir,
                        Path::new(&rel_path),
                    )
                }
                _ => None,
            };

            let prop_changes = if props != "none" {
                let peg = Self::svn_peg_target(&rel_path);
                self.svn_prop_diff(&[], &peg)
            } else {
                None
            };

            if is_dir && prop_changes.is_none() {
                return None;
            }

            let is_binary = old_content.as_deref().is_some_and(looks_binary)
                || new_content.as_deref().is_some_and(looks_binary);
            let old_size = old_content.as_ref().map(|b| b.len());
            let new_size = new_content.as_ref().map(|b| b.len());

            Some(FileChange {
                path: rel_path,
                old_path: moved_from,
                kind: if is_dir { ChangeKind::PropertyOnly } else { kind },
                is_binary,
                old_content,
                new_content,
                old_size,
                new_size,
                prop_changes,
            })
        });

        let mut changes: Vec<FileChange> = results.into_iter().flatten().collect();
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    fn fetch_commit_info(&self, rev: &str) -> Option<CommitInfo> {
        let xml_bytes = self.run_svn(&["log", "--xml", "-r", rev]).ok()?;
        let xml = String::from_utf8(xml_bytes).ok()?;
        Self::parse_log_xml(&xml).ok()?.into_iter().next()
    }

    fn collect_revision_range(
        &self,
        old_rev: &str,
        new_rev: &str,
        target: DiffTarget,
        commit_info: Option<CommitInfo>,
        ignore: Option<&IgnoreMatcher>,
    ) -> Result<DiffBundle> {
        let rev_range = format!("{}:{}", old_rev, new_rev);
        let url_peg = format!("{}@{}", self.wc_url, new_rev);
        let xml_bytes = self.run_svn(&["diff", "--summarize", "--xml", "-r", &rev_range, "--", &url_peg])?;
        let xml = String::from_utf8(xml_bytes).context("Invalid UTF-8 from `svn diff --summarize --xml`")?;
        let doc = roxmltree::Document::parse(&xml).context("Failed to parse `svn diff --summarize --xml`")?;

        let mut raw_entries = Vec::new();
        let mut seen = HashSet::new();

        for path_node in doc.descendants().filter(|n| n.has_tag_name("path")) {
            let item = path_node.attribute("item").unwrap_or("modified");
            let props = path_node.attribute("props").unwrap_or("none");
            let node_kind = path_node.attribute("kind").unwrap_or("file");
            let Some(raw_url) = path_node.text() else {
                continue;
            };
            let raw_url = raw_url.trim().to_string();
            let display_path = self.url_to_display_path(&raw_url);
            if display_path.is_empty()
                || display_path.starts_with(".git/")
                || display_path.contains("/.git/")
                || !seen.insert(display_path.clone())
            {
                continue;
            }
            if ignore.is_some_and(|m| m.is_ignored(&self.scope_dir, &display_path)) {
                continue;
            }

            let is_dir = node_kind == "dir";
            if is_dir && props == "none" {
                continue;
            }

            let kind = match item {
                "added" => ChangeKind::Added,
                "deleted" => ChangeKind::Deleted,
                "replaced" => ChangeKind::Replaced,
                "none" if props != "none" => ChangeKind::PropertyOnly,
                _ => ChangeKind::Modified,
            };

            raw_entries.push((raw_url, display_path, is_dir, props.to_string(), kind));
        }

        let rev_range_ref = rev_range.as_str();
        let results: Vec<Result<Option<FileChange>>> =
            run_bounded(&raw_entries, 8, |(raw_url, display_path, is_dir, props, kind)| {
                let old_content = if !*is_dir && old_rev != "0" && *kind != ChangeKind::Added {
                    match kind {
                        // A replaced path may have changed kind (e.g. dir -> file) in `old_rev`.
                        ChangeKind::Replaced => self.svn_cat_at_peg(raw_url, old_rev).ok(),
                        _ => Some(self.svn_cat_at_peg(raw_url, old_rev)?),
                    }
                } else {
                    None
                };

                let new_content = if !*is_dir && *kind != ChangeKind::Deleted {
                    match kind {
                        ChangeKind::Replaced => self.svn_cat_at_peg(raw_url, new_rev).ok(),
                        _ => Some(self.svn_cat_at_peg(raw_url, new_rev)?),
                    }
                } else {
                    None
                };

                let prop_changes = if props != "none" {
                    let prop_target = format!("{}@{}", raw_url, new_rev);
                    self.svn_prop_diff(&["-r", rev_range_ref], &prop_target)
                } else {
                    None
                };

                if *is_dir && prop_changes.is_none() {
                    return Ok(None);
                }

                let is_binary = old_content.as_deref().is_some_and(looks_binary)
                    || new_content.as_deref().is_some_and(looks_binary);
                let old_size = old_content.as_ref().map(|b| b.len());
                let new_size = new_content.as_ref().map(|b| b.len());

                Ok(Some(FileChange {
                    path: display_path.clone(),
                    old_path: None,
                    kind: if *is_dir {
                        ChangeKind::PropertyOnly
                    } else {
                        kind.clone()
                    },
                    is_binary,
                    old_content,
                    new_content,
                    old_size,
                    new_size,
                    prop_changes,
                }))
            });

        let mut changes = Vec::new();
        for res in results {
            if let Some(ch) = res? {
                changes.push(ch);
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(DiffBundle {
            vcs: VcsKind::Svn,
            repo_root: self.wc_root.clone(),
            path_base_dir: self.scope_dir.clone(),
            target,
            commit_info,
            changes,
        })
    }
}

fn run_bounded<T: Sync, R: Send>(
    items: &[T],
    max_workers: usize,
    worker: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    crate::parallel::run_bounded(items, max_workers, worker)
}

impl SvnBackend {
    fn repo_identity_in(&self, dir: &Path) -> Option<(String, String)> {
        if !dir.exists() {
            return None;
        }
        let xml_bytes = self.run_svn_in(dir, &["info", "--non-interactive", "--xml"]).ok()?;
        let xml = std::str::from_utf8(&xml_bytes).ok()?;
        let (_, root_url, uuid, _) = Self::parse_info_repo_identity(xml);
        if root_url.is_empty() && uuid.is_empty() {
            None
        } else {
            Some((root_url, uuid))
        }
    }

    fn is_same_repo_as_main(&self, dir: &Path) -> bool {
        let Some((ext_root, ext_uuid)) = self.repo_identity_in(dir) else {
            return false;
        };
        (!self.repo_uuid.is_empty() && ext_uuid == self.repo_uuid)
            || (!self.repo_root_url.is_empty() && ext_root == self.repo_root_url)
    }

    fn discover_split_externals(&self, ignore: Option<&IgnoreMatcher>) -> Vec<String> {
        self.discover_externals(ignore)
            .into_iter()
            .filter(|ext| !self.is_same_repo_as_main(&self.scope_dir.join(ext)))
            .collect()
    }

    fn group_paths_by_external_list(
        &self,
        paths: &[String],
        mut externals: Vec<String>,
    ) -> BTreeMap<String, Vec<(String, String)>> {
        externals.sort_by_key(|b| std::cmp::Reverse(b.len()));

        let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for p in paths {
            let norm = p.trim_start_matches("./").to_string();
            if let Some(ext) = externals
                .iter()
                .find(|e| norm == **e || norm.starts_with(&format!("{}/", e)))
            {
                let rel_in_ext = norm
                    .strip_prefix(&format!("{}/", ext))
                    .unwrap_or(".")
                    .to_string();
                groups.entry(ext.clone()).or_default().push((rel_in_ext, p.clone()));
            } else {
                groups.entry(String::new()).or_default().push((norm, p.clone()));
            }
        }
        groups
    }

    fn group_paths_by_external(&self, paths: &[String]) -> BTreeMap<String, Vec<(String, String)>> {
        self.group_paths_by_external_list(paths, self.discover_externals(None))
    }

    /// Group commit targets, merging externals whose SVN repository matches main WC's
    /// repository (`uuid` or root URL) into `""` (single atomic commit).
    fn group_paths_for_commit(&self, paths: &[String]) -> BTreeMap<String, Vec<(String, String)>> {
        self.group_paths_by_external_list(paths, self.discover_split_externals(None))
    }
}

impl VcsBackend for SvnBackend {
    fn kind(&self) -> VcsKind {
        VcsKind::Svn
    }

    fn repo_root(&self) -> &Path {
        &self.wc_root
    }

    fn scope_dir(&self) -> &Path {
        &self.scope_dir
    }

    fn cache_identity(&self) -> String {
        self.wc_url.clone()
    }

    fn collect_changes_filtered(
        &self,
        target: &DiffTarget,
        ignore: Option<&IgnoreMatcher>,
    ) -> Result<DiffBundle> {
        match target {
            DiffTarget::Uncommitted => self.collect_uncommitted(ignore),
            DiffTarget::Commit(rev) => {
                let rev_clean = Self::validate_rev(rev)?;
                let rev_num: u64 = rev_clean
                    .parse()
                    .with_context(|| format!("Invalid SVN revision number: {}", rev))?;
                if rev_num == 0 {
                    bail!("SVN revision 0 has no changes");
                }
                let prev_rev = (rev_num - 1).to_string();
                let info = self.fetch_commit_info(&rev_clean);
                self.collect_revision_range(&prev_rev, &rev_clean, target.clone(), info, ignore)
            }
            DiffTarget::Range { from, to } => {
                let from_clean = Self::validate_rev(from)?;
                let to_clean = Self::validate_rev(to)?;
                self.collect_revision_range(&from_clean, &to_clean, target.clone(), None, ignore)
            }
        }
    }

    fn recent_commits(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        let limit_str = limit.to_string();
        let xml_bytes = self.run_svn(&["log", "--xml", "-v", "-l", &limit_str, &self.wc_url])?;
        let xml = String::from_utf8(xml_bytes).context("Invalid UTF-8 from `svn log --xml`")?;
        Self::parse_log_xml(&xml)
    }

    fn latest_revision(&self) -> Result<Option<String>> {
        let xml_bytes = self.run_svn(&["log", "--xml", "-l", "1", "-q", &self.wc_url])?;
        let xml = String::from_utf8(xml_bytes).context("Invalid UTF-8 from `svn log --xml`")?;
        let commits = Self::parse_log_xml(&xml)?;
        Ok(commits.into_iter().next().map(|c| c.revision))
    }

    fn commits_in_range_after(
        &self,
        newer_rev_inclusive: &str,
        older_rev_exclusive: &str,
        limit: usize,
    ) -> Result<Option<Vec<CommitInfo>>> {
        let Ok(newer) = newer_rev_inclusive.trim().parse::<u64>() else {
            return Ok(None);
        };
        let Ok(older) = older_rev_exclusive.trim().parse::<u64>() else {
            return Ok(None);
        };
        if newer < older {
            return Ok(None);
        }
        if newer == older {
            return Ok(Some(Vec::new()));
        }
        if newer - older > limit as u64 {
            return Ok(None);
        }
        let rev_arg = format!("{}:{}", newer, older + 1);
        let limit_str = limit.to_string();
        let xml_bytes =
            self.run_svn(&["log", "--xml", "-v", "-r", &rev_arg, "-l", &limit_str, &self.wc_url])?;
        let xml = String::from_utf8(xml_bytes).context("Invalid UTF-8 from `svn log --xml`")?;
        Ok(Some(Self::parse_log_xml(&xml)?))
    }

    fn resolve_immutable_target(&self, target: &DiffTarget) -> Option<DiffTarget> {
        match target {
            DiffTarget::Uncommitted => None,
            DiffTarget::Commit(rev) => {
                let clean = Self::normalize_rev(rev);
                clean.parse::<u64>().ok().map(|_| DiffTarget::Commit(clean))
            }
            DiffTarget::Range { from, to } => {
                let f = Self::normalize_rev(from);
                let t = Self::normalize_rev(to);
                if f.parse::<u64>().is_ok() && t.parse::<u64>().is_ok() {
                    Some(DiffTarget::Range { from: f, to: t })
                } else {
                    None
                }
            }
        }
    }

    fn external_prefixes(&self) -> Vec<String> {
        self.discover_split_externals(None)
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

        let groups = self.group_paths_for_commit(paths);
        let needs_main = groups.contains_key("");
        let has_ext = groups.keys().any(|k| !k.is_empty());
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

        let mut outputs = Vec::new();
        let mut errors = Vec::new();
        let mut committed_paths = Vec::new();
        let mut failed_paths = Vec::new();

        let locale = crate::i18n::current_thread_locale();
        for (prefix, entries) in groups {
            let work_dir = if prefix.is_empty() {
                self.scope_dir.clone()
            } else {
                self.scope_dir.join(&prefix)
            };
            let label = if prefix.is_empty() {
                rust_i18n::t!("vcs.main_repo", locale = locale.as_str()).into_owned()
            } else {
                format!("external[{}]", prefix)
            };
            let msg_path = if prefix.is_empty() {
                &main_msg_path
            } else {
                &ext_msg_path
            };
            let mut cmd = Command::new("svn");
            cmd.args([
                "commit",
                "--non-interactive",
                "--depth",
                "empty",
                "-F",
                msg_path,
                "--",
            ])
            .current_dir(&work_dir);
            for (gp, _) in &entries {
                cmd.arg(Self::svn_peg_target(gp));
            }
            match cmd.output() {
                Ok(out) if out.status.success() => {
                    for (_, orig) in entries {
                        committed_paths.push(orig);
                    }
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    outputs.push(if stdout.is_empty() {
                        rust_i18n::t!("vcs.svn_committed", locale = locale.as_str(), label = label)
                            .into_owned()
                    } else {
                        format!("[{}] {}", label, stdout)
                    });
                }
                Ok(out) => {
                    for (_, orig) in entries {
                        failed_paths.push(orig);
                    }
                    let err = String::from_utf8_lossy(&out.stderr);
                    errors.push(
                        rust_i18n::t!(
                            "vcs.svn_commit_failed",
                            locale = locale.as_str(),
                            label = label,
                            err = err.trim()
                        )
                        .into_owned(),
                    );
                }
                Err(e) => {
                    for (_, orig) in entries {
                        failed_paths.push(orig);
                    }
                    errors.push(
                        rust_i18n::t!(
                            "vcs.svn_exec_error",
                            locale = locale.as_str(),
                            label = label,
                            err = e
                        )
                        .into_owned(),
                    );
                }
            }
        }

        let summary = if outputs.is_empty() {
            String::new()
        } else {
            outputs.join(" | ")
        };
        let error = if errors.is_empty() {
            None
        } else {
            Some(errors.join(" ; "))
        };
        crate::vcs::CommitOutcome {
            committed_paths,
            failed_paths,
            summary,
            error,
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

        // Expand any paired old_path for renamed entries via lightweight `svn status -q --xml`
        // so `svn mv` restores cleanly without reading full file contents.
        let mut expanded_paths = paths.to_vec();
        let selected_set: HashSet<&str> = paths.iter().map(|s| s.as_str()).collect();
        if let Ok(xml_bytes) = self.run_svn_in(&self.scope_dir, &["status", "-q", "--xml", "."]) {
            if let Ok(xml) = std::str::from_utf8(&xml_bytes) {
                if let Ok(doc) = roxmltree::Document::parse(xml) {
                    for entry in doc.descendants().filter(|n| n.has_tag_name("entry")) {
                        let Some(raw_path) = entry.attribute("path") else {
                            continue;
                        };
                        let rel_path = self.url_to_display_path(raw_path);
                        if !selected_set.contains(rel_path.as_str()) {
                            continue;
                        }
                        if let Some(moved_from) = entry
                            .children()
                            .find(|n| n.has_tag_name("wc-status"))
                            .and_then(|wc| wc.attribute("moved-from"))
                        {
                            let old_p = self.url_to_display_path(moved_from);
                            if !old_p.is_empty() && !expanded_paths.contains(&old_p) {
                                expanded_paths.push(old_p);
                            }
                        }
                    }
                }
            }
        }

        let groups = self.group_paths_by_external(&expanded_paths);
        let mut outputs = Vec::new();
        let mut errors = Vec::new();
        let mut reverted_paths = Vec::new();
        let mut failed_paths = Vec::new();

        let locale = crate::i18n::current_thread_locale();
        for (prefix, entries) in groups {
            let work_dir = if prefix.is_empty() {
                self.scope_dir.clone()
            } else {
                self.scope_dir.join(&prefix)
            };
            let label = if prefix.is_empty() {
                rust_i18n::t!("vcs.main_repo", locale = locale.as_str()).into_owned()
            } else {
                format!("external[{}]", prefix)
            };
            let mut cmd = Command::new("svn");
            cmd.args(["revert", "--non-interactive", "--depth", "empty", "--"])
                .current_dir(&work_dir);
            for (gp, _) in &entries {
                cmd.arg(Self::svn_peg_target(gp));
            }
            match cmd.output() {
                Ok(out) if out.status.success() => {
                    for (_, orig) in entries {
                        if selected_set.contains(orig.as_str()) {
                            reverted_paths.push(orig);
                        }
                    }
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    outputs.push(if stdout.is_empty() {
                        rust_i18n::t!("vcs.svn_reverted", locale = locale.as_str(), label = label)
                            .into_owned()
                    } else {
                        format!("[{}] {}", label, stdout)
                    });
                }
                Ok(out) => {
                    for (_, orig) in entries {
                        if selected_set.contains(orig.as_str()) {
                            failed_paths.push(orig);
                        }
                    }
                    let err = String::from_utf8_lossy(&out.stderr);
                    errors.push(
                        rust_i18n::t!(
                            "vcs.svn_revert_failed",
                            locale = locale.as_str(),
                            label = label,
                            err = err.trim()
                        )
                        .into_owned(),
                    );
                }
                Err(e) => {
                    for (_, orig) in entries {
                        if selected_set.contains(orig.as_str()) {
                            failed_paths.push(orig);
                        }
                    }
                    errors.push(
                        rust_i18n::t!(
                            "vcs.svn_exec_error",
                            locale = locale.as_str(),
                            label = label,
                            err = e
                        )
                        .into_owned(),
                    );
                }
            }
        }

        let summary = if outputs.is_empty() {
            String::new()
        } else {
            outputs.join(" | ")
        };
        let error = if errors.is_empty() {
            None
        } else {
            Some(errors.join(" ; "))
        };
        crate::vcs::RevertOutcome {
            reverted_paths,
            failed_paths,
            summary,
            error,
        }
    }
}

fn split_shell_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
            }
            Some(_) => cur.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
            }
            None if ch.is_whitespace() => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Extract local subdirectory path from a single line of `svn:externals` property value.
fn parse_external_local_dir(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let raw_tokens = split_shell_tokens(trimmed);
    let mut tokens: Vec<String> = Vec::new();
    let mut skip_next = false;
    for t in raw_tokens {
        if skip_next {
            skip_next = false;
            continue;
        }
        if t == "-r" {
            skip_next = true;
            continue;
        }
        if (t.starts_with("-r") && t.len() > 2 && t[2..].chars().all(|c| c.is_ascii_digit()))
            || t == "--"
        {
            continue;
        }
        tokens.push(t);
    }

    if tokens.len() < 2 {
        return None;
    }
    let a = &tokens[0];
    let b = &tokens[1];
    let looks_like_url = |s: &str| {
        s.contains("://")
            || s.starts_with("^/")
            || s.starts_with("//")
            || s.starts_with("../")
            || s.starts_with('/')
    };
    let local = if looks_like_url(a) && !looks_like_url(b) {
        b
    } else {
        a
    };
    let clean = local.trim_matches('/');
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}
