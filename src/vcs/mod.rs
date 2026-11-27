pub mod git;
pub mod svn;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::AppConfig;
use crate::ignore::IgnoreMatcher;
use crate::model::{CommitInfo, DiffTarget, FileChange, VcsKind};

#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub committed_paths: Vec<String>,
    pub failed_paths: Vec<String>,
    pub summary: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RevertOutcome {
    pub reverted_paths: Vec<String>,
    pub failed_paths: Vec<String>,
    pub summary: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffBundle {
    pub vcs: VcsKind,
    pub repo_root: PathBuf,
    pub path_base_dir: PathBuf,
    pub target: DiffTarget,
    pub commit_info: Option<CommitInfo>,
    pub changes: Vec<FileChange>,
}

pub trait VcsBackend: Send + Sync {
    fn kind(&self) -> VcsKind;
    fn repo_root(&self) -> &Path;
    fn scope_dir(&self) -> &Path;
    fn cache_identity(&self) -> String {
        self.repo_root().to_string_lossy().to_string()
    }
    fn collect_changes(&self, target: &DiffTarget) -> Result<DiffBundle> {
        self.collect_changes_filtered(target, None)
    }
    fn collect_changes_filtered(
        &self,
        target: &DiffTarget,
        ignore: Option<&IgnoreMatcher>,
    ) -> Result<DiffBundle>;
    fn recent_commits(&self, limit: usize) -> Result<Vec<CommitInfo>>;
    fn latest_revision(&self) -> Result<Option<String>> {
        Ok(None)
    }
    fn commits_in_range_after(
        &self,
        _newer_rev_inclusive: &str,
        _older_rev_exclusive: &str,
        _limit: usize,
    ) -> Result<Option<Vec<CommitInfo>>> {
        Ok(None)
    }
    fn resolve_immutable_target(&self, _target: &DiffTarget) -> Option<DiffTarget> {
        None
    }
    fn external_prefixes(&self) -> Vec<String> {
        Vec::new()
    }
    fn commit_files(&self, paths: &[String], message: &str) -> Result<String>;
    fn commit_files_detailed(&self, paths: &[String], message: &str) -> CommitOutcome {
        match self.commit_files(paths, message) {
            Ok(summary) => CommitOutcome {
                committed_paths: paths.to_vec(),
                failed_paths: Vec::new(),
                summary,
                error: None,
            },
            Err(e) => CommitOutcome {
                committed_paths: Vec::new(),
                failed_paths: paths.to_vec(),
                summary: String::new(),
                error: Some(format!("{:#}", e)),
            },
        }
    }
    fn commit_files_with_external_message(
        &self,
        paths: &[String],
        message: &str,
        external_message: Option<&str>,
    ) -> CommitOutcome {
        let _ = external_message;
        self.commit_files_detailed(paths, message)
    }
    fn revert_files(&self, paths: &[String]) -> Result<String>;
    fn revert_files_detailed(&self, paths: &[String]) -> RevertOutcome {
        match self.revert_files(paths) {
            Ok(summary) => RevertOutcome {
                reverted_paths: paths.to_vec(),
                failed_paths: Vec::new(),
                summary,
                error: None,
            },
            Err(e) => RevertOutcome {
                reverted_paths: Vec::new(),
                failed_paths: paths.to_vec(),
                summary: String::new(),
                error: Some(format!("{:#}", e)),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryCacheEntry {
    head_revision: String,
    limit: usize,
    commits: Vec<CommitInfo>,
}

fn hash_key(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for p in parts {
        hasher.update(p.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

fn history_cache_file(vcs: &dyn VcsBackend) -> Option<PathBuf> {
    let base = AppConfig::cache_dir()?;
    let id = vcs.cache_identity();
    let kind = vcs.kind().to_string();
    let key = hash_key(&[&kind, &id]);
    Some(base.join("history").join(format!("{}.json", key)))
}

fn diff_cache_file(
    vcs: &dyn VcsBackend,
    canon_target: &DiffTarget,
    ignore_fp: &str,
) -> Option<PathBuf> {
    let base = AppConfig::cache_dir()?;
    let kind = vcs.kind().to_string();
    let id = vcs.cache_identity();
    let scope = vcs.scope_dir().to_string_lossy();
    let target_str = match canon_target {
        DiffTarget::Uncommitted => return None,
        DiffTarget::Commit(r) => format!("commit:{}", r),
        DiffTarget::Range { from, to } => format!("range:{}..{}", from, to),
    };
    let key = hash_key(&["v2", &kind, &id, &scope, &target_str, ignore_fp]);
    Some(base.join("history_diffs").join(format!("{}.json", key)))
}

/// Immediately read cached commit history from disk (0 network calls).
pub fn load_history_disk_cache(vcs: &dyn VcsBackend, limit: usize) -> Option<Vec<CommitInfo>> {
    let path = history_cache_file(vcs)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let entry: HistoryCacheEntry = serde_json::from_str(&raw).ok()?;
    Some(entry.commits.into_iter().take(limit).collect())
}

/// Fetch recent commits using disk cache when repository head revision has not changed.
/// If head moved forward, attempts an incremental range fetch before falling back to full log.
pub fn recent_commits_cached(
    vcs: &dyn VcsBackend,
    limit: usize,
    use_cache: bool,
    force_refresh: bool,
) -> Result<Vec<CommitInfo>> {
    if !use_cache {
        return vcs.recent_commits(limit);
    }

    let cache_path = history_cache_file(vcs);
    let existing = if !force_refresh {
        cache_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<HistoryCacheEntry>(&raw).ok())
    } else {
        None
    };

    let head_rev = vcs.latest_revision().ok().flatten();

    if let (Some(entry), Some(head)) = (&existing, &head_rev) {
        if entry.limit >= limit && entry.head_revision == *head {
            return Ok(entry.commits.iter().take(limit).cloned().collect());
        }
        if entry.limit >= limit && entry.head_revision != *head {
            if let Ok(Some(delta)) =
                vcs.commits_in_range_after(head, &entry.head_revision, limit)
            {
                let mut merged = Vec::with_capacity(limit);
                let mut seen = HashSet::new();
                for c in delta.into_iter().chain(entry.commits.iter().cloned()) {
                    if seen.insert(c.revision.clone()) {
                        merged.push(c);
                        if merged.len() >= limit {
                            break;
                        }
                    }
                }
                if let Some(p) = &cache_path {
                    save_history_entry(p, head, limit, &merged);
                }
                return Ok(merged);
            }
        }
    }

    let commits = vcs.recent_commits(limit)?;
    if let (Some(p), Some(head)) = (&cache_path, head_rev.as_deref()) {
        save_history_entry(p, head, limit, &commits);
    } else if let (Some(p), Some(first)) = (&cache_path, commits.first()) {
        save_history_entry(p, &first.revision, limit, &commits);
    }
    Ok(commits)
}

fn save_history_entry(path: &Path, head_revision: &str, limit: usize, commits: &[CommitInfo]) {
    let entry = HistoryCacheEntry {
        head_revision: head_revision.to_string(),
        limit,
        commits: commits.to_vec(),
    };
    if let Ok(json) = serde_json::to_vec(&entry) {
        let _ = atomic_write_cache(path, &json, 128, 64 * 1024 * 1024);
    }
}

pub(crate) fn atomic_write_cache(
    path: &Path,
    bytes: &[u8],
    max_files: usize,
    max_total_bytes: u64,
) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut tmp, bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    prune_cache_dir(parent, max_files, max_total_bytes);
    Ok(())
}

fn prune_cache_dir(dir: &Path, max_files: usize, max_total_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(meta) = ent.metadata() {
            if meta.is_file() {
                let len = meta.len();
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                total = total.saturating_add(len);
                files.push((path, len, mtime));
            }
        }
    }
    if files.len() <= max_files && total <= max_total_bytes {
        return;
    }
    files.sort_by_key(|(_, _, mtime)| *mtime);
    let mut count = files.len();
    for (p, len, _) in files {
        if count <= max_files && total <= max_total_bytes {
            break;
        }
        if std::fs::remove_file(p).is_ok() {
            count = count.saturating_sub(1);
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(unix)]
mod unix_worktree {
    use anyhow::{Context, Result, bail};
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    fn path_to_cstr(p: &std::ffi::OsStr) -> Result<CString> {
        CString::new(p.as_bytes()).map_err(|_| anyhow::anyhow!("Embedded NUL byte in path"))
    }

    /// Pin `base_dir` and step through every intermediate component atomically with
    /// `O_RDONLY | O_DIRECTORY | O_NOFOLLOW` so parent directories cannot be swapped
    /// for symlinks between validation and use.
    fn open_parent_dir_nofollow(base_dir: &Path, rel_path: &Path) -> Result<Option<(OwnedFd, CString)>> {
        let mut normal_comps: Vec<std::ffi::OsString> = Vec::new();
        for comp in rel_path.components() {
            match comp {
                std::path::Component::Normal(seg) => normal_comps.push(seg.to_os_string()),
                std::path::Component::CurDir => {}
                _ => bail!("Unsafe path outside worktree root: {}", rel_path.display()),
            }
        }
        if normal_comps.is_empty() {
            return Ok(None);
        }

        let base_cstr = path_to_cstr(base_dir.as_os_str())?;
        let base_raw = unsafe {
            libc::open(
                base_cstr.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if base_raw < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(err).with_context(|| {
                format!("Failed to open worktree root {}", base_dir.display())
            });
        }
        let mut cur_fd = unsafe { OwnedFd::from_raw_fd(base_raw) };

        let last_idx = normal_comps.len() - 1;
        for seg in &normal_comps[..last_idx] {
            let seg_cstr = path_to_cstr(seg.as_os_str())?;
            let next_raw = unsafe {
                libc::openat(
                    cur_fd.as_raw_fd(),
                    seg_cstr.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if next_raw < 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::ENOENT) => return Ok(None),
                    Some(libc::ELOOP) | Some(libc::ENOTDIR) => bail!(
                        "Refusing to traverse symlink or non-directory ancestor in worktree path {}",
                        rel_path.display()
                    ),
                    _ => {
                        return Err(err).with_context(|| {
                            format!("Failed to open worktree parent segment in {}", rel_path.display())
                        });
                    }
                }
            }
            cur_fd = unsafe { OwnedFd::from_raw_fd(next_raw) };
        }

        let leaf_cstr = path_to_cstr(normal_comps[last_idx].as_os_str())?;
        Ok(Some((cur_fd, leaf_cstr)))
    }

    pub(crate) fn read_file_safe(
        base_dir: &Path,
        rel_path: &Path,
        svn_link_prefix: bool,
    ) -> Option<Vec<u8>> {
        let (parent_fd, leaf_cstr) = open_parent_dir_nofollow(base_dir, rel_path).ok().flatten()?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                parent_fd.as_raw_fd(),
                leaf_cstr.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return None;
        }
        let ftype = st.st_mode & libc::S_IFMT;
        if ftype == libc::S_IFLNK {
            let mut cap = 1024usize;
            loop {
                let mut buf = vec![0u8; cap];
                let n = unsafe {
                    libc::readlinkat(
                        parent_fd.as_raw_fd(),
                        leaf_cstr.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    )
                };
                if n < 0 {
                    return None;
                }
                let n = n as usize;
                if n < cap || cap >= 65_536 {
                    buf.truncate(n);
                    if svn_link_prefix {
                        let mut out = Vec::with_capacity(5 + buf.len());
                        out.extend_from_slice(b"link ");
                        out.extend_from_slice(&buf);
                        return Some(out);
                    }
                    return Some(buf);
                }
                cap *= 2;
            }
        }
        if ftype != libc::S_IFREG {
            return None;
        }
        let file_raw = unsafe {
            libc::openat(
                parent_fd.as_raw_fd(),
                leaf_cstr.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if file_raw < 0 {
            return None;
        }
        let file_fd = unsafe { OwnedFd::from_raw_fd(file_raw) };
        let mut file = std::fs::File::from(file_fd);
        let fmeta = file.metadata().ok()?;
        if !fmeta.is_file()
            || fmeta.dev() != (st.st_dev as u64)
            || fmeta.ino() != (st.st_ino as u64)
        {
            return None;
        }
        let mut buf = Vec::with_capacity(fmeta.len() as usize);
        file.read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    pub(crate) fn remove_file_safe(base_dir: &Path, rel_path: &Path) -> Result<()> {
        let Some((parent_fd, leaf_cstr)) = open_parent_dir_nofollow(base_dir, rel_path)? else {
            return Ok(());
        };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                parent_fd.as_raw_fd(),
                leaf_cstr.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(err).with_context(|| {
                format!("Failed to inspect {} before revert removal", rel_path.display())
            });
        }
        let ftype = st.st_mode & libc::S_IFMT;
        if ftype == libc::S_IFREG || ftype == libc::S_IFLNK {
            if unsafe { libc::unlinkat(parent_fd.as_raw_fd(), leaf_cstr.as_ptr(), 0) } != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENOENT) {
                    return Ok(());
                }
                return Err(err).with_context(|| {
                    format!("Failed to unlink added/untracked file {}", rel_path.display())
                });
            }
        }
        Ok(())
    }
}

#[cfg(not(unix))]
mod fallback_worktree {
    use anyhow::{Context, Result, bail};
    use std::path::{Path, PathBuf};

    fn resolve_worktree_path_safe(
        base_dir: &Path,
        rel_path: &Path,
    ) -> Result<Option<(PathBuf, std::fs::Metadata)>> {
        let mut normal_comps: Vec<std::ffi::OsString> = Vec::new();
        for comp in rel_path.components() {
            match comp {
                std::path::Component::Normal(seg) => normal_comps.push(seg.to_os_string()),
                std::path::Component::CurDir => {}
                _ => bail!("Unsafe path outside worktree root: {}", rel_path.display()),
            }
        }
        if normal_comps.is_empty() {
            return Ok(None);
        }
        let mut cur = base_dir.to_path_buf();
        let last_idx = normal_comps.len() - 1;
        for (idx, seg) in normal_comps.into_iter().enumerate() {
            cur.push(&seg);
            let meta = match std::fs::symlink_metadata(&cur) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to inspect worktree path {}", cur.display())
                    });
                }
            };
            if idx < last_idx && (meta.file_type().is_symlink() || !meta.is_dir()) {
                bail!(
                    "Refusing to traverse symlink or non-directory ancestor in worktree path {}",
                    rel_path.display()
                );
            }
        }
        let meta = std::fs::symlink_metadata(&cur)?;
        Ok(Some((cur, meta)))
    }

    pub(crate) fn read_file_safe(
        base_dir: &Path,
        rel_path: &Path,
        svn_link_prefix: bool,
    ) -> Option<Vec<u8>> {
        let (abs, meta) = resolve_worktree_path_safe(base_dir, rel_path).ok().flatten()?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&abs).ok()?;
            let bytes = target.to_string_lossy().into_owned().into_bytes();
            if svn_link_prefix {
                let mut out = Vec::with_capacity(5 + bytes.len());
                out.extend_from_slice(b"link ");
                out.extend_from_slice(&bytes);
                return Some(out);
            }
            return Some(bytes);
        }
        meta.is_file().then(|| std::fs::read(&abs).ok()).flatten()
    }

    pub(crate) fn remove_file_safe(base_dir: &Path, rel_path: &Path) -> Result<()> {
        let Some((abs, meta)) = resolve_worktree_path_safe(base_dir, rel_path)? else {
            return Ok(());
        };
        if meta.is_file() || meta.file_type().is_symlink() {
            std::fs::remove_file(&abs).with_context(|| {
                format!("Failed to remove added/untracked file {}", abs.display())
            })?;
        }
        Ok(())
    }
}

/// Read a worktree entry without following symlinks on either ancestor directories or the leaf.
/// Symlinks return raw link target bytes (matching Git `120000` blobs).
pub(crate) fn read_worktree_file_safe(base_dir: &Path, rel_path: &Path) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        unix_worktree::read_file_safe(base_dir, rel_path, false)
    }
    #[cfg(not(unix))]
    {
        fallback_worktree::read_file_safe(base_dir, rel_path, false)
    }
}

/// SVN variant representing symlinks in SVN's `svn:special` format (`link <target>`).
pub(crate) fn read_worktree_svn_special_safe(base_dir: &Path, rel_path: &Path) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        unix_worktree::read_file_safe(base_dir, rel_path, true)
    }
    #[cfg(not(unix))]
    {
        fallback_worktree::read_file_safe(base_dir, rel_path, true)
    }
}

/// Remove a leaf file or symlink strictly underneath `base_dir` without traversing symlinked ancestors.
pub(crate) fn remove_worktree_file_safe(base_dir: &Path, rel_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        unix_worktree::remove_file_safe(base_dir, rel_path)
    }
    #[cfg(not(unix))]
    {
        fallback_worktree::remove_file_safe(base_dir, rel_path)
    }
}

/// Collect changes for `target`, caching immutable commit/range bundles on disk.
/// `DiffTarget::Uncommitted` is never cached. Binary byte payloads are stripped before caching
/// while preserving `old_size`/`new_size`.
pub fn collect_changes_cached(
    vcs: &dyn VcsBackend,
    target: &DiffTarget,
    ignore: Option<&IgnoreMatcher>,
    use_cache: bool,
    force_refresh: bool,
) -> Result<DiffBundle> {
    let immutable = if use_cache {
        vcs.resolve_immutable_target(target)
    } else {
        None
    };
    let ignore_fp = ignore.map(|m| m.fingerprint()).unwrap_or("");
    let cache_path = immutable
        .as_ref()
        .and_then(|canon| diff_cache_file(vcs, canon, ignore_fp));

    if !force_refresh {
        if let Some(p) = &cache_path {
            if let Ok(raw) = std::fs::read_to_string(p) {
                if let Ok(mut cached) = serde_json::from_str::<DiffBundle>(&raw) {
                    cached.target = target.clone();
                    return Ok(cached);
                }
            }
        }
    }

    // Fetch using the exact resolved immutable target so cache key and fetched bytes always match.
    let fetch_target = immutable.as_ref().unwrap_or(target);
    let mut bundle = vcs.collect_changes_filtered(fetch_target, ignore)?;
    bundle.target = target.clone();

    if let Some(p) = &cache_path {
        const MAX_CACHED_BUNDLE_BYTES: usize = 4 * 1024 * 1024;
        let est_text_bytes: usize = bundle
            .changes
            .iter()
            .filter(|ch| !ch.is_binary)
            .map(|ch| {
                ch.old_content.as_ref().map_or(0, Vec::len)
                    + ch.new_content.as_ref().map_or(0, Vec::len)
            })
            .sum();
        if est_text_bytes <= MAX_CACHED_BUNDLE_BYTES {
            let mut cache_bundle = bundle.clone();
            for ch in &mut cache_bundle.changes {
                if ch.is_binary {
                    if ch.old_size.is_none() {
                        ch.old_size = ch.old_content.as_ref().map(|b| b.len());
                    }
                    if ch.new_size.is_none() {
                        ch.new_size = ch.new_content.as_ref().map(|b| b.len());
                    }
                    ch.old_content = None;
                    ch.new_content = None;
                }
            }
            if let Ok(json) = serde_json::to_vec(&cache_bundle) {
                if json.len() <= MAX_CACHED_BUNDLE_BYTES {
                    let _ = atomic_write_cache(p, &json, 128, 128 * 1024 * 1024);
                }
            }
        }
    }
    Ok(bundle)
}

pub fn open_vcs(path: &Path, preferred: Option<VcsKind>) -> Result<Arc<dyn VcsBackend>> {
    match preferred {
        Some(VcsKind::Svn) => Ok(Arc::new(svn::SvnBackend::open(path)?)),
        Some(VcsKind::Git) => Ok(Arc::new(git::GitBackend::open(path)?)),
        None => {
            if let Ok(svn) = svn::SvnBackend::open(path) {
                return Ok(Arc::new(svn));
            }
            if let Ok(git) = git::GitBackend::open(path) {
                return Ok(Arc::new(git));
            }
            bail!(
                "No SVN working copy or Git repository found at {}",
                path.display()
            )
        }
    }
}

pub fn looks_binary(bytes: &[u8]) -> bool {
    let check_len = bytes.len().min(8192);
    bytes[..check_len].contains(&0)
}
