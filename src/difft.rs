use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::TempDir;

use crate::model::{
    ChangeKind, DifftDisplayMode, DifftJsonOutput, FileChange, FileDiffRendered, HunkMeta,
};

pub struct DifftEngine {
    difft_bin: String,
    ignore_comments: bool,
    _snapshot_dir: TempDir,
    old_root: PathBuf,
    new_root: PathBuf,
    next_pair_id: AtomicU64,
}

struct PairTempGuard {
    old_dir: PathBuf,
    new_dir: PathBuf,
}

impl Drop for PairTempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.old_dir);
        let _ = std::fs::remove_dir_all(&self.new_dir);
    }
}

impl DifftEngine {
    pub fn new(difft_bin: &str, ignore_comments: bool) -> Result<Self> {
        let snapshot_dir = TempDir::new().context("Failed to create temp dir for difft snapshots")?;
        let old_root = snapshot_dir.path().join("old");
        let new_root = snapshot_dir.path().join("new");
        std::fs::create_dir_all(&old_root)?;
        std::fs::create_dir_all(&new_root)?;

        Ok(Self {
            difft_bin: difft_bin.to_string(),
            ignore_comments,
            _snapshot_dir: snapshot_dir,
            old_root,
            new_root,
            next_pair_id: AtomicU64::new(1),
        })
    }

    pub fn check_available(difft_bin: &str) -> Result<String> {
        let out = Command::new(difft_bin)
            .arg("--version")
            .output()
            .with_context(|| format!("Failed to execute `{}` (is difftastic installed?)", difft_bin))?;
        if !out.status.success() {
            bail!("`{} --version` exited with error", difft_bin);
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn write_pair(&self, change: &FileChange) -> Result<(PathBuf, PathBuf, PairTempGuard)> {
        let pair_id = self.next_pair_id.fetch_add(1, Ordering::Relaxed);
        let raw_name = Path::new(&change.path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file");
        let safe_name: String = raw_name
            .chars()
            .map(|c| {
                if c == '/' || c == '\\' || c == '\0' {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let safe_name = if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
            "file".to_string()
        } else {
            safe_name
        };

        let old_dir = self.old_root.join(pair_id.to_string());
        let new_dir = self.new_root.join(pair_id.to_string());
        std::fs::create_dir_all(&old_dir)?;
        std::fs::create_dir_all(&new_dir)?;

        let old_path = old_dir.join(&safe_name);
        let new_path = new_dir.join(&safe_name);
        std::fs::write(&old_path, change.old_content.as_deref().unwrap_or(&[]))?;
        std::fs::write(&new_path, change.new_content.as_deref().unwrap_or(&[]))?;
        Ok((old_path, new_path, PairTempGuard { old_dir, new_dir }))
    }

    pub fn render_changes_bounded(
        &self,
        changes: &[FileChange],
        mode: DifftDisplayMode,
        width: u16,
    ) -> Result<Vec<FileDiffRendered>> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8)
            .min(changes.len());
        let next_idx = std::sync::atomic::AtomicUsize::new(0);
        let slots: Vec<std::sync::Mutex<Option<Result<FileDiffRendered>>>> =
            (0..changes.len()).map(|_| std::sync::Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                    if idx >= changes.len() {
                        break;
                    }
                    let res = self.render_file(&changes[idx], mode, width);
                    *slots[idx].lock().unwrap() = Some(res);
                });
            }
        });
        let mut rendered = Vec::with_capacity(changes.len());
        for slot in slots {
            rendered.push(slot.into_inner().unwrap().expect("render slot populated")?);
        }
        Ok(rendered)
    }

    pub fn render_file(
        &self,
        change: &FileChange,
        mode: DifftDisplayMode,
        width: u16,
    ) -> Result<FileDiffRendered> {
        if change.is_binary {
            let old_len = change.old_byte_len();
            let new_len = change.new_byte_len();
            let ansi_output = format!(
                "\x1b[1;33m{} (Binary file)\x1b[0m\n\nOld size: {} bytes\nNew size: {} bytes\n",
                change.path, old_len, new_len
            );
            return Ok(FileDiffRendered {
                path: change.path.clone(),
                language: "Binary".to_string(),
                status: change.kind.short_badge().to_string(),
                ansi_output,
                rendered_width: width,
                rendered_mode: mode,
                json: None,
                hunk_anchors: vec![0],
                hunks_meta: vec![HunkMeta {
                    chunk_index: 1,
                    local_anchor_line: 0,
                    start_line: None,
                    end_line: None,
                }],
                ai_summary_diff: format!(
                    "[Binary file {} ({}) old={}B new={}B]",
                    change.path,
                    change.kind.short_badge(),
                    old_len,
                    new_len
                ),
            });
        }

        if change.kind == ChangeKind::PropertyOnly {
            let props = change
                .prop_changes
                .clone()
                .unwrap_or_else(|| "(SVN property change)".to_string());
            let ansi_output = format!(
                "\x1b[1;36m{} (Property-only change)\x1b[0m\n\n{}",
                change.path, props
            );
            return Ok(FileDiffRendered {
                path: change.path.clone(),
                language: "SVN Properties".to_string(),
                status: "property".to_string(),
                ansi_output,
                rendered_width: width,
                rendered_mode: mode,
                json: None,
                hunk_anchors: vec![0],
                hunks_meta: vec![HunkMeta {
                    chunk_index: 1,
                    local_anchor_line: 0,
                    start_line: None,
                    end_line: None,
                }],
                ai_summary_diff: format!("[SVN Property changes for {}]\n{}", change.path, props),
            });
        }

        let (old_file, new_file, _pair_guard) = self.write_pair(change)?;
        let max_bytes = change
            .old_content
            .as_ref()
            .map_or(0, |b| b.len())
            .max(change.new_content.as_ref().map_or(0, |b| b.len()));
        // Skip JSON AST parse for huge generated files (>256KB) since difft uses line-oriented mode anyway
        let json = if max_bytes <= 262_144 {
            self.run_json(&old_file, &new_file, &change.path).ok().flatten()
        } else {
            None
        };
        let mut ansi_output = self.run_ansi(&old_file, &new_file, mode, width, &change.path)?;

        if let Some(prop_diff) = &change.prop_changes {
            ansi_output.push_str(&format!("\n\x1b[1;36m--- SVN Properties ---\x1b[0m\n{}", prop_diff));
        }

        let hunk_anchors = extract_hunk_anchors(&ansi_output);
        let hunks_meta = build_hunks_meta(&ansi_output, &hunk_anchors, json.as_ref());
        let language = json
            .as_ref()
            .map(|j| j.language.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "Text".to_string());
        let status = json
            .as_ref()
            .map(|j| j.status.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{:?}", change.kind));

        let ai_summary_diff = build_ai_structural_diff(change, json.as_ref());

        Ok(FileDiffRendered {
            path: change.path.clone(),
            language,
            status,
            ansi_output,
            rendered_width: width,
            rendered_mode: mode,
            json,
            hunk_anchors,
            hunks_meta,
            ai_summary_diff,
        })
    }

    pub fn re_render_ansi(
        &self,
        change: &FileChange,
        existing: &mut FileDiffRendered,
        mode: DifftDisplayMode,
        width: u16,
    ) -> Result<()> {
        if change.is_binary || change.kind == ChangeKind::PropertyOnly {
            existing.rendered_mode = mode;
            existing.rendered_width = width;
            return Ok(());
        }
        let (old_file, new_file, _pair_guard) = self.write_pair(change)?;
        let mut ansi_output = self.run_ansi(&old_file, &new_file, mode, width, &change.path)?;
        if let Some(prop_diff) = &change.prop_changes {
            ansi_output.push_str(&format!("\n\x1b[1;36m--- SVN Properties ---\x1b[0m\n{}", prop_diff));
        }
        let hunk_anchors = extract_hunk_anchors(&ansi_output);
        existing.hunks_meta = build_hunks_meta(&ansi_output, &hunk_anchors, existing.json.as_ref());
        existing.hunk_anchors = hunk_anchors;
        existing.ansi_output = ansi_output;
        existing.rendered_width = width;
        existing.rendered_mode = mode;
        Ok(())
    }

    fn run_ansi(
        &self,
        old_file: &Path,
        new_file: &Path,
        mode: DifftDisplayMode,
        width: u16,
        display_path: &str,
    ) -> Result<String> {
        let width_str = width.max(40).to_string();
        let mut cmd = Command::new(&self.difft_bin);
        cmd.args([
            "--display",
            mode.as_cli_arg(),
            "--color",
            "always",
            "--width",
            &width_str,
            "--context",
            "3",
            "--byte-limit",
            "262144",
            "--graph-limit",
            "1500000",
        ]);
        if self.ignore_comments {
            cmd.arg("--ignore-comments");
        }
        cmd.arg(old_file).arg(new_file);

        let output = cmd
            .output()
            .with_context(|| format!("Failed to invoke `{}`", self.difft_bin))?;

        // difft exits 0 on success (with or without --exit-code)
        if !output.status.success() && output.stdout.is_empty() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("difft failed for {}: {}", display_path, err.trim());
        }

        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let new_prefix = format!("{}/", self.new_root.display());
        let old_prefix = format!("{}/", self.old_root.display());
        let cleaned = raw
            .replace(&new_prefix, "")
            .replace(&old_prefix, "");

        if cleaned.trim().is_empty() {
            Ok(format!(
                "\x1b[2m{} --- No syntactic changes (whitespace/comments only)\x1b[0m\n",
                display_path
            ))
        } else {
            Ok(cleaned)
        }
    }

    fn run_json(
        &self,
        old_file: &Path,
        new_file: &Path,
        _display_path: &str,
    ) -> Result<Option<DifftJsonOutput>> {
        let mut cmd = Command::new(&self.difft_bin);
        cmd.env("DFT_UNSTABLE", "yes");
        cmd.args([
            "--display",
            "json",
            "--context",
            "3",
            "--byte-limit",
            "262144",
            "--graph-limit",
            "1500000",
        ]);
        if self.ignore_comments {
            cmd.arg("--ignore-comments");
        }
        cmd.arg(old_file).arg(new_file);

        let output = cmd.output()?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        // difft outputs either a single JSON object or JSON array depending on invocation
        if trimmed.starts_with('[') {
            let mut arr: Vec<DifftJsonOutput> = serde_json::from_str(trimmed)?;
            Ok(arr.pop())
        } else {
            let parsed: DifftJsonOutput = serde_json::from_str(trimmed)?;
            Ok(Some(parsed))
        }
    }
}

fn has_ansi_change_color(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'm' {
                let params = &line[i + 2..j];
                if params
                    .split(';')
                    .any(|p| matches!(p, "31" | "32" | "91" | "92"))
                {
                    return true;
                }
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    false
}

/// Detect ANSI hunk starting lines in difft output for `n` / `N` (`]c` / `[c`) navigation.
pub fn extract_hunk_anchors(ansi_output: &str) -> Vec<usize> {
    let mut anchors = Vec::new();
    let mut gap_since_change = usize::MAX;

    for (idx, line) in ansi_output.lines().enumerate() {
        if has_ansi_change_color(line) {
            if gap_since_change >= 2 {
                anchors.push(idx);
            }
            gap_since_change = 0;
        } else {
            gap_since_change = gap_since_change.saturating_add(1);
        }
    }

    if anchors.is_empty() && !ansi_output.is_empty() {
        anchors.push(0);
    }
    anchors
}

pub fn strip_ansi_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&nc) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_gutter_token(s: &str) -> Option<(Option<u32>, &str)> {
    let mut len = 0usize;
    let bytes = s.as_bytes();
    if bytes.starts_with(b"...") {
        len = 3;
        let rest = &s[len..];
        if rest.is_empty() || rest.starts_with(' ') {
            return Some((None, rest));
        }
        return None;
    }
    if bytes.starts_with(b"..") {
        len = 2;
        let rest = &s[len..];
        if rest.is_empty() || rest.starts_with(' ') {
            return Some((None, rest));
        }
        return None;
    }
    while len < bytes.len() && bytes[len].is_ascii_digit() {
        len += 1;
    }
    if len == 0 || len > 7 {
        return None;
    }
    let rest = &s[len..];
    if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('|') {
        let n = s[..len].parse::<u32>().ok()?;
        Some((Some(n), rest))
    } else {
        None
    }
}

pub fn parse_source_line_from_ansi(ansi_line: &str) -> Option<u32> {
    let plain = strip_ansi_escapes(ansi_line);
    let trimmed_start = plain.trim_start_matches(' ');
    let leading_ws = plain.len() - trimmed_start.len();

    // Handle simple_line_diff fallback ("+   42 | ..." / "-   41 | ...")
    if leading_ws == 0 && (plain.starts_with("+ ") || plain.starts_with("- ")) {
        let after_sign = plain[2..].trim_start_matches(' ');
        if let Some((Some(n), _)) = parse_gutter_token(after_sign) {
            return Some(n);
        }
    }

    let mut lhs_fallback: Option<u32> = None;
    let mut rhs_inline_candidate: Option<u32> = None;

    // Leading gutter (<= 8 leading spaces): supports
    // - single column: "  14 code", "     18 code"
    // - aligned old+new gutter: " 371  424 code", " ...  436 code", " 371  ... code"
    if leading_ws <= 8 {
        if let Some((tok1, rest1)) = parse_gutter_token(trimmed_start) {
            let sp = rest1.bytes().take_while(|&b| b == b' ').count();
            // Restrict tight gutter gap so indented code literals (e.g. " 444    0 => ...")
            // are not mistaken for a second line-number column.
            let max_gap = if tok1.is_none() { 4 } else { 2 };
            if (1..=max_gap).contains(&sp) {
                if let Some((Some(rhs_inline), _)) = parse_gutter_token(&rest1[sp..]) {
                    rhs_inline_candidate = Some(rhs_inline);
                }
            }
            lhs_fallback = tok1;
        }
    }

    // Side-by-side right column gutter: preceded by >= 2 spaces in the right half of the line
    let bytes = plain.as_bytes();
    let mid_start = (bytes.len() / 3).max(18);
    let mut i = mid_start;
    while i + 2 < bytes.len() {
        if bytes[i] == b' ' && bytes[i + 1] == b' ' {
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            if let Some((Some(rhs_sbs), _)) = parse_gutter_token(&plain[i..]) {
                return Some(rhs_sbs);
            }
        } else {
            i += 1;
        }
    }

    rhs_inline_candidate.or(lhs_fallback)
}

fn build_hunks_meta(
    ansi_output: &str,
    hunk_anchors: &[usize],
    json: Option<&DifftJsonOutput>,
) -> Vec<HunkMeta> {
    let ansi_lines: Vec<&str> = ansi_output.lines().collect();
    let json_ranges: Vec<(Option<u32>, Option<u32>)> = json
        .map(|j| {
            j.chunks
                .iter()
                .map(|chunk| {
                    let mut rhs_min: Option<u32> = None;
                    let mut rhs_max: Option<u32> = None;
                    let mut lhs_min: Option<u32> = None;
                    let mut lhs_max: Option<u32> = None;
                    for entry in chunk {
                        if let Some(r) = &entry.rhs {
                            let ln = (r.line_number + 1) as u32;
                            rhs_min = Some(rhs_min.map_or(ln, |m| m.min(ln)));
                            rhs_max = Some(rhs_max.map_or(ln, |m| m.max(ln)));
                        }
                        if let Some(l) = &entry.lhs {
                            let ln = (l.line_number + 1) as u32;
                            lhs_min = Some(lhs_min.map_or(ln, |m| m.min(ln)));
                            lhs_max = Some(lhs_max.map_or(ln, |m| m.max(ln)));
                        }
                    }
                    if rhs_min.is_some() {
                        (rhs_min, rhs_max)
                    } else {
                        (lhs_min, lhs_max)
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    hunk_anchors
        .iter()
        .enumerate()
        .map(|(i, &anchor)| {
            let line_num = ansi_lines
                .get(anchor)
                .and_then(|l| parse_source_line_from_ansi(l));

            let matched_chunk = if let Some(ln) = line_num {
                json_ranges
                    .iter()
                    .enumerate()
                    .find(|(_, (s, e))| match (s, e) {
                        (Some(a), Some(b)) => ln >= *a && ln <= *b,
                        _ => false,
                    })
                    .map(|(idx, (s, e))| (idx + 1, *s, *e))
            } else {
                None
            };

            let (chunk_index, start_line, end_line) = if let Some((cidx, s, e)) = matched_chunk {
                (cidx, s, e)
            } else if let Some((s, e)) = json_ranges.get(i).copied() {
                (i + 1, s, e)
            } else {
                (i + 1, line_num, line_num)
            };

            HunkMeta {
                chunk_index,
                local_anchor_line: anchor,
                start_line,
                end_line,
            }
        })
        .collect()
}

/// Build a compact, high-signal structural diff snippet for AI review using Difftastic JSON chunks
/// paired with the actual source lines.
pub fn build_ai_structural_diff(change: &FileChange, json: Option<&DifftJsonOutput>) -> String {
    if change.is_binary {
        return format!(
            "[Binary file {} ({} bytes -> {} bytes)]",
            change.path,
            change.old_byte_len(),
            change.new_byte_len()
        );
    }
    let old_text = change
        .old_content
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let new_text = change
        .new_content
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    let lang = json
        .map(|j| j.language.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Text");

    let mut out = format!(
        "=== File: {} [{}] (Language: {}) ===\n",
        change.path,
        change.kind.short_badge(),
        lang
    );

    if let Some(old_p) = &change.old_path {
        out.push_str(&format!("Moved/Copied from: {}\n", old_p));
    }

    if let Some(j) = json {
        if !j.chunks.is_empty() {
            for (chunk_idx, chunk) in j.chunks.iter().enumerate() {
                if chunk_idx >= 25 {
                    out.push_str(&format!("... ({} more structural chunks omitted)\n", j.chunks.len() - 25));
                    break;
                }
                let mut lhs_nums = BTreeSet::new();
                let mut rhs_nums = BTreeSet::new();
                let mut token_notes = Vec::new();

                for entry in chunk {
                    if let Some(l) = &entry.lhs {
                        lhs_nums.insert(l.line_number);
                        if !l.changes.is_empty() {
                            let toks: Vec<&str> = l.changes.iter().map(|c| c.content.as_str()).collect();
                            token_notes.push(format!("-L{}: `{}`", l.line_number + 1, toks.join("")));
                        }
                    }
                    if let Some(r) = &entry.rhs {
                        rhs_nums.insert(r.line_number);
                        if !r.changes.is_empty() {
                            let toks: Vec<&str> = r.changes.iter().map(|c| c.content.as_str()).collect();
                            token_notes.push(format!("+R{}: `{}`", r.line_number + 1, toks.join("")));
                        }
                    }
                }

                let lhs_min = lhs_nums.iter().next().copied();
                let lhs_max = lhs_nums.iter().next_back().copied();
                let rhs_min = rhs_nums.iter().next().copied();
                let rhs_max = rhs_nums.iter().next_back().copied();

                out.push_str(&format!(
                    "@@ Chunk {} (old {:?}, new {:?}) @@\n",
                    chunk_idx + 1,
                    lhs_min.zip(lhs_max).map(|(a, b)| format!("L{}-L{}", a + 1, b + 1)),
                    rhs_min.zip(rhs_max).map(|(a, b)| format!("L{}-L{}", a + 1, b + 1)),
                ));

                if let (Some(min_l), Some(max_l)) = (lhs_min, lhs_max) {
                    let start = min_l.saturating_sub(2);
                    let end = (max_l + 3).min(old_lines.len());
                    for ln in start..end {
                        let prefix = if lhs_nums.contains(&ln) { "-" } else { " " };
                        if let Some(text) = old_lines.get(ln) {
                            out.push_str(&format!("{} {:>4} | {}\n", prefix, ln + 1, text));
                        }
                    }
                }
                if let (Some(min_r), Some(max_r)) = (rhs_min, rhs_max) {
                    let start = min_r.saturating_sub(2);
                    let end = (max_r + 3).min(new_lines.len());
                    for ln in start..end {
                        let prefix = if rhs_nums.contains(&ln) { "+" } else { " " };
                        if let Some(text) = new_lines.get(ln) {
                            out.push_str(&format!("{} {:>4} | {}\n", prefix, ln + 1, text));
                        }
                    }
                }
                if !token_notes.is_empty() {
                    let preview: Vec<_> = token_notes.into_iter().take(12).collect();
                    out.push_str(&format!("AST token changes: {}\n", preview.join(", ")));
                }
            }
            return out;
        }
    }

    // Fallback for newly added / deleted / non-chunked small files
    match change.kind {
        ChangeKind::Added => {
            for (i, line) in new_lines.iter().take(120).enumerate() {
                out.push_str(&format!("+ {:>4} | {}\n", i + 1, line));
            }
            if new_lines.len() > 120 {
                out.push_str(&format!("... ({} more added lines)\n", new_lines.len() - 120));
            }
        }
        ChangeKind::Deleted => {
            for (i, line) in old_lines.iter().take(80).enumerate() {
                out.push_str(&format!("- {:>4} | {}\n", i + 1, line));
            }
            if old_lines.len() > 80 {
                out.push_str(&format!("... ({} more deleted lines)\n", old_lines.len() - 80));
            }
        }
        _ => {
            // Bounded line-level diff snippet when AST JSON was skipped (e.g. >256KB files)
            let mut prefix = 0usize;
            let min_len = old_lines.len().min(new_lines.len());
            while prefix < min_len && old_lines[prefix] == new_lines[prefix] {
                prefix += 1;
            }
            let mut suffix = 0usize;
            while prefix + suffix < min_len
                && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
            {
                suffix += 1;
            }
            let old_mid_end = old_lines.len().saturating_sub(suffix);
            let new_mid_end = new_lines.len().saturating_sub(suffix);
            if prefix >= old_mid_end && prefix >= new_mid_end {
                out.push_str("(No textual changes)\n");
            } else {
                let ctx_start = prefix.saturating_sub(2);
                out.push_str(&format!(
                    "@@ Line-diff fallback (old L{}-L{}, new L{}-L{}) @@\n",
                    prefix + 1,
                    old_mid_end.max(prefix + 1),
                    prefix + 1,
                    new_mid_end.max(prefix + 1),
                ));
                for idx in ctx_start..prefix {
                    if let Some(line) = new_lines.get(idx) {
                        out.push_str(&format!("  {:>4} | {}\n", idx + 1, line));
                    }
                }
                let mut emitted = 0usize;
                for idx in prefix..old_mid_end {
                    if emitted >= 60 {
                        out.push_str(&format!("... ({} more old lines omitted)\n", old_mid_end - idx));
                        break;
                    }
                    out.push_str(&format!("- {:>4} | {}\n", idx + 1, old_lines[idx]));
                    emitted += 1;
                }
                let mut emitted_new = 0usize;
                for idx in prefix..new_mid_end {
                    if emitted_new >= 60 {
                        out.push_str(&format!("... ({} more new lines omitted)\n", new_mid_end - idx));
                        break;
                    }
                    out.push_str(&format!("+ {:>4} | {}\n", idx + 1, new_lines[idx]));
                    emitted_new += 1;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_source_line_from_ansi;

    #[test]
    fn parses_side_by_side_inline_and_unified_difft_gutters() {
        // Pure-addition collapsed gutter in side-by-side output ("...  436")
        assert_eq!(
            parse_source_line_from_ansi(
                " \x1b[2m...\x1b[0m  \x1b[92m436\x1b[0m                 let prefix_cols = 0;"
            ),
            Some(436)
        );
        // Modified aligned line ("371  424") prefers new rhs line 424
        assert_eq!(
            parse_source_line_from_ansi(" 371  424             }"),
            Some(424)
        );
        // Side-by-side dual columns ("271 ... 324 ...") prefers right rhs line 324
        assert_eq!(
            parse_source_line_from_ansi(
                " 271     } else {                                   324     } else {"
            ),
            Some(324)
        );
        // Blank left column with right column line 322
        assert_eq!(
            parse_source_line_from_ansi(
                " ...                                                322             draw_diff_pane(frame);"
            ),
            Some(322)
        );
        // Inline added line (5 spaces leading gutter)
        assert_eq!(
            parse_source_line_from_ansi("     18     App, CommitModalFocus,"),
            Some(18)
        );
        // Pure deleted line falls back to lhs line 371
        assert_eq!(
            parse_source_line_from_ansi(" 371  ...                 old deleted"),
            Some(371)
        );
        // Wrapped continuation line returns None
        assert_eq!(
            parse_source_line_from_ansi(
                "  .. ar, UnicodeWidthStr};                 .. ar, UnicodeWidthStr};"
            ),
            None
        );
        // Code line starting with a numeric literal after >= 4 spaces must not override rhs/lhs gutter
        assert_eq!(
            parse_source_line_from_ansi("  13    0 => foo"),
            Some(13)
        );
        assert_eq!(
            parse_source_line_from_ansi(" 444    0 => foo                  496    0 => bar"),
            Some(496)
        );
        // Diff section banner must not be misparsed as line number
        assert_eq!(
            parse_source_line_from_ansi("14/mod.rs --- 1/18 --- Text (exceeded DFT_GRAPH_LIMIT)"),
            None
        );
    }
}
