use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsKind {
    Svn,
    Git,
}

impl std::fmt::Display for VcsKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VcsKind::Svn => write!(f, "SVN"),
            VcsKind::Git => write!(f, "Git"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffTarget {
    /// Uncommitted working copy changes (and staged changes in git).
    Uncommitted,
    /// A single commit / revision (e.g. SVN `-c 123` or Git commit hash).
    Commit(String),
    /// A range between two revisions/refs (e.g. SVN `100:120` or Git `main..HEAD`).
    Range { from: String, to: String },
}

impl DiffTarget {
    pub fn parse(spec: &str) -> Self {
        if let Some((a, b)) = spec.split_once(':').or_else(|| spec.split_once("..")) {
            Self::Range {
                from: a.to_string(),
                to: b.to_string(),
            }
        } else {
            Self::Commit(spec.to_string())
        }
    }
}

impl std::fmt::Display for DiffTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffTarget::Uncommitted => write!(f, "uncommitted"),
            DiffTarget::Commit(rev) => write!(f, "commit {}", rev),
            DiffTarget::Range { from, to } => write!(f, "{}..{}", from, to),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Replaced,
    Renamed { from: String },
    PropertyOnly,
}

impl ChangeKind {
    pub fn short_badge(&self) -> &'static str {
        match self {
            ChangeKind::Added => "A",
            ChangeKind::Modified => "M",
            ChangeKind::Deleted => "D",
            ChangeKind::Replaced => "R",
            ChangeKind::Renamed { .. } => "MV",
            ChangeKind::PropertyOnly => "P",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub revision: String,
    pub author: String,
    pub date: String,
    pub message: String,
    pub changed_paths_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    pub is_binary: bool,
    #[serde(default, with = "opt_base64")]
    pub old_content: Option<Vec<u8>>,
    #[serde(default, with = "opt_base64")]
    pub new_content: Option<Vec<u8>>,
    #[serde(default)]
    pub old_size: Option<usize>,
    #[serde(default)]
    pub new_size: Option<usize>,
    pub prop_changes: Option<String>,
}

impl FileChange {
    pub fn old_byte_len(&self) -> usize {
        self.old_content
            .as_ref()
            .map(|b| b.len())
            .or(self.old_size)
            .unwrap_or(0)
    }

    pub fn new_byte_len(&self) -> usize {
        self.new_content
            .as_ref()
            .map(|b| b.len())
            .or(self.new_size)
            .unwrap_or(0)
    }
}

mod opt_base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S>(val: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match val {
            None => serializer.serialize_none(),
            Some(bytes) => {
                let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
                for chunk in bytes.chunks(3) {
                    let b0 = chunk[0] as u32;
                    let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
                    let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
                    let n = (b0 << 16) | (b1 << 8) | b2;
                    out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
                    out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
                    out.push(if chunk.len() > 1 {
                        TABLE[((n >> 6) & 0x3f) as usize] as char
                    } else {
                        '='
                    });
                    out.push(if chunk.len() > 2 {
                        TABLE[(n & 0x3f) as usize] as char
                    } else {
                        '='
                    });
                }
                serializer.serialize_some(&out)
            }
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        let Some(s) = opt else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(s.len() * 3 / 4);
        let bytes = s.as_bytes();
        if bytes.len() % 4 != 0 {
            return Err(serde::de::Error::custom("invalid base64 length"));
        }
        fn decode_char(b: u8) -> Option<u32> {
            match b {
                b'A'..=b'Z' => Some((b - b'A') as u32),
                b'a'..=b'z' => Some((b - b'a' + 26) as u32),
                b'0'..=b'9' => Some((b - b'0' + 52) as u32),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }
        for chunk in bytes.chunks(4) {
            let v0 = decode_char(chunk[0]).ok_or_else(|| serde::de::Error::custom("bad b64"))?;
            let v1 = decode_char(chunk[1]).ok_or_else(|| serde::de::Error::custom("bad b64"))?;
            let pad2 = chunk[2] == b'=';
            let pad3 = chunk[3] == b'=';
            let v2 = if pad2 {
                0
            } else {
                decode_char(chunk[2]).ok_or_else(|| serde::de::Error::custom("bad b64"))?
            };
            let v3 = if pad3 {
                0
            } else {
                decode_char(chunk[3]).ok_or_else(|| serde::de::Error::custom("bad b64"))?
            };
            let n = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
            out.push(((n >> 16) & 0xff) as u8);
            if !pad2 {
                out.push(((n >> 8) & 0xff) as u8);
            }
            if !pad3 {
                out.push((n & 0xff) as u8);
            }
        }
        Ok(Some(out))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DifftDisplayMode {
    #[default]
    SideBySide,
    SideBySideShowBoth,
    Inline,
}

impl DifftDisplayMode {
    pub fn as_cli_arg(&self) -> &'static str {
        match self {
            DifftDisplayMode::SideBySide => "side-by-side",
            DifftDisplayMode::SideBySideShowBoth => "side-by-side-show-both",
            DifftDisplayMode::Inline => "inline",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            DifftDisplayMode::SideBySide => DifftDisplayMode::Inline,
            DifftDisplayMode::Inline => DifftDisplayMode::SideBySideShowBoth,
            DifftDisplayMode::SideBySideShowBoth => DifftDisplayMode::SideBySide,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            DifftDisplayMode::SideBySide => "side-by-side",
            DifftDisplayMode::SideBySideShowBoth => "side-by-side-both",
            DifftDisplayMode::Inline => "inline",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DifftTokenChange {
    pub start: usize,
    pub end: usize,
    pub content: String,
    pub highlight: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DifftLineSide {
    pub line_number: usize,
    #[serde(default)]
    pub changes: Vec<DifftTokenChange>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DifftChunkEntry {
    pub lhs: Option<DifftLineSide>,
    pub rhs: Option<DifftLineSide>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DifftJsonOutput {
    #[serde(default)]
    pub aligned_lines: Vec<(Option<usize>, Option<usize>)>,
    #[serde(default)]
    pub chunks: Vec<Vec<DifftChunkEntry>>,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct HunkMeta {
    pub chunk_index: usize,
    pub local_anchor_line: usize,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct FileDiffRendered {
    pub path: String,
    pub language: String,
    pub status: String,
    pub ansi_output: String,
    pub rendered_width: u16,
    pub rendered_mode: DifftDisplayMode,
    pub json: Option<DifftJsonOutput>,
    /// 0-based line indices inside `ansi_output` that start a new diff hunk/section.
    pub hunk_anchors: Vec<usize>,
    /// Rich hunk metadata linking ANSI anchor lines to source line ranges & chunk indices.
    pub hunks_meta: Vec<HunkMeta>,
    /// Compact structural summary text fed to AI.
    pub ai_summary_diff: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Critical,
    High,
    #[default]
    Medium,
    Low,
    Info,
}

impl RiskLevel {
    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" | "severe" | "blocker" => RiskLevel::Critical,
            "high" => RiskLevel::High,
            "medium" | "moderate" => RiskLevel::Medium,
            "low" => RiskLevel::Low,
            _ => RiskLevel::Info,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            RiskLevel::Critical => "CRITICAL",
            RiskLevel::High => "HIGH",
            RiskLevel::Medium => "MEDIUM",
            RiskLevel::Low => "LOW",
            RiskLevel::Info => "INFO",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    #[default]
    Suggestion,
    Info,
    Praise,
}

impl Severity {
    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" | "error" | "bug" | "high" | "security" => Severity::Critical,
            "warning" | "warn" | "medium" => Severity::Warning,
            "suggestion" | "nit" | "refactor" | "low" => Severity::Suggestion,
            "praise" | "good" | "positive" => Severity::Praise,
            _ => Severity::Info,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Severity::Critical => "CRIT",
            Severity::Warning => "WARN",
            Severity::Suggestion => "SUGG",
            Severity::Info => "INFO",
            Severity::Praise => "GOOD",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGroup {
    pub id: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub risk_level: RiskLevel,
    pub review_order: u32,
    pub files: Vec<String>,
    pub key_points: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAnalysis {
    pub path: String,
    pub category: String,
    pub risk_level: RiskLevel,
    pub summary: String,
    pub review_focus: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkAnalysis {
    pub file: String,
    #[serde(default)]
    pub chunk_index: Option<usize>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub analysis: String,
    #[serde(default)]
    pub risk_level: RiskLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    pub file: String,
    pub line: Option<u32>,
    pub severity: Severity,
    pub title: String,
    pub description: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiReviewResult {
    pub provider_name: String,
    pub summary: String,
    pub overall_risk: RiskLevel,
    pub review_strategy: String,
    pub groups: Vec<ReviewGroup>,
    pub files: Vec<FileAnalysis>,
    #[serde(default)]
    pub hunks: Vec<HunkAnalysis>,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserNote {
    pub file: String,
    pub line: Option<u32>,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewSessionState {
    pub notes: Vec<UserNote>,
    pub file_analysis_map: HashMap<String, FileAnalysis>,
}
