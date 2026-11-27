use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
            .args(["--literal-pathspecs", "-c", "core.quotepath=false"])
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

    fn strict_utf8_path(bytes: &[u8], context_cmd: &str) -> Result<String> {
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| {
                anyhow::anyhow!(
                    "Non-UTF-8 path emitted by `{}` cannot be addressed safely: {:?}",
                    context_cmd,
                    String::from_utf8_lossy(bytes)
                )
            })
    }

    fn git_cat_file_batch_in(
        &self,
        dir: &Path,
        shas: &[String],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let mut blobs = HashMap::new();
        if shas.is_empty() {
            return Ok(blobs);
        }
        let mut unique = Vec::with_capacity(shas.len());
        let mut seen = HashSet::with_capacity(shas.len());
        for s in shas {
            if seen.insert(s.as_str()) {
                unique.push(s.as_str());
            }
        }
        let mut child = Command::new("git")
            .args([
                "--literal-pathspecs",
                "-c",
                "core.quotepath=false",
                "cat-file",
                "--batch",
            ])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn `git cat-file --batch`")?;

        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let err_handle = std::thread::spawn(move || {
            let mut err_buf = Vec::new();
            if let Some(mut s) = stderr {
                let _ = s.read_to_end(&mut err_buf);
            }
            err_buf
        });

        let mut parse_err: Option<anyhow::Error> = None;
        std::thread::scope(|s| {
            let writer_handle = s.spawn(move || -> std::io::Result<()> {
                if let Some(mut sin) = stdin.take() {
                    for sha in unique {
                        writeln!(sin, "{}", sha)?;
                    }
                    sin.flush()?;
                }
                Ok(())
            });

            if let Some(sout) = stdout {
                let mut reader = BufReader::new(sout);
                let mut header_buf = Vec::with_capacity(128);
                loop {
                    header_buf.clear();
                    match reader.read_until(b'\n', &mut header_buf) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) => {
                            parse_err = Some(anyhow::anyhow!("Failed reading `git cat-file --batch`: {}", e));
                            break;
                        }
                    }
                    let header = String::from_utf8_lossy(&header_buf);
                    let trimmed = header.trim_end_matches(['\r', '\n']);
                    if trimmed.ends_with(" missing") {
                        continue;
                    }
                    let mut parts = trimmed.split_whitespace();
                    let Some(sha) = parts.next() else {
                        parse_err = Some(anyhow::anyhow!("Malformed `git cat-file --batch` header: {}", trimmed));
                        break;
                    };
                    let Some(_obj_type) = parts.next() else {
                        parse_err = Some(anyhow::anyhow!("Malformed `git cat-file --batch` header: {}", trimmed));
                        break;
                    };
                    let Some(size_str) = parts.next() else {
                        parse_err = Some(anyhow::anyhow!("Malformed `git cat-file --batch` header: {}", trimmed));
                        break;
                    };
                    let Ok(size) = size_str.parse::<usize>() else {
                        parse_err = Some(anyhow::anyhow!("Invalid `git cat-file --batch` size: {}", size_str));
                        break;
                    };
                    let mut data = vec![0u8; size];
                    if let Err(e) = reader.read_exact(&mut data) {
                        parse_err = Some(anyhow::anyhow!("Truncated `git cat-file --batch` blob {}: {}", sha, e));
                        break;
                    }
                    let mut nl = [0u8; 1];
                    match reader.read_exact(&mut nl) {
                        Ok(()) if nl == [b'\n'] => {}
                        Ok(()) => {
                            parse_err = Some(anyhow::anyhow!(
                                "Expected trailing newline after `git cat-file --batch` blob {}",
                                sha
                            ));
                            break;
                        }
                        Err(e) => {
                            parse_err = Some(anyhow::anyhow!(
                                "Truncated delimiter after `git cat-file --batch` blob {}: {}",
                                sha,
                                e
                            ));
                            break;
                        }
                    }
                    blobs.insert(sha.to_string(), data);
                }
            }
            let _ = writer_handle.join();
        });

        let status = child.wait().context("Failed waiting for `git cat-file --batch`")?;
        let err_bytes = err_handle.join().unwrap_or_default();
        if let Some(e) = parse_err {
            return Err(e);
        }
        if !status.success() {
            bail!(
                "`git cat-file --batch` failed: {}",
                String::from_utf8_lossy(&err_bytes).trim()
            );
        }
        Ok(blobs)
    }

    fn head_exists_in(&self, dir: &Path) -> Result<bool> {
        let out = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .current_dir(dir)
            .output()
            .with_context(|| format!("Failed to inspect HEAD in {}", dir.display()))?;
        if out.status.success() {
            return Ok(true);
        }
        // On an unborn branch (`git init` with 0 commits), `rev-parse --verify --quiet HEAD`
        // exits 1 silently while `symbolic-ref --quiet HEAD` succeeds.
        if out.stderr.is_empty() {
            let sym = Command::new("git")
                .args(["symbolic-ref", "--quiet", "HEAD"])
                .current_dir(dir)
                .output()
                .with_context(|| format!("Failed to inspect symbolic HEAD in {}", dir.display()))?;
            if sym.status.success() {
                return Ok(false);
            }
        }
        bail!(
            "Failed to resolve HEAD in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
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

    fn collect_index_gitlink_state_in(
        &self,
        dir: &Path,
        prefix: &str,
    ) -> Result<(Vec<String>, HashSet<String>)> {
        let mut seen_gl = HashSet::new();
        let mut links = Vec::new();
        let mut non_gitlinks = HashSet::new();
        let stage_out = self
            .run_git_in(dir, &["ls-files", "--stage", "-z"])
            .with_context(|| format!("Failed to inspect staged gitlinks in {}", dir.display()))?;
        for rec in stage_out.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            let Some(tab_pos) = rec.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let meta = std::str::from_utf8(&rec[..tab_pos]).unwrap_or("");
            let path = Self::strict_utf8_path(&rec[tab_pos + 1..], "git ls-files --stage -z")?;
            let full = Self::prefix_path(prefix, &path);
            if meta.starts_with("160000 ") {
                if seen_gl.insert(full.clone()) {
                    links.push(full);
                }
            } else {
                non_gitlinks.insert(full);
            }
        }
        if self.head_exists_in(dir)? {
            let head_out = self
                .run_git_in(dir, &["ls-tree", "-r", "-z", "HEAD"])
                .with_context(|| format!("Failed to inspect HEAD gitlinks in {}", dir.display()))?;
            for rec in head_out.split(|&b| b == 0).filter(|s| !s.is_empty()) {
                let Some(tab_pos) = rec.iter().position(|&b| b == b'\t') else {
                    continue;
                };
                let meta = std::str::from_utf8(&rec[..tab_pos]).unwrap_or("");
                if meta.starts_with("160000 ") {
                    let path =
                        Self::strict_utf8_path(&rec[tab_pos + 1..], "git ls-tree -r -z HEAD")?;
                    let full = Self::prefix_path(prefix, &path);
                    if seen_gl.insert(full.clone()) {
                        links.push(full);
                    }
                }
            }
        }
        Ok((links, non_gitlinks))
    }

    fn verify_no_unresolved_gitlinks(
        &self,
        paths: &[String],
        subs: &[String],
    ) -> Result<()> {
        let (mut gitlinks, mut non_gitlinks) =
            self.collect_index_gitlink_state_in(&self.repo_root, "")?;
        for sub in subs {
            let sub_dir = self.repo_root.join(sub);
            if sub_dir.join(".git").exists() {
                let (sub_gl, sub_non_gl) = self.collect_index_gitlink_state_in(&sub_dir, sub)?;
                gitlinks.extend(sub_gl);
                non_gitlinks.extend(sub_non_gl);
            }
        }
        let sub_set: HashSet<&str> = subs.iter().map(String::as_str).collect();
        for p in paths {
            let norm = p.trim_start_matches("./");
            if non_gitlinks.contains(norm) {
                // Explicitly tracked as a non-gitlink entry in the owning repo index (e.g.,
                // submodule converted into regular directory and staged in the parent index).
                continue;
            }
            let mut anc_opt = std::path::Path::new(norm).parent();
            while let Some(anc) = anc_opt {
                if !anc.as_os_str().is_empty() {
                    let anc_str = anc.to_string_lossy();
                    let anc_norm = anc_str.replace('\\', "/");
                    if !sub_set.contains(anc_norm.as_str())
                        && self.repo_root.join(anc).join(".git").exists()
                    {
                        bail!(
                            "Path `{}` lies inside embedded git repository `{}` whose submodule metadata cannot be resolved safely",
                            p,
                            anc_norm
                        );
                    }
                }
                anc_opt = anc.parent();
            }
            for gl in &gitlinks {
                let prefix = format!("{}/", gl);
                if norm.starts_with(&prefix) {
                    // Must be owned by an initialized submodule strictly beneath (or equal to) `gl`,
                    // otherwise removing `.gitmodules` or staging deletion would misroute `gl/...`.
                    let owned_by_init_sub = sub_set.contains(gl.as_str())
                        || subs
                            .iter()
                            .any(|s| s.starts_with(&prefix) && norm.starts_with(&format!("{}/", s)));
                    if !owned_by_init_sub {
                        bail!(
                            "Path `{}` belongs to index gitlink `{}` whose submodule metadata cannot be resolved safely",
                            p,
                            gl
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Discover initialized submodule prefixes (including nested `--recursive` submodules)
    /// relative to `self.repo_root`, constrained to `self.scope_dir` and `ignore`,
    /// propagating command errors instead of silently falling back to the parent repo.
    fn initialized_submodules_checked(&self, ignore: Option<&IgnoreMatcher>) -> Result<Vec<String>> {
        if !self.repo_root.join(".gitmodules").is_file() {
            return Ok(Vec::new());
        }
        let out = self
            .run_git(&["submodule", "status", "--recursive"])
            .context("Failed to enumerate initialized git submodules")?;
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
        Ok(subs)
    }

    fn initialized_submodules(&self, ignore: Option<&IgnoreMatcher>) -> Vec<String> {
        self.initialized_submodules_checked(ignore)
            .unwrap_or_default()
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
        let has_head = self.head_exists_in(repo_dir)?;

        if has_head {
            let out = self.run_git_in(
                scan_dir,
                &["diff", "--raw", "-z", "--no-abbrev", "-M", "HEAD", "--", "."],
            )?;
            for fc in self.build_changes_from_raw_z(
                repo_dir,
                prefix,
                &out,
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

        // Also check untracked or staged files on unborn HEAD via NUL-delimited `git status --porcelain -z -uall`
        let status_out =
            self.run_git_in(scan_dir, &["status", "--porcelain", "-z", "-uall", "--", "."])?;
        let mut iter = status_out.split(|&b| b == 0).filter(|s| !s.is_empty());
        while let Some(entry_bytes) = iter.next() {
            if entry_bytes.len() < 4 {
                continue;
            }
            let entry = Self::strict_utf8_path(entry_bytes, "git status -z")?;
            let mut chars = entry.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            let _sp = chars.next();
            let inner_path: String = chars.collect();
            // In `-z` format, R/C entries emit a second NUL-terminated token for orig_path; consume it.
            if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
                if let Some(orig_bytes) = iter.next() {
                    let _ = Self::strict_utf8_path(orig_bytes, "git status -z")?;
                }
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
                let meta = std::fs::symlink_metadata(&abs).ok();
                if meta.as_ref().is_some_and(|m| m.is_dir()) {
                    continue;
                }
                let new_content =
                    crate::vcs::read_worktree_file_safe(repo_dir, Path::new(&inner_path));
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

    fn build_changes_from_raw_z(
        &self,
        repo_dir: &Path,
        prefix: &str,
        raw_out: &[u8],
        old_rev: Option<&str>,
        new_rev: Option<&str>,
        skip_dirty_subs: &HashSet<String>,
        ignore: Option<&IgnoreMatcher>,
    ) -> Result<Vec<FileChange>> {
        struct PendingEntry {
            path: String,
            old_path: Option<String>,
            inner_path: String,
            kind: ChangeKind,
            old_mode: String,
            new_mode: String,
            old_sha: String,
            new_sha: String,
            eff_new_sha: Option<String>,
        }

        let mut pending = Vec::new();
        let mut needed_shas = Vec::new();
        let mut iter = raw_out.split(|&b| b == 0);

        while let Some(meta_bytes) = iter.next() {
            if meta_bytes.is_empty() {
                continue;
            }
            let meta = String::from_utf8_lossy(meta_bytes);
            let meta_trimmed = meta.trim_start();
            let Some(after_colon) = meta_trimmed.strip_prefix(':') else {
                continue;
            };
            let meta_fields: Vec<&str> = after_colon.split_whitespace().collect();
            if meta_fields.len() < 5 {
                continue;
            }
            let (old_mode, new_mode, old_sha, new_sha, status) = (
                meta_fields[0],
                meta_fields[1],
                meta_fields[2],
                meta_fields[3],
                meta_fields[4],
            );

            let Some(p1_bytes) = iter.next() else {
                break;
            };
            let p1 = Self::strict_utf8_path(p1_bytes, "git diff --raw -z")?;
            let (kind, inner_old_path, inner_path) = if status.starts_with('R') {
                let Some(p2_bytes) = iter.next() else {
                    break;
                };
                let p2 = Self::strict_utf8_path(p2_bytes, "git diff --raw -z")?;
                let from = Self::prefix_path(prefix, &p1);
                (ChangeKind::Renamed { from }, Some(p1), p2)
            } else if status.starts_with('C') {
                let Some(p2_bytes) = iter.next() else {
                    break;
                };
                let p2 = Self::strict_utf8_path(p2_bytes, "git diff --raw -z")?;
                (ChangeKind::Added, None, p2)
            } else if status.starts_with('A') {
                (ChangeKind::Added, None, p1)
            } else if status.starts_with('D') {
                (ChangeKind::Deleted, None, p1)
            } else {
                (ChangeKind::Modified, None, p1)
            };

            let path = Self::prefix_path(prefix, &inner_path);
            let old_path = inner_old_path.as_deref().map(|p| Self::prefix_path(prefix, p));

            if ignore.is_some_and(|m| m.is_ignored(&self.repo_root, &path)) {
                continue;
            }

            let is_gitlink = old_mode == "160000" || new_mode == "160000";
            let eff_new_sha = if is_gitlink && new_mode == "160000" {
                if new_rev.is_some() || !Self::is_zero_sha(new_sha) {
                    Some(new_sha.to_string())
                } else {
                    self.resolve_head_in(&repo_dir.join(&inner_path))
                }
            } else {
                None
            };

            // Skip redundant parent gitlink entry when inner submodule files are scanned directly.
            if new_rev.is_none()
                && old_mode == "160000"
                && new_mode == "160000"
                && skip_dirty_subs.contains(&inner_path)
                && !Self::is_zero_sha(old_sha)
                && eff_new_sha.as_deref() == Some(old_sha)
            {
                continue;
            }

            if !matches!(kind, ChangeKind::Added)
                && old_mode != "160000"
                && old_rev.is_some()
                && !Self::is_zero_sha(old_sha)
            {
                needed_shas.push(old_sha.to_string());
            }
            if !matches!(kind, ChangeKind::Deleted)
                && new_mode != "160000"
                && new_rev.is_some()
                && !Self::is_zero_sha(new_sha)
            {
                needed_shas.push(new_sha.to_string());
            }

            pending.push(PendingEntry {
                path,
                old_path,
                inner_path,
                kind,
                old_mode: old_mode.to_string(),
                new_mode: new_mode.to_string(),
                old_sha: old_sha.to_string(),
                new_sha: new_sha.to_string(),
                eff_new_sha,
            });
        }

        let blobs = self.git_cat_file_batch_in(repo_dir, &needed_shas)?;
        let mut changes = Vec::with_capacity(pending.len());
        for item in pending {
            let old_content = match (&item.kind, old_rev) {
                (ChangeKind::Added, _) => None,
                _ if item.old_mode == "160000" && !Self::is_zero_sha(&item.old_sha) => {
                    Some(Self::gitlink_placeholder(&item.old_sha))
                }
                (_, Some(_)) if !Self::is_zero_sha(&item.old_sha) => Some(
                    blobs
                        .get(&item.old_sha)
                        .cloned()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Missing git blob {} for old side of {}",
                                item.old_sha,
                                item.path
                            )
                        })?,
                ),
                _ => None,
            };

            let new_content = match (&item.kind, new_rev) {
                (ChangeKind::Deleted, _) => None,
                _ if item.new_mode == "160000" => item
                    .eff_new_sha
                    .as_deref()
                    .filter(|s| !Self::is_zero_sha(s))
                    .map(Self::gitlink_placeholder),
                (_, Some(_)) if !Self::is_zero_sha(&item.new_sha) => Some(
                    blobs
                        .get(&item.new_sha)
                        .cloned()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Missing git blob {} for new side of {}",
                                item.new_sha,
                                item.path
                            )
                        })?,
                ),
                (_, None) => {
                    crate::vcs::read_worktree_file_safe(repo_dir, Path::new(&item.inner_path))
                }
                _ => None,
            };

            let is_binary = old_content.as_deref().is_some_and(looks_binary)
                || new_content.as_deref().is_some_and(looks_binary);
            let old_size = old_content.as_ref().map(|b| b.len());
            let new_size = new_content.as_ref().map(|b| b.len());

            changes.push(FileChange {
                path: item.path,
                old_path: item.old_path,
                kind: item.kind,
                is_binary,
                old_content,
                new_content,
                old_size,
                new_size,
                prop_changes: None,
            });
        }
        Ok(changes)
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
                    "-z",
                    "--no-abbrev",
                    "-r",
                    "-M",
                    "--first-parent",
                    "-m",
                    "--root",
                    valid_rev,
                ])?;
                let parent_rev = format!("{}^", valid_rev);
                let has_parent = self.run_git(&["rev-parse", "--verify", &parent_rev]).is_ok();
                let empty_subs = HashSet::new();
                let mut changes = self.build_changes_from_raw_z(
                    &self.repo_root,
                    "",
                    &out,
                    if has_parent { Some(parent_rev.as_str()) } else { None },
                    Some(valid_rev),
                    &empty_subs,
                    ignore,
                )?;
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
                    "-z",
                    "--no-abbrev",
                    "-M",
                    valid_from,
                    valid_to,
                ])?;
                let empty_subs = HashSet::new();
                let mut changes = self.build_changes_from_raw_z(
                    &self.repo_root,
                    "",
                    &out,
                    Some(valid_from),
                    Some(valid_to),
                    &empty_subs,
                    ignore,
                )?;
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

        let subs = match self.initialized_submodules_checked(None) {
            Ok(s) => s,
            Err(e) => return make_err(format!("{:#}", e)),
        };
        if let Err(e) = self.verify_no_unresolved_gitlinks(paths, &subs) {
            return make_err(format!("{:#}", e));
        }
        let selected_set: HashSet<&str> = paths.iter().map(|s| s.as_str()).collect();
        let rename_old_map = match self.collect_rename_old_map_with_subs(&selected_set, &subs) {
            Ok(m) => m,
            Err(e) => return make_err(format!("{:#}", e)),
        };
        let mut groups = Self::group_paths_by_submodule_with_subs(paths, &subs);
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
                let changed_in_parent = self.head_exists_in(&repo_dir).ok() == Some(false)
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

        let subs = match self.initialized_submodules_checked(None) {
            Ok(s) => s,
            Err(e) => return make_err(format!("{:#}", e)),
        };
        if let Err(e) = self.verify_no_unresolved_gitlinks(paths, &subs) {
            return make_err(format!("{:#}", e));
        }
        let selected_set: HashSet<&str> = paths.iter().map(|s| s.as_str()).collect();
        let rename_old_map = match self.collect_rename_old_map_with_subs(&selected_set, &subs) {
            Ok(m) => m,
            Err(e) => return make_err(format!("{:#}", e)),
        };
        let groups = Self::group_paths_by_submodule_with_subs(paths, &subs);
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
    fn group_paths_by_submodule_with_subs(
        paths: &[String],
        subs_in: &[String],
    ) -> BTreeMap<String, Vec<(String, String)>> {
        let mut subs = subs_in.to_vec();
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

    fn collect_rename_old_map_with_subs(
        &self,
        selected: &HashSet<&str>,
        subs: &[String],
    ) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();
        if selected.is_empty() {
            return Ok(map);
        }
        let scan_dir = |repo_dir: &Path, prefix: &str, out: &mut HashMap<String, String>| -> Result<()> {
            if !self.head_exists_in(repo_dir)? {
                return Ok(());
            }
            let raw = self.run_git_in(
                repo_dir,
                &[
                    "diff",
                    "--raw",
                    "-z",
                    "--no-abbrev",
                    "-M",
                    "--diff-filter=R",
                    "HEAD",
                    "--",
                    ".",
                ],
            )?;
            let mut iter = raw.split(|&b| b == 0);
            while let Some(meta_bytes) = iter.next() {
                if meta_bytes.is_empty() {
                    continue;
                }
                let meta = String::from_utf8_lossy(meta_bytes);
                if !meta.trim_start().starts_with(':') {
                    continue;
                }
                let Some(old_bytes) = iter.next() else {
                    break;
                };
                let Some(new_bytes) = iter.next() else {
                    break;
                };
                let inner_old = Self::strict_utf8_path(old_bytes, "git diff --raw -z")?;
                let inner_new = Self::strict_utf8_path(new_bytes, "git diff --raw -z")?;
                let full_new = Self::prefix_path(prefix, &inner_new);
                if selected.contains(full_new.as_str()) {
                    out.insert(full_new, Self::prefix_path(prefix, &inner_old));
                }
            }
            Ok(())
        };

        scan_dir(&self.repo_root, "", &mut map)?;
        for sub in subs {
            let sub_prefix = format!("{}/", sub);
            if selected.iter().any(|p| p.starts_with(&sub_prefix)) {
                let sub_dir = self.repo_root.join(sub);
                if sub_dir.join(".git").exists() {
                    scan_dir(&sub_dir, sub, &mut map)?;
                }
            }
        }
        Ok(map)
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
        let mut add_paths: Vec<String> = Vec::with_capacity(inner_paths.len());
        let mut missing_paths: Vec<&str> = Vec::new();
        for p in inner_paths {
            if std::fs::symlink_metadata(repo_dir.join(p)).is_ok() {
                add_paths.push(p.clone());
            } else {
                missing_paths.push(p.as_str());
            }
        }
        if !missing_paths.is_empty() {
            let mut ls_cmd = Command::new("git");
            ls_cmd
                .args(["--literal-pathspecs", "-c", "core.quotepath=false", "ls-files", "-z", "--"])
                .current_dir(repo_dir);
            for p in &missing_paths {
                ls_cmd.arg(p);
            }
            if let Ok(out) = ls_cmd.output() {
                if out.status.success() {
                    let mut tracked = HashSet::new();
                    for raw in out.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
                        tracked.insert(Self::strict_utf8_path(raw, "git ls-files -z")?);
                    }
                    for p in missing_paths {
                        if tracked.contains(p) {
                            add_paths.push(p.to_string());
                        }
                    }
                }
            }
        }

        if !add_paths.is_empty() {
            let mut add_cmd = Command::new("git");
            add_cmd
                .args(["--literal-pathspecs", "-c", "core.quotepath=false", "add", "-A", "--"])
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
            .args([
                "--literal-pathspecs",
                "-c",
                "core.quotepath=false",
                "commit",
                "-F",
                msg_path,
                "--",
            ])
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
        let has_head = self.head_exists_in(repo_dir)?;
        let mut head_restore_paths: Vec<String> = Vec::new();
        let mut remove_new_paths: Vec<String> = Vec::new();

        for p in inner_paths {
            let in_head = if has_head {
                let out = self
                    .run_git_in(
                        repo_dir,
                        &["ls-tree", "-r", "-z", "--name-only", "HEAD", "--", p],
                    )
                    .with_context(|| format!("Failed to check whether `{}` exists in HEAD", p))?;
                !out.is_empty()
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
                    .args([
                        "--literal-pathspecs",
                        "-c",
                        "core.quotepath=false",
                        "reset",
                        "-q",
                        "HEAD",
                        "--",
                    ])
                    .current_dir(repo_dir);
                for p in &remove_new_paths {
                    reset_cmd.arg(p);
                }
                let out = reset_cmd
                    .output()
                    .context("Failed to execute `git reset` during revert")?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    bail!("`git reset` failed during revert: {}", err.trim());
                }
            } else {
                let mut rm_cmd = Command::new("git");
                rm_cmd
                    .args([
                        "--literal-pathspecs",
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
                let out = rm_cmd
                    .output()
                    .context("Failed to execute `git rm --cached` during revert")?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    bail!("`git rm --cached` failed during revert: {}", err.trim());
                }
            }

            for p in &remove_new_paths {
                crate::vcs::remove_worktree_file_safe(repo_dir, Path::new(p))?;
            }
        }

        if !head_restore_paths.is_empty() {
            let mut restore_cmd = Command::new("git");
            restore_cmd
                .args([
                    "--literal-pathspecs",
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
