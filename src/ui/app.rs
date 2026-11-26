use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::ai::{self, ProviderDescriptor};
use crate::config::AppConfig;
use crate::difft::{DifftEngine, parse_source_line_from_ansi};
use crate::i18n;
use crate::ignore::IgnoreMatcher;
use crate::model::{
    AiReviewResult, CommitInfo, DiffTarget, DifftDisplayMode, FileDiffRendered, HunkAnalysis,
    ReviewFinding, ReviewGroup, ReviewSessionState, UserNote,
};
use crate::ui::export::export_review_markdown_with_locale;
use crate::vcs::{DiffBundle, VcsBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneFocus {
    Tree,
    Diff,
}

impl PaneFocus {
    pub fn toggle(self) -> Self {
        match self {
            PaneFocus::Tree => PaneFocus::Diff,
            PaneFocus::Diff => PaneFocus::Tree,
        }
    }

    pub fn label(self, locale: &str) -> String {
        match self {
            PaneFocus::Tree => rust_i18n::t!("focus.tree", locale = locale).into_owned(),
            PaneFocus::Diff => rust_i18n::t!("focus.diff", locale = locale).into_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMode {
    AiGrouped,
    FlatFiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Command,
    Search,
    AddNote,
    AskAi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    None,
    Help,
    Settings,
    CommitPicker,
    AiPicker,
    CommitConfirm,
    CommitDoubleConfirm,
    RevertConfirm,
    RevertDoubleConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitModalFocus {
    Files,
    Message,
}

#[derive(Debug, Clone)]
pub struct CommitFileEntry {
    pub path: String,
    pub badge: String,
    pub is_external: bool,
    pub included: bool,
}

#[derive(Debug, Clone)]
pub struct CommitGroupSection {
    pub id: String,
    pub title: String,
    pub file_indices: Vec<usize>,
    pub expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitListRow {
    GroupHeader(usize),
    File(usize),
}

#[derive(Debug, Clone)]
pub enum TreeRow {
    GroupHeader {
        group_id: String,
        title: String,
        description: String,
        category: String,
        risk_label: String,
        file_count: usize,
        expanded: bool,
        order: u32,
        first_change_idx: Option<usize>,
    },
    FileItem {
        path: String,
        change_idx: usize,
        badge: String,
        finding_count: usize,
        in_group: bool,
    },
}

#[derive(Debug, Clone)]
pub struct FileSectionSpan {
    pub change_idx: usize,
    pub path: String,
    pub group_title: Option<String>,
    pub start_y: usize,
    pub body_start_y: usize,
    pub end_y: usize,
}

#[derive(Debug, Clone)]
pub struct GlobalHunkAnchor {
    pub change_idx: usize,
    pub path: String,
    pub global_y: usize,
    pub chunk_index: usize,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
}

#[derive(Debug, Default)]
pub struct ContinuousDiff {
    pub lines: Vec<Line<'static>>,
    pub sections: Vec<FileSectionSpan>,
    pub global_hunks: Vec<GlobalHunkAnchor>,
    pub line_to_file: Vec<Option<usize>>,
    pub line_to_source_line: Vec<Option<u32>>,
    pub line_to_hunk: Vec<Option<usize>>,
}

pub enum BgMessage {
    DiffLoaded {
        generation: u64,
        result: Result<
            (
                DiffBundle,
                Arc<DifftEngine>,
                Vec<FileDiffRendered>,
                Vec<Text<'static>>,
                HashMap<String, usize>,
                Vec<String>,
            ),
            String,
        >,
    },
    DiffReRendered {
        generation: u64,
        diff_generation: u64,
        mode: DifftDisplayMode,
        width: u16,
        rendered_diffs: Vec<FileDiffRendered>,
        parsed_texts: Vec<Text<'static>>,
    },
    AiAnalysisProgress {
        generation: u64,
        update: ai::AiProgressUpdate,
    },
    AiGroupsReady {
        generation: u64,
        session_epoch: u64,
        provider_id: String,
        review: AiReviewResult,
        session_id: Option<String>,
    },
    AiAnalysisDone {
        generation: u64,
        session_epoch: u64,
        provider_id: String,
        result: Result<ai::AnalyzeDiffOutput, String>,
    },
    AiFollowupDone {
        generation: u64,
        diff_generation: u64,
        session_epoch: u64,
        provider_id: String,
        question: String,
        file_path: Option<String>,
        answer: Result<ai::AiPromptOutput, String>,
    },
    CommitsLoaded(Result<Vec<CommitInfo>, String>),
    CommitMsgProgress {
        generation: u64,
        update: ai::AiProgressUpdate,
    },
    CommitMsgGenerated {
        generation: u64,
        cache_key: String,
        result: Result<ai::AiPromptOutput, String>,
    },
    CommitDone(crate::vcs::CommitOutcome),
    RevertDone(crate::vcs::RevertOutcome),
}

pub struct QaEntry {
    pub file: Option<String>,
    pub question: String,
    pub answer: String,
}

pub struct App {
    pub vcs: Arc<dyn VcsBackend>,
    pub config: AppConfig,
    pub bundle: DiffBundle,
    pub difft_engine: Arc<DifftEngine>,
    pub rendered_diffs: Vec<FileDiffRendered>,
    pub path_to_index: HashMap<String, usize>,
    pub parsed_texts: Vec<Text<'static>>,
    pub continuous: ContinuousDiff,
    pub diff_loading: bool,
    pub diff_generation: u64,
    pub rerender_generation: u64,
    pub pending_rerender_at: Option<Instant>,
    pub diff_error: Option<String>,

    pub display_mode: DifftDisplayMode,
    pub focus: PaneFocus,
    pub tree_mode: TreeMode,
    pub user_forced_flat_tree: bool,
    pub input_mode: InputMode,
    pub overlay: OverlayKind,

    pub show_sidebar: bool,
    pub show_inspector: bool,

    pub tree_rows: Vec<TreeRow>,
    pub tree_cursor: usize,
    pub collapsed_groups: HashSet<String>,
    pub selected_file_idx: Option<usize>,

    pub diff_cursor_y: usize,
    pub diff_scroll_y: usize,
    pub diff_scroll_x: u16,
    pub diff_pane_width: u16,
    pub diff_pane_height: u16,

    pub ai_review: Option<AiReviewResult>,
    pub ai_running: bool,
    pub ai_deep_running: bool,
    pub ai_started_at: Option<Instant>,
    pub ai_progress_stage: String,
    pub ai_stream_logs: Vec<String>,
    pub ai_stream_thinking: String,
    pub ai_stream_text: String,
    pub ai_error: Option<String>,
    pub ai_generation: u64,
    pub ai_stage1_completed_gen: u64,
    pub ai_session_epoch: u64,
    pub ai_followup_generation: u64,
    pub ai_cancel_flag: Arc<AtomicBool>,
    pub ai_followup_cancel_flag: Arc<AtomicBool>,
    pub bypass_next_ai_cache: bool,
    pub active_ai_provider_id: String,
    pub resolved_ai_provider_id: Option<String>,
    pub active_ai_session_id: Option<String>,
    pub available_providers: Vec<ProviderDescriptor>,
    pub ai_picker_cursor: usize,
    pub cached_external_prefixes: Vec<String>,
    pub pending_status_override: Option<String>,

    pub commits_cache: Vec<CommitInfo>,
    pub commits_loading: bool,
    pub commit_picker_cursor: usize,

    pub commit_files: Vec<CommitFileEntry>,
    pub commit_groups: Vec<CommitGroupSection>,
    pub commit_rows: Vec<CommitListRow>,
    pub commit_files_cursor: usize,
    pub commit_message: String,
    pub commit_msg_cursor: usize,
    pub commit_msg_dirty: bool,
    pub commit_modal_focus: CommitModalFocus,
    pub commit_editing_msg: bool,
    pub commit_msg_generating: bool,
    pub commit_msg_needs_regen: bool,
    pub commit_started_at: Option<Instant>,
    pub commit_progress_stage: String,
    pub commit_stream_logs: Vec<String>,
    pub commit_stream_thinking: String,
    pub commit_stream_text: String,
    pub commit_executing: bool,
    pub commit_generation: u64,
    pub commit_cancel_flag: Arc<AtomicBool>,
    pub pending_commit_regen_at: Option<Instant>,
    pub commit_msg_cache: HashMap<String, String>,
    pub commit_error: Option<String>,
    pub commit_hint_notice: Option<String>,

    pub revert_title: String,
    pub revert_files: Vec<CommitFileEntry>,
    pub revert_files_cursor: usize,
    pub revert_executing: bool,
    pub revert_error: Option<String>,

    pub session: ReviewSessionState,
    pub qa_history: Vec<QaEntry>,
    pub ai_followup_running: bool,
    pub last_stepped_finding_id: Option<String>,

    pub input_buffer: String,
    pub input_cursor: usize,
    pub search_query: String,
    pub search_matches: Vec<usize>,
    pub search_match_cursor: usize,
    pub inspector_scroll_y: u16,
    pub help_scroll_y: u16,
    pub pending_commits_force_refresh: bool,

    pub pending_key: Option<char>,
    pub status_message: String,
    pub should_quit: bool,

    pub settings_cursor: usize,
    pub settings_status: Option<String>,
    pub settings_ai_dirty: bool,
    pub config_path_override: Option<PathBuf>,

    pub bg_rx: Receiver<BgMessage>,
    bg_tx: mpsc::Sender<BgMessage>,
}

impl App {
    pub fn new(
        vcs: Arc<dyn VcsBackend>,
        config: AppConfig,
        target: DiffTarget,
        initial_width: u16,
    ) -> anyhow::Result<Self> {
        let difft_engine = Arc::new(DifftEngine::new(&config.difft_bin, config.ignore_comments)?);
        let matcher = IgnoreMatcher::load(vcs.repo_root(), vcs.scope_dir());
        let bundle = vcs.collect_changes_filtered(&target, Some(&matcher))?;

        let display_mode = config.display_mode;
        let diff_pane_width = initial_width.saturating_sub(44).max(40);

        let (rendered_diffs, parsed_texts, path_to_index) =
            render_all_files(&difft_engine, &bundle, display_mode, diff_pane_width)?;

        let available_providers = ai::detect_available_providers(&config);
        let active_ai_provider_id = config.ai_provider.clone();
        let active_ai_session_id = if config.reuse_session {
            config.ai_sessions.get(&active_ai_provider_id).cloned()
        } else {
            None
        };
        let (bg_tx, bg_rx) = mpsc::channel();
        let ui_locale = config.ui_language.clone();
        let selected_file_idx = (!rendered_diffs.is_empty()).then_some(0);
        let cached_external_prefixes = vcs.external_prefixes();

        let mut app = Self {
            vcs,
            config,
            bundle,
            difft_engine,
            rendered_diffs,
            path_to_index,
            parsed_texts,
            continuous: ContinuousDiff::default(),
            diff_loading: false,
            diff_generation: 0,
            rerender_generation: 0,
            pending_rerender_at: None,
            diff_error: None,
            display_mode,
            focus: PaneFocus::Tree,
            tree_mode: TreeMode::FlatFiles,
            user_forced_flat_tree: false,
            input_mode: InputMode::Normal,
            overlay: OverlayKind::None,
            show_sidebar: true,
            show_inspector: true,
            tree_rows: Vec::new(),
            tree_cursor: 0,
            collapsed_groups: HashSet::new(),
            selected_file_idx,
            diff_cursor_y: 0,
            diff_scroll_y: 0,
            diff_scroll_x: 0,
            diff_pane_width,
            diff_pane_height: 24,
            ai_review: None,
            ai_running: false,
            ai_deep_running: false,
            ai_started_at: None,
            ai_progress_stage: String::new(),
            ai_stream_logs: Vec::new(),
            ai_stream_thinking: String::new(),
            ai_stream_text: String::new(),
            ai_error: None,
            ai_generation: 0,
            ai_stage1_completed_gen: 0,
            ai_session_epoch: 0,
            ai_followup_generation: 0,
            ai_cancel_flag: Arc::new(AtomicBool::new(false)),
            ai_followup_cancel_flag: Arc::new(AtomicBool::new(false)),
            bypass_next_ai_cache: false,
            active_ai_provider_id,
            resolved_ai_provider_id: None,
            active_ai_session_id,
            available_providers,
            ai_picker_cursor: 0,
            cached_external_prefixes,
            pending_status_override: None,
            commits_cache: Vec::new(),
            commits_loading: false,
            commit_picker_cursor: 0,
            commit_files: Vec::new(),
            commit_groups: Vec::new(),
            commit_rows: Vec::new(),
            commit_files_cursor: 0,
            commit_message: String::new(),
            commit_msg_cursor: 0,
            commit_msg_dirty: false,
            commit_modal_focus: CommitModalFocus::Files,
            commit_editing_msg: false,
            commit_msg_generating: false,
            commit_msg_needs_regen: false,
            commit_started_at: None,
            commit_progress_stage: String::new(),
            commit_stream_logs: Vec::new(),
            commit_stream_thinking: String::new(),
            commit_stream_text: String::new(),
            commit_executing: false,
            commit_generation: 0,
            commit_cancel_flag: Arc::new(AtomicBool::new(false)),
            pending_commit_regen_at: None,
            commit_msg_cache: HashMap::new(),
            commit_error: None,
            commit_hint_notice: None,
            revert_title: String::new(),
            revert_files: Vec::new(),
            revert_files_cursor: 0,
            revert_executing: false,
            revert_error: None,
            session: ReviewSessionState::default(),
            qa_history: Vec::new(),
            ai_followup_running: false,
            last_stepped_finding_id: None,
            input_buffer: String::new(),
            input_cursor: 0,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_match_cursor: 0,
            inspector_scroll_y: 0,
            help_scroll_y: 0,
            pending_commits_force_refresh: false,
            pending_key: None,
            status_message: rust_i18n::t!("status.startup_hint", locale = ui_locale.as_str())
                .into_owned(),
            should_quit: false,
            settings_cursor: 0,
            settings_status: None,
            settings_ai_dirty: false,
            config_path_override: None,
            bg_rx,
            bg_tx,
        };

        app.rebuild_tree_and_continuous();
        app.trigger_ai_analysis(false);
        Ok(app)
    }

    /// Enter TUI immediately and fetch VCS changes + Difftastic output in background.
    pub fn new_async(
        vcs: Arc<dyn VcsBackend>,
        config: AppConfig,
        target: DiffTarget,
        initial_width: u16,
    ) -> anyhow::Result<Self> {
        let difft_engine = Arc::new(DifftEngine::new(&config.difft_bin, config.ignore_comments)?);
        let display_mode = config.display_mode;
        let diff_pane_width = initial_width.saturating_sub(44).max(40);
        let available_providers = ai::detect_available_providers(&config);
        let active_ai_provider_id = config.ai_provider.clone();
        let active_ai_session_id = if config.reuse_session {
            config.ai_sessions.get(&active_ai_provider_id).cloned()
        } else {
            None
        };
        let ui_locale = config.ui_language.clone();
        let (bg_tx, bg_rx) = mpsc::channel();

        let empty_bundle = DiffBundle {
            vcs: vcs.kind(),
            repo_root: vcs.repo_root().to_path_buf(),
            path_base_dir: vcs.scope_dir().to_path_buf(),
            target: target.clone(),
            commit_info: None,
            changes: Vec::new(),
        };

        let mut app = Self {
            vcs,
            config,
            bundle: empty_bundle,
            difft_engine,
            rendered_diffs: Vec::new(),
            path_to_index: HashMap::new(),
            parsed_texts: Vec::new(),
            continuous: ContinuousDiff::default(),
            diff_loading: false,
            diff_generation: 0,
            rerender_generation: 0,
            pending_rerender_at: None,
            diff_error: None,
            display_mode,
            focus: PaneFocus::Tree,
            tree_mode: TreeMode::FlatFiles,
            user_forced_flat_tree: false,
            input_mode: InputMode::Normal,
            overlay: OverlayKind::None,
            show_sidebar: true,
            show_inspector: true,
            tree_rows: Vec::new(),
            tree_cursor: 0,
            collapsed_groups: HashSet::new(),
            selected_file_idx: None,
            diff_cursor_y: 0,
            diff_scroll_y: 0,
            diff_scroll_x: 0,
            diff_pane_width,
            diff_pane_height: 24,
            ai_review: None,
            ai_running: false,
            ai_deep_running: false,
            ai_started_at: None,
            ai_progress_stage: String::new(),
            ai_stream_logs: Vec::new(),
            ai_stream_thinking: String::new(),
            ai_stream_text: String::new(),
            ai_error: None,
            ai_generation: 0,
            ai_stage1_completed_gen: 0,
            ai_session_epoch: 0,
            ai_followup_generation: 0,
            ai_cancel_flag: Arc::new(AtomicBool::new(false)),
            ai_followup_cancel_flag: Arc::new(AtomicBool::new(false)),
            bypass_next_ai_cache: false,
            active_ai_provider_id,
            resolved_ai_provider_id: None,
            active_ai_session_id,
            available_providers,
            ai_picker_cursor: 0,
            cached_external_prefixes: Vec::new(),
            pending_status_override: None,
            commits_cache: Vec::new(),
            commits_loading: false,
            commit_picker_cursor: 0,
            commit_files: Vec::new(),
            commit_groups: Vec::new(),
            commit_rows: Vec::new(),
            commit_files_cursor: 0,
            commit_message: String::new(),
            commit_msg_cursor: 0,
            commit_msg_dirty: false,
            commit_modal_focus: CommitModalFocus::Files,
            commit_editing_msg: false,
            commit_msg_generating: false,
            commit_msg_needs_regen: false,
            commit_started_at: None,
            commit_progress_stage: String::new(),
            commit_stream_logs: Vec::new(),
            commit_stream_thinking: String::new(),
            commit_stream_text: String::new(),
            commit_executing: false,
            commit_generation: 0,
            commit_cancel_flag: Arc::new(AtomicBool::new(false)),
            pending_commit_regen_at: None,
            commit_msg_cache: HashMap::new(),
            commit_error: None,
            commit_hint_notice: None,
            revert_title: String::new(),
            revert_files: Vec::new(),
            revert_files_cursor: 0,
            revert_executing: false,
            revert_error: None,
            session: ReviewSessionState::default(),
            qa_history: Vec::new(),
            ai_followup_running: false,
            last_stepped_finding_id: None,
            input_buffer: String::new(),
            input_cursor: 0,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_match_cursor: 0,
            inspector_scroll_y: 0,
            help_scroll_y: 0,
            pending_commits_force_refresh: false,
            pending_key: None,
            status_message: rust_i18n::t!("status.loading_diff", locale = ui_locale.as_str())
                .into_owned(),
            should_quit: false,
            settings_cursor: 0,
            settings_status: None,
            settings_ai_dirty: false,
            config_path_override: None,
            bg_rx,
            bg_tx,
        };

        app.reload_target(target)?;
        Ok(app)
    }

    pub fn locale(&self) -> &str {
        self.config.ui_language.as_str()
    }

    pub fn reload_target(&mut self, target: DiffTarget) -> anyhow::Result<()> {
        self.reload_target_with_options(target, false)
    }

    pub fn reload_target_with_options(
        &mut self,
        target: DiffTarget,
        force_refresh: bool,
    ) -> anyhow::Result<()> {
        self.diff_generation += 1;
        let current_gen = self.diff_generation;
        self.diff_loading = true;
        self.diff_error = None;
        self.ai_generation += 1;
        self.ai_stage1_completed_gen = 0;
        self.ai_followup_generation += 1;
        self.commit_generation += 1;
        self.clear_ai_runtime_flags();
        self.commit_msg_cache.clear();
        self.commit_error = None;
        self.commit_hint_notice = None;
        self.ai_review = None;
        self.ai_error = None;
        self.qa_history.clear();
        self.last_stepped_finding_id = None;
        self.user_forced_flat_tree = false;
        self.bundle = DiffBundle {
            vcs: self.vcs.kind(),
            repo_root: self.vcs.repo_root().to_path_buf(),
            path_base_dir: self.vcs.scope_dir().to_path_buf(),
            target: target.clone(),
            commit_info: None,
            changes: Vec::new(),
        };
        self.rendered_diffs.clear();
        self.parsed_texts.clear();
        self.path_to_index.clear();
        self.continuous = ContinuousDiff::default();
        self.selected_file_idx = None;
        self.tree_cursor = 0;
        self.diff_cursor_y = 0;
        self.diff_scroll_y = 0;
        self.diff_scroll_x = 0;
        self.session = ReviewSessionState::default();
        self.tree_mode = TreeMode::FlatFiles;
        self.rebuild_tree_rows();
        self.status_message = rust_i18n::t!(
            "status.fetching_diff",
            locale = self.locale(),
            target = target
        )
        .into_owned();

        let vcs = Arc::clone(&self.vcs);
        let difft_bin = self.config.difft_bin.clone();
        let ignore_comments = self.config.ignore_comments;
        let display_mode = self.display_mode;
        let width = self.diff_pane_width;
        let cache_history = self.config.cache_history;
        let tx = self.bg_tx.clone();

        thread::spawn(move || {
            let res = (|| -> anyhow::Result<_> {
                let matcher = IgnoreMatcher::load(vcs.repo_root(), vcs.scope_dir());
                let bundle = crate::vcs::collect_changes_cached(
                    &*vcs,
                    &target,
                    Some(&matcher),
                    cache_history,
                    force_refresh,
                )?;
                let externals = vcs.external_prefixes();
                let engine = Arc::new(DifftEngine::new(&difft_bin, ignore_comments)?);
                let (rendered_diffs, parsed_texts, path_to_index) =
                    render_all_files(&engine, &bundle, display_mode, width)?;
                Ok((
                    bundle,
                    engine,
                    rendered_diffs,
                    parsed_texts,
                    path_to_index,
                    externals,
                ))
            })()
            .map_err(|e| format!("{:#}", e));

            let _ = tx.send(BgMessage::DiffLoaded {
                generation: current_gen,
                result: res,
            });
        });

        Ok(())
    }

    fn cancel_analysis_task(&mut self) {
        self.ai_cancel_flag.store(true, Ordering::Relaxed);
        self.ai_cancel_flag = Arc::new(AtomicBool::new(false));
    }

    fn cancel_followup_task(&mut self) {
        self.ai_followup_cancel_flag.store(true, Ordering::Relaxed);
        self.ai_followup_cancel_flag = Arc::new(AtomicBool::new(false));
    }

    fn cancel_active_ai_tasks(&mut self) {
        self.cancel_analysis_task();
        self.cancel_followup_task();
        self.cancel_commit_msg_task();
    }

    fn clear_ai_runtime_flags(&mut self) {
        self.cancel_active_ai_tasks();
        self.ai_running = false;
        self.ai_deep_running = false;
        self.ai_started_at = None;
        self.ai_progress_stage.clear();
        self.ai_stream_logs.clear();
        self.ai_stream_thinking.clear();
        self.ai_stream_text.clear();
        self.ai_followup_running = false;
        self.commit_msg_generating = false;
        self.pending_commit_regen_at = None;
        self.commit_started_at = None;
        self.commit_progress_stage.clear();
        self.commit_stream_logs.clear();
        self.commit_stream_thinking.clear();
        self.commit_stream_text.clear();
    }

    fn cancel_commit_msg_task(&mut self) {
        self.commit_cancel_flag.store(true, Ordering::Relaxed);
        self.commit_cancel_flag = Arc::new(AtomicBool::new(false));
    }

    pub fn trigger_ai_analysis(&mut self, bypass_cache: bool) {
        if self.bundle.changes.is_empty() {
            self.cancel_analysis_task();
            self.ai_running = false;
            self.ai_deep_running = false;
            self.ai_started_at = None;
            self.status_message =
                rust_i18n::t!("status.no_changes", locale = self.locale()).into_owned();
            return;
        }

        let Some(provider) = ai::resolve_provider(&self.active_ai_provider_id, &self.config) else {
            self.cancel_analysis_task();
            self.ai_running = false;
            self.ai_deep_running = false;
            self.ai_started_at = None;
            self.resolved_ai_provider_id = None;
            if self.active_ai_provider_id.eq_ignore_ascii_case("none")
                || self.active_ai_provider_id.eq_ignore_ascii_case("off")
            {
                self.ai_error = None;
                self.status_message =
                    rust_i18n::t!("status.ai_disabled", locale = self.locale()).into_owned();
            } else {
                self.ai_error = Some(
                    rust_i18n::t!(
                        "status.ai_missing_cli",
                        locale = self.locale(),
                        provider = self.active_ai_provider_id
                    )
                    .into_owned(),
                );
            }
            return;
        };

        let provider_id = provider.id().to_string();
        self.resolved_ai_provider_id = Some(provider_id.clone());
        self.active_ai_session_id = if self.config.reuse_session {
            self.config.ai_sessions.get(&provider_id).cloned()
        } else {
            None
        };

        // Cancel any previous in-flight Stage 1 / Stage 2 worker before launching a new run
        self.cancel_analysis_task();
        self.ai_generation += 1;
        let current_gen = self.ai_generation;
        let current_epoch = self.ai_session_epoch;
        let cancel_flag = Arc::clone(&self.ai_cancel_flag);
        self.ai_running = true;
        self.ai_deep_running = false;
        self.ai_started_at = Some(Instant::now());
        self.ai_progress_stage = rust_i18n::t!(
            "status.ai_preparing",
            locale = self.locale(),
            provider = provider.id(),
            count = self.bundle.changes.len()
        )
        .into_owned();
        self.ai_stream_logs.clear();
        self.ai_stream_thinking.clear();
        self.ai_stream_text.clear();
        self.ai_error = None;
        self.status_message = rust_i18n::t!(
            "status.ai_grouping_short",
            locale = self.locale(),
            provider = provider.id(),
            count = self.bundle.changes.len()
        )
        .into_owned();

        let bundle = self.bundle.clone();
        let diffs = self.rendered_diffs.clone();
        let cfg = self.config.clone();
        let fast_cfg = cfg.fast_config();
        let fast_provider = ai::resolve_provider(&self.active_ai_provider_id, &fast_cfg);
        let tx = self.bg_tx.clone();
        let tx_prog = tx.clone();
        let tx_grp = tx.clone();
        let prov_for_grp = provider_id.clone();

        thread::spawn(move || {
            let progress_cb: ai::AiProgressCallback = Arc::new(move |update| {
                let _ = tx_prog.send(BgMessage::AiAnalysisProgress {
                    generation: current_gen,
                    update,
                });
            });
            let res = i18n::run_with_locale(&cfg.ui_language, || {
                ai::run_with_ai_hooks(cancel_flag, Some(progress_cb), || {
                    let fast_ref = fast_provider.as_deref().unwrap_or(provider.as_ref());
                    ai::analyze_diff_two_stage(
                        fast_ref,
                        provider.as_ref(),
                        &bundle,
                        &diffs,
                        &cfg,
                        bypass_cache,
                        |stage1_review, sid| {
                            let _ = tx_grp.send(BgMessage::AiGroupsReady {
                                generation: current_gen,
                                session_epoch: current_epoch,
                                provider_id: prov_for_grp,
                                review: stage1_review.clone(),
                                session_id: sid.map(str::to_string),
                            });
                        },
                    )
                })
            })
            .map_err(|e| format!("{:#}", e));
            let _ = tx.send(BgMessage::AiAnalysisDone {
                generation: current_gen,
                session_epoch: current_epoch,
                provider_id,
                result: res,
            });
        });
    }

    fn persist_provider_session(
        &mut self,
        provider_id: &str,
        expected_epoch: u64,
        session_id: Option<String>,
    ) {
        if !self.config.reuse_session || expected_epoch != self.ai_session_epoch {
            return;
        }
        if let Some(active) = &self.resolved_ai_provider_id {
            if active != provider_id {
                return;
            }
        }
        let Some(sid) = session_id.filter(|s| !s.trim().is_empty()) else {
            return;
        };
        if self
            .config
            .ai_sessions
            .get(provider_id)
            .map(String::as_str)
            == Some(sid.as_str())
        {
            self.active_ai_session_id = Some(sid);
            return;
        }
        self.config
            .ai_sessions
            .insert(provider_id.to_string(), sid.clone());
        self.active_ai_session_id = Some(sid.clone());
        let _ = crate::config::save_project_session(
            self.vcs.repo_root(),
            self.vcs.scope_dir(),
            provider_id,
            Some(&sid),
        );
    }

    pub fn reset_ai_session(&mut self) {
        let provider_id = ai::resolve_provider(&self.active_ai_provider_id, &self.config)
            .map(|p| p.id().to_string())
            .unwrap_or_else(|| self.active_ai_provider_id.clone());
        self.ai_generation += 1;
        self.ai_stage1_completed_gen = 0;
        self.ai_session_epoch += 1;
        self.ai_followup_generation += 1;
        self.commit_generation += 1;
        self.clear_ai_runtime_flags();
        self.commit_msg_cache.clear();
        self.config.ai_sessions.remove(&provider_id);
        self.active_ai_session_id = None;
        let _ = crate::config::save_project_session(
            self.vcs.repo_root(),
            self.vcs.scope_dir(),
            &provider_id,
            None,
        );
        self.status_message = rust_i18n::t!(
            "status.ai_session_cleared",
            locale = self.locale(),
            provider = provider_id
        )
        .into_owned();
        self.trigger_ai_analysis(true);
    }

    pub fn poll_background(&mut self) {
        if let Some(deadline) = self.pending_rerender_at {
            if Instant::now() >= deadline {
                self.pending_rerender_at = None;
                if !self.bundle.changes.is_empty() && !self.diff_loading {
                    self.re_render_all_for_mode_or_width();
                }
            }
        }
        if let Some(deadline) = self.pending_commit_regen_at {
            if Instant::now() >= deadline {
                self.pending_commit_regen_at = None;
                if matches!(
                    self.overlay,
                    OverlayKind::CommitConfirm | OverlayKind::CommitDoubleConfirm
                ) && !self.commit_editing_msg
                    && !self.commit_msg_dirty
                {
                    self.regenerate_commit_message();
                }
            }
        }
        while let Ok(msg) = self.bg_rx.try_recv() {
            match msg {
                BgMessage::DiffLoaded { generation, result } => {
                    if generation != self.diff_generation {
                        continue;
                    }
                    self.diff_loading = false;
                    match result {
                        Ok((
                            bundle,
                            engine,
                            rendered_diffs,
                            parsed_texts,
                            path_to_index,
                            externals,
                        )) => {
                            self.bundle = bundle;
                            self.difft_engine = engine;
                            self.rendered_diffs = rendered_diffs;
                            self.parsed_texts = parsed_texts;
                            self.path_to_index = path_to_index;
                            self.cached_external_prefixes = externals;
                            self.selected_file_idx =
                                (!self.rendered_diffs.is_empty()).then_some(0);
                            self.rebuild_tree_and_continuous();
                            if self.rendered_diffs.iter().any(|r| {
                                r.rendered_width != self.diff_pane_width
                                    || r.rendered_mode != self.display_mode
                            }) {
                                self.re_render_all_for_mode_or_width();
                            }
                            self.status_message = if let Some(override_msg) =
                                self.pending_status_override.take()
                            {
                                override_msg
                            } else {
                                rust_i18n::t!(
                                    "status.diff_loaded",
                                    locale = self.locale(),
                                    target = self.bundle.target,
                                    count = self.bundle.changes.len()
                                )
                                .into_owned()
                            };
                            let bypass_cache = std::mem::take(&mut self.bypass_next_ai_cache);
                            self.trigger_ai_analysis(bypass_cache);
                        }
                        Err(err) => {
                            self.pending_status_override = None;
                            self.diff_error = Some(err.clone());
                            self.status_message = rust_i18n::t!(
                                "status.diff_load_failed",
                                locale = self.locale(),
                                err = truncate(&err, 80)
                            )
                            .into_owned();
                        }
                    }
                }
                BgMessage::DiffReRendered {
                    generation,
                    diff_generation,
                    mode,
                    width,
                    rendered_diffs,
                    parsed_texts,
                } => {
                    if generation != self.rerender_generation
                        || diff_generation != self.diff_generation
                        || mode != self.display_mode
                        || width != self.diff_pane_width
                        || rendered_diffs.len() != self.bundle.changes.len()
                        || parsed_texts.len() != self.bundle.changes.len()
                    {
                        continue;
                    }
                    self.rendered_diffs = rendered_diffs;
                    self.parsed_texts = parsed_texts;
                    self.rebuild_continuous_diff();
                    if !self.search_query.is_empty() {
                        self.recompute_search_matches();
                    }
                }
                BgMessage::AiAnalysisProgress { generation, update } => {
                    if generation != self.ai_generation
                        || (!self.ai_running && !self.ai_deep_running)
                    {
                        continue;
                    }
                    if let Some(stage) = apply_progress_update(
                        self.ai_started_at,
                        &mut self.ai_progress_stage,
                        &mut self.ai_stream_logs,
                        &mut self.ai_stream_thinking,
                        &mut self.ai_stream_text,
                        update,
                    ) {
                        if self.ai_running {
                            let elapsed = self
                                .ai_started_at
                                .map(|t| t.elapsed().as_secs_f32())
                                .unwrap_or(0.0);
                            self.status_message = rust_i18n::t!(
                                "status.ai_grouping_elapsed",
                                locale = self.locale(),
                                secs = format!("{:.0}", elapsed),
                                stage = truncate(&stage, 72)
                            )
                            .into_owned();
                        }
                    }
                }
                BgMessage::AiGroupsReady {
                    generation,
                    session_epoch,
                    provider_id,
                    mut review,
                    session_id,
                } => {
                    if generation != self.ai_generation {
                        continue;
                    }
                    self.ai_running = false;
                    self.ai_deep_running = true;
                    self.ai_stage1_completed_gen = generation;
                    self.ai_started_at = Some(Instant::now());
                    self.ai_stream_logs.clear();
                    self.ai_stream_thinking.clear();
                    self.ai_stream_text.clear();
                    self.ai_progress_stage =
                        rust_i18n::t!("status.ai_analyzing_risks", locale = self.locale())
                            .into_owned();
                    self.persist_provider_session(&provider_id, session_epoch, session_id);
                    self.retain_review_active_paths(&mut review);
                    let group_cnt = review.groups.len();
                    self.ai_review = Some(review);
                    if !self.user_forced_flat_tree {
                        self.tree_mode = TreeMode::AiGrouped;
                    }
                    self.rebuild_tree_and_continuous();
                    self.sync_tree_to_diff_cursor();
                    self.status_message = rust_i18n::t!(
                        "status.ai_groups_ready",
                        locale = self.locale(),
                        count = group_cnt
                    )
                    .into_owned();
                }
                BgMessage::AiAnalysisDone {
                    generation,
                    session_epoch,
                    provider_id,
                    result,
                } => {
                    if generation != self.ai_generation {
                        continue;
                    }
                    self.ai_running = false;
                    self.ai_deep_running = false;
                    self.ai_started_at = None;
                    self.ai_stream_thinking.clear();
                    self.ai_stream_text.clear();
                    match result {
                        Ok(out) => {
                            self.persist_provider_session(
                                &provider_id,
                                session_epoch,
                                out.session_id,
                            );
                            let mut review = out.review;
                            self.retain_review_active_paths(&mut review);
                            let group_cnt = review.groups.len();
                            let finding_cnt = review.findings.len();
                            self.session.file_analysis_map.clear();
                            for fa in &review.files {
                                self.session
                                    .file_analysis_map
                                    .insert(fa.path.clone(), fa.clone());
                            }
                            self.ai_review = Some(review);
                            if !self.user_forced_flat_tree {
                                self.tree_mode = TreeMode::AiGrouped;
                            }
                            self.rebuild_tree_and_continuous();
                            self.sync_tree_to_diff_cursor();
                            self.status_message = rust_i18n::t!(
                                "status.ai_ready",
                                locale = self.locale(),
                                groups = group_cnt,
                                findings = finding_cnt
                            )
                            .into_owned();
                        }
                        Err(err) => {
                            if self.ai_stage1_completed_gen == generation && self.ai_review.is_some()
                            {
                                self.ai_error = None;
                                self.status_message = rust_i18n::t!(
                                    "status.ai_groups_ready_deep_failed",
                                    locale = self.locale(),
                                    err = truncate(&err, 60)
                                )
                                .into_owned();
                            } else {
                                self.ai_error = Some(err.clone());
                                self.status_message = rust_i18n::t!(
                                    "status.ai_failed",
                                    locale = self.locale(),
                                    err = truncate(&err, 80)
                                )
                                .into_owned();
                            }
                        }
                    }
                }
                BgMessage::AiFollowupDone {
                    generation,
                    diff_generation,
                    session_epoch,
                    provider_id,
                    question,
                    file_path,
                    answer,
                } => {
                    if generation != self.ai_followup_generation
                        || diff_generation != self.diff_generation
                    {
                        continue;
                    }
                    self.ai_followup_running = false;
                    match answer {
                        Ok(out) => {
                            self.persist_provider_session(
                                &provider_id,
                                session_epoch,
                                out.session_id,
                            );
                            self.qa_history.push(QaEntry {
                                file: file_path,
                                question,
                                answer: out.text,
                            });
                            self.show_inspector = true;
                            self.status_message =
                                rust_i18n::t!("status.ai_answer_added", locale = self.locale())
                                    .into_owned();
                        }
                        Err(e) => {
                            self.status_message = rust_i18n::t!(
                                "status.ai_question_failed",
                                locale = self.locale(),
                                err = truncate(&e, 80)
                            )
                            .into_owned();
                        }
                    }
                }
                BgMessage::CommitsLoaded(res) => {
                    self.commits_loading = false;
                    match res {
                        Ok(commits) => {
                            let changed = self.commits_cache.len() != commits.len()
                                || self.commits_cache.first().map(|c| &c.revision)
                                    != commits.first().map(|c| &c.revision);
                            self.commits_cache = commits;
                            if changed {
                                self.commit_picker_cursor =
                                    self.commit_picker_cursor.min(self.commits_cache.len());
                            }
                        }
                        Err(e) => {
                            self.status_message = rust_i18n::t!(
                                "status.commits_load_failed",
                                locale = self.locale(),
                                err = e
                            )
                            .into_owned();
                        }
                    }
                    if self.pending_commits_force_refresh {
                        self.pending_commits_force_refresh = false;
                        self.spawn_commits_refresh(true);
                    }
                }
                BgMessage::CommitMsgProgress { generation, update } => {
                    if generation != self.commit_generation || !self.commit_msg_generating {
                        continue;
                    }
                    apply_progress_update(
                        self.commit_started_at,
                        &mut self.commit_progress_stage,
                        &mut self.commit_stream_logs,
                        &mut self.commit_stream_thinking,
                        &mut self.commit_stream_text,
                        update,
                    );
                }
                BgMessage::CommitMsgGenerated {
                    generation,
                    cache_key,
                    result,
                } => {
                    if generation != self.commit_generation {
                        continue;
                    }
                    self.commit_msg_generating = false;
                    self.commit_started_at = None;
                    self.commit_hint_notice = None;
                    match result {
                        Ok(out) => {
                            if !out.text.trim().is_empty() {
                                self.commit_msg_cache.insert(cache_key, out.text.clone());
                                if !self.commit_msg_dirty && !self.commit_editing_msg {
                                    self.commit_message = out.text;
                                    self.commit_msg_cursor = self.commit_message.len();
                                }
                            }
                        }
                        Err(e) => {
                            self.commit_error = Some(
                                rust_i18n::t!(
                                    "commit.gen_failed",
                                    locale = self.locale(),
                                    err = truncate(&e, 90)
                                )
                                .into_owned(),
                            );
                        }
                    }
                }
                BgMessage::CommitDone(outcome) => {
                    self.commit_executing = false;
                    if let Some(err) = outcome.error {
                        if !outcome.committed_paths.is_empty() {
                            let committed: HashSet<String> =
                                outcome.committed_paths.into_iter().collect();
                            self.commit_files.retain(|f| !committed.contains(&f.path));
                            self.rebuild_commit_groups_from_files();
                            let _ = self.reload_target_with_options(DiffTarget::Uncommitted, true);
                            self.spawn_commits_refresh(true);
                            self.commit_error = Some(
                                rust_i18n::t!(
                                    "commit.partial",
                                    locale = self.locale(),
                                    summary = outcome.summary,
                                    err = err
                                )
                                .into_owned(),
                            );
                        } else {
                            self.commit_error = Some(err);
                        }
                    } else {
                        self.overlay = OverlayKind::None;
                        self.commit_editing_msg = false;
                        self.commit_error = None;
                        let msg = rust_i18n::t!(
                            "commit.succeeded",
                            locale = self.locale(),
                            summary = truncate(&outcome.summary, 90)
                        )
                        .into_owned();
                        self.pending_status_override = Some(msg.clone());
                        let _ = self.reload_target_with_options(DiffTarget::Uncommitted, true);
                        self.spawn_commits_refresh(true);
                        self.status_message = msg;
                    }
                }
                BgMessage::RevertDone(outcome) => {
                    self.revert_executing = false;
                    if let Some(err) = outcome.error {
                        if !outcome.reverted_paths.is_empty() {
                            let reverted: HashSet<String> =
                                outcome.reverted_paths.into_iter().collect();
                            self.revert_files.retain(|f| !reverted.contains(&f.path));
                            if self.revert_files_cursor >= self.revert_files.len()
                                && !self.revert_files.is_empty()
                            {
                                self.revert_files_cursor = self.revert_files.len() - 1;
                            }
                            let _ = self.reload_target_with_options(DiffTarget::Uncommitted, true);
                            self.revert_error = Some(
                                rust_i18n::t!(
                                    "revert.partial",
                                    locale = self.locale(),
                                    summary = outcome.summary,
                                    err = err
                                )
                                .into_owned(),
                            );
                        } else {
                            self.revert_error = Some(err);
                        }
                    } else {
                        self.overlay = OverlayKind::None;
                        self.revert_error = None;
                        let msg = rust_i18n::t!(
                            "revert.succeeded",
                            locale = self.locale(),
                            summary = truncate(&outcome.summary, 90)
                        )
                        .into_owned();
                        self.pending_status_override = Some(msg.clone());
                        let _ = self.reload_target_with_options(DiffTarget::Uncommitted, true);
                        self.status_message = msg;
                    }
                }
            }
        }
    }

    pub fn ensure_diff_rendered_for_width(&mut self, pane_width: u16, pane_height: u16) {
        self.diff_pane_height = pane_height.max(1);
        let target_w = pane_width.max(40);
        if (target_w as i32 - self.diff_pane_width as i32).abs() >= 4 {
            self.diff_pane_width = target_w;
            if !self.bundle.changes.is_empty() && !self.diff_loading {
                self.pending_rerender_at = Some(Instant::now() + Duration::from_millis(120));
            }
        }
    }

    fn re_render_all_for_mode_or_width(&mut self) {
        if self.bundle.changes.is_empty() {
            return;
        }
        self.rerender_generation += 1;
        let rerender_gen = self.rerender_generation;
        let diff_gen = self.diff_generation;
        let engine = Arc::clone(&self.difft_engine);
        let changes = self.bundle.changes.clone();
        let mut rendered_diffs = self.rendered_diffs.clone();
        let mode = self.display_mode;
        let width = self.diff_pane_width;
        let tx = self.bg_tx.clone();

        thread::spawn(move || {
            let needs_any = rendered_diffs
                .iter()
                .any(|r| r.rendered_width != width || r.rendered_mode != mode);
            if !needs_any {
                return;
            }
            let workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(2, 8)
                .min(changes.len().max(1));
            let next_idx = std::sync::atomic::AtomicUsize::new(0);
            let slots: Vec<std::sync::Mutex<Option<FileDiffRendered>>> =
                (0..changes.len()).map(|_| std::sync::Mutex::new(None)).collect();
            std::thread::scope(|s| {
                for _ in 0..workers {
                    s.spawn(|| loop {
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= changes.len() {
                            break;
                        }
                        let mut rendered = rendered_diffs[idx].clone();
                        if rendered.rendered_width != width || rendered.rendered_mode != mode {
                            let _ = engine.re_render_ansi(&changes[idx], &mut rendered, mode, width);
                        }
                        *slots[idx].lock().unwrap() = Some(rendered);
                    });
                }
            });
            rendered_diffs = slots
                .into_iter()
                .map(|m| m.into_inner().unwrap().expect("rerender slot"))
                .collect();
            let parsed_texts: Vec<Text<'static>> = rendered_diffs
                .iter()
                .map(|r| ansi_to_ratatui_text(&r.ansi_output))
                .collect();
            let _ = tx.send(BgMessage::DiffReRendered {
                generation: rerender_gen,
                diff_generation: diff_gen,
                mode,
                width,
                rendered_diffs,
                parsed_texts,
            });
        });
    }

    pub fn rebuild_tree_and_continuous(&mut self) {
        self.rebuild_tree_rows();
        self.rebuild_continuous_diff();
        if !self.search_query.is_empty() {
            self.recompute_search_matches();
        }
    }

    pub fn rebuild_tree_rows(&mut self) {
        let mut rows = Vec::new();
        let finding_counts: HashMap<&str, usize> = if let Some(ai) = &self.ai_review {
            let mut map = HashMap::new();
            for f in &ai.findings {
                *map.entry(f.file.as_str()).or_insert(0) += 1;
            }
            map
        } else {
            HashMap::new()
        };

        if self.tree_mode == TreeMode::AiGrouped {
            if let Some(ai) = &self.ai_review {
                for g in &ai.groups {
                    let expanded = !self.collapsed_groups.contains(&g.id);
                    let valid_files: Vec<(&String, usize)> = g
                        .files
                        .iter()
                        .filter_map(|p| self.path_to_index.get(p).map(|&idx| (p, idx)))
                        .collect();
                    if valid_files.is_empty() {
                        continue;
                    }
                    let first_change_idx = valid_files.first().map(|(_, idx)| *idx);

                    rows.push(TreeRow::GroupHeader {
                        group_id: g.id.clone(),
                        title: g.title.clone(),
                        description: g.description.clone(),
                        category: g.category.clone(),
                        risk_label: g.risk_level.label().to_string(),
                        file_count: valid_files.len(),
                        expanded,
                        order: g.review_order,
                        first_change_idx,
                    });

                    if expanded {
                        for (path, idx) in valid_files {
                            let badge = self.bundle.changes[idx].kind.short_badge().to_string();
                            rows.push(TreeRow::FileItem {
                                path: path.clone(),
                                change_idx: idx,
                                badge,
                                finding_count: finding_counts.get(path.as_str()).copied().unwrap_or(0),
                                in_group: true,
                            });
                        }
                    }
                }
                self.tree_rows = rows;
                if self.tree_cursor >= self.tree_rows.len() && !self.tree_rows.is_empty() {
                    self.tree_cursor = self.tree_rows.len() - 1;
                }
                return;
            }
        }

        // Flat file list
        for (idx, ch) in self.bundle.changes.iter().enumerate() {
            rows.push(TreeRow::FileItem {
                path: ch.path.clone(),
                change_idx: idx,
                badge: ch.kind.short_badge().to_string(),
                finding_count: finding_counts.get(ch.path.as_str()).copied().unwrap_or(0),
                in_group: false,
            });
        }
        self.tree_rows = rows;
        if self.tree_cursor >= self.tree_rows.len() && !self.tree_rows.is_empty() {
            self.tree_cursor = self.tree_rows.len() - 1;
        }
    }

    pub fn rebuild_continuous_diff(&mut self) {
        let prev_file_idx = self.selected_file_idx;
        let prev_offset_in_file = prev_file_idx.and_then(|cidx| {
            self.continuous
                .sections
                .iter()
                .find(|s| s.change_idx == cidx)
                .map(|s| self.diff_cursor_y.saturating_sub(s.start_y))
        });

        let mut continuous = ContinuousDiff::default();
        if self.rendered_diffs.is_empty() {
            self.continuous = continuous;
            self.diff_cursor_y = 0;
            self.diff_scroll_y = 0;
            return;
        }

        // Order files by AI semantic groups if active, deduplicating any repeated path
        let mut ordered_entries: Vec<(usize, Option<String>)> = Vec::new();
        let mut seen = HashSet::new();

        if self.tree_mode == TreeMode::AiGrouped {
            if let Some(ai) = &self.ai_review {
                for g in &ai.groups {
                    for p in &g.files {
                        if let Some(&idx) = self.path_to_index.get(p) {
                            if seen.insert(idx) {
                                ordered_entries.push((idx, Some(format!("{}. {}", g.review_order, g.title))));
                            }
                        }
                    }
                }
            }
        }
        for idx in 0..self.rendered_diffs.len() {
            if seen.insert(idx) {
                ordered_entries.push((idx, None));
            }
        }

        let total_sections = ordered_entries.len();
        for (sec_no, (change_idx, group_title)) in ordered_entries.into_iter().enumerate() {
            let fd = &self.rendered_diffs[change_idx];
            let ch = &self.bundle.changes[change_idx];
            let start_y = continuous.lines.len();

            let grp_badge = group_title
                .as_ref()
                .map(|gt| format!("  [{}]", gt))
                .unwrap_or_default();

            let banner = Line::from(vec![
                Span::styled(
                    format!("━━ [{}/{}] [{}] {} ", sec_no + 1, total_sections, ch.kind.short_badge(), fd.path),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" ({}) ", fd.language),
                    Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(30, 42, 58))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{} ━━", grp_badge),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);

            continuous.lines.push(banner);
            continuous.line_to_file.push(Some(change_idx));
            continuous.line_to_source_line.push(None);

            let body_start_y = continuous.lines.len();
            let raw_ansi_lines: Vec<&str> = fd.ansi_output.lines().collect();
            let parsed_lines = &self.parsed_texts[change_idx].lines;

            for (local_y, line) in parsed_lines.iter().enumerate() {
                let src_line = raw_ansi_lines
                    .get(local_y)
                    .and_then(|raw| parse_source_line_from_ansi(raw));
                continuous.lines.push(line.clone());
                continuous.line_to_file.push(Some(change_idx));
                continuous.line_to_source_line.push(src_line);
            }

            for hm in &fd.hunks_meta {
                let global_y = (body_start_y + hm.local_anchor_line)
                    .min(continuous.lines.len().saturating_sub(1));
                continuous.global_hunks.push(GlobalHunkAnchor {
                    change_idx,
                    path: fd.path.clone(),
                    global_y,
                    chunk_index: hm.chunk_index,
                    start_line: hm.start_line,
                    end_line: hm.end_line,
                });
            }

            // Spacer line between file sections
            continuous.lines.push(Line::raw(""));
            continuous.line_to_file.push(Some(change_idx));
            continuous.line_to_source_line.push(None);

            let end_y = continuous.lines.len();
            continuous.sections.push(FileSectionSpan {
                change_idx,
                path: fd.path.clone(),
                group_title,
                start_y,
                body_start_y,
                end_y,
            });
        }

        // Map every global line y to its active GlobalHunkAnchor index in O(L + H)
        continuous.line_to_hunk = vec![None; continuous.lines.len()];
        let mut hunks_by_change: HashMap<usize, Vec<usize>> = HashMap::new();
        for (hi, h) in continuous.global_hunks.iter().enumerate() {
            hunks_by_change.entry(h.change_idx).or_default().push(hi);
        }
        for sec in &continuous.sections {
            let Some(hunks) = hunks_by_change.get(&sec.change_idx) else {
                continue;
            };
            let mut k = 0usize;
            for y in sec.start_y..sec.end_y.min(continuous.lines.len()) {
                while k + 1 < hunks.len() && continuous.global_hunks[hunks[k + 1]].global_y <= y {
                    k += 1;
                }
                continuous.line_to_hunk[y] = hunks.get(k).copied();
            }
        }

        self.continuous = continuous;
        if self.continuous.lines.is_empty() {
            self.diff_cursor_y = 0;
            self.diff_scroll_y = 0;
            return;
        }

        if let (Some(cidx), Some(rel_off)) = (prev_file_idx, prev_offset_in_file) {
            if let Some(sec) = self.continuous.sections.iter().find(|s| s.change_idx == cidx) {
                let max_in_sec = sec.end_y.saturating_sub(1);
                self.diff_cursor_y = (sec.start_y + rel_off).min(max_in_sec);
            }
        }

        let max_line = self.continuous.lines.len().saturating_sub(1);
        self.diff_cursor_y = self.diff_cursor_y.min(max_line);
        self.diff_scroll_y = self.diff_scroll_y.min(max_line);
        self.ensure_cursor_visible();
    }

    pub fn current_file(&self) -> Option<&FileDiffRendered> {
        self.selected_file_idx
            .and_then(|i| self.rendered_diffs.get(i))
    }

    fn retain_review_active_paths(&self, review: &mut AiReviewResult) {
        for g in &mut review.groups {
            g.files.retain(|p| self.path_to_index.contains_key(p));
        }
        review.groups.retain(|g| !g.files.is_empty());
        review
            .files
            .retain(|f| self.path_to_index.contains_key(&f.path));
        review
            .hunks
            .retain(|h| self.path_to_index.contains_key(&h.file));
        review
            .findings
            .retain(|f| self.path_to_index.contains_key(&f.file));
    }

    pub fn active_group_in_tree(&self) -> Option<&ReviewGroup> {
        let ai = self.ai_review.as_ref()?;
        match self.tree_rows.get(self.tree_cursor)? {
            TreeRow::GroupHeader { group_id, .. } => ai.groups.iter().find(|g| &g.id == group_id),
            TreeRow::FileItem {
                path, in_group, ..
            } => {
                if *in_group {
                    for row in self.tree_rows[..self.tree_cursor].iter().rev() {
                        if let TreeRow::GroupHeader { group_id, .. } = row {
                            if let Some(grp) = ai.groups.iter().find(|g| &g.id == group_id) {
                                return Some(grp);
                            }
                        }
                    }
                }
                ai.groups.iter().find(|g| g.files.contains(path))
            }
        }
    }

    pub fn active_hunk_context(
        &self,
    ) -> (
        Option<&FileDiffRendered>,
        Option<&GlobalHunkAnchor>,
        Option<u32>,
        Option<&HunkAnalysis>,
        Vec<&ReviewFinding>,
    ) {
        let y = self
            .diff_cursor_y
            .min(self.continuous.lines.len().saturating_sub(1));
        let file_idx = self
            .continuous
            .line_to_file
            .get(y)
            .copied()
            .flatten()
            .or(self.selected_file_idx);

        let fd = file_idx.and_then(|i| self.rendered_diffs.get(i));
        let hunk = self
            .continuous
            .line_to_hunk
            .get(y)
            .copied()
            .flatten()
            .and_then(|hi| self.continuous.global_hunks.get(hi));
        let source_line = self.continuous.line_to_source_line.get(y).copied().flatten();

        let (hunk_analysis, findings) = if let (Some(ai), Some(fd)) = (&self.ai_review, fd) {
            let matched_hunk = match_hunk_analysis(&ai.hunks, &fd.path, hunk, source_line);
            let mut file_findings: Vec<&ReviewFinding> = ai
                .findings
                .iter()
                .filter(|f| f.file == fd.path)
                .collect();

            // Prioritize findings closest to active source_line / hunk range
            if let Some(target_ln) = source_line.or_else(|| hunk.and_then(|h| h.start_line)) {
                file_findings.sort_by_key(|f| {
                    f.line
                        .map(|fl| fl.abs_diff(target_ln))
                        .unwrap_or(u32::MAX / 2)
                });
            }
            (matched_hunk, file_findings)
        } else {
            (None, Vec::new())
        };

        (fd, hunk, source_line, hunk_analysis, findings)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if self.overlay != OverlayKind::None {
            self.handle_overlay_key(key);
            return;
        }

        match self.input_mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Command | InputMode::Search | InputMode::AddNote | InputMode::AskAi => {
                self.handle_input_mode_key(key);
            }
        }
    }

    pub fn handle_paste(&mut self, raw: &str) {
        let sanitized: String = raw
            .chars()
            .map(|c| match c {
                '\r' | '\n' | '\t' => ' ',
                _ if c.is_control() => ' ',
                _ => c,
            })
            .collect();
        if sanitized.is_empty() {
            return;
        }
        if self.overlay == OverlayKind::CommitConfirm && self.commit_editing_msg {
            crate::ui::text_input::insert_clean_str(
                &mut self.commit_message,
                &mut self.commit_msg_cursor,
                &sanitized,
            );
            self.mark_commit_message_edited();
        } else if self.overlay == OverlayKind::None && self.input_mode != InputMode::Normal {
            crate::ui::text_input::insert_clean_str(
                &mut self.input_buffer,
                &mut self.input_cursor,
                &sanitized,
            );
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        match self.overlay {
            OverlayKind::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                    self.overlay = OverlayKind::None;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll_y = self.help_scroll_y.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll_y = self.help_scroll_y.saturating_sub(1);
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.help_scroll_y = self.help_scroll_y.saturating_add(8);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.help_scroll_y = self.help_scroll_y.saturating_sub(8);
                }
                _ => {}
            },
            OverlayKind::CommitPicker => {
                let total = 1 + self.commits_cache.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlay = OverlayKind::None;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if self.commit_picker_cursor + 1 < total {
                            self.commit_picker_cursor += 1;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.commit_picker_cursor = self.commit_picker_cursor.saturating_sub(1);
                    }
                    KeyCode::Char('r') => {
                        self.spawn_commits_refresh(true);
                    }
                    KeyCode::Enter => {
                        let target = if self.commit_picker_cursor == 0 {
                            DiffTarget::Uncommitted
                        } else if let Some(ci) =
                            self.commits_cache.get(self.commit_picker_cursor - 1)
                        {
                            DiffTarget::Commit(ci.revision.clone())
                        } else {
                            return;
                        };
                        self.overlay = OverlayKind::None;
                        if let Err(e) = self.reload_target(target) {
                            self.status_message = rust_i18n::t!(
                                "status.target_load_failed",
                                locale = self.locale(),
                                err = e
                            )
                            .into_owned();
                        }
                    }
                    _ => {}
                }
            }
            OverlayKind::Settings => {
                self.handle_settings_key(key);
            }
            OverlayKind::AiPicker => {
                let total = self.available_providers.len() + 1;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlay = OverlayKind::None;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if self.ai_picker_cursor + 1 < total {
                            self.ai_picker_cursor += 1;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.ai_picker_cursor = self.ai_picker_cursor.saturating_sub(1);
                    }
                    KeyCode::Enter => {
                        self.overlay = OverlayKind::None;
                        self.ai_session_epoch += 1;
                        self.ai_followup_generation += 1;
                        self.commit_generation += 1;
                        self.clear_ai_runtime_flags();
                        if self.ai_picker_cursor < self.available_providers.len() {
                            let new_id =
                                self.available_providers[self.ai_picker_cursor].id.clone();
                            self.config.switch_provider(&new_id);
                            self.active_ai_provider_id = new_id;
                            self.trigger_ai_analysis(true);
                        } else {
                            self.active_ai_provider_id = "none".to_string();
                            self.resolved_ai_provider_id = None;
                            self.active_ai_session_id = None;
                            self.ai_generation += 1;
                            self.ai_stage1_completed_gen = 0;
                            self.ai_error = None;
                            self.status_message =
                                rust_i18n::t!("status.ai_turned_off", locale = self.locale())
                                    .into_owned();
                        }
                    }
                    _ => {}
                }
            }
            OverlayKind::CommitConfirm => {
                self.handle_commit_confirm_key(key);
            }
            OverlayKind::CommitDoubleConfirm => {
                self.handle_commit_double_confirm_key(key);
            }
            OverlayKind::RevertConfirm => {
                self.handle_revert_confirm_key(key);
            }
            OverlayKind::RevertDoubleConfirm => {
                self.handle_revert_double_confirm_key(key);
            }
            OverlayKind::None => {}
        }
    }

    pub fn open_settings(&mut self) {
        self.available_providers = ai::detect_available_providers(&self.config);
        self.settings_cursor = 0;
        self.settings_status = None;
        self.settings_ai_dirty = false;
        self.overlay = OverlayKind::Settings;
    }

    fn save_user_settings_with_status(&mut self) {
        match self
            .config
            .save_user_settings(self.config_path_override.as_deref())
        {
            Ok(path) => {
                let msg = rust_i18n::t!(
                    "status.settings_saved",
                    locale = self.locale(),
                    path = path.display()
                )
                .into_owned();
                self.settings_status = Some(msg.clone());
                self.status_message = msg;
            }
            Err(e) => {
                let msg = rust_i18n::t!(
                    "status.settings_save_failed",
                    locale = self.locale(),
                    err = e
                )
                .into_owned();
                self.settings_status = Some(msg.clone());
                self.status_message = msg;
            }
        }
    }

    fn close_settings(&mut self) {
        self.overlay = OverlayKind::None;
        if self.settings_ai_dirty {
            self.settings_ai_dirty = false;
            self.trigger_ai_analysis(false);
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) {
        const SETTINGS_ROWS: usize = 8;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_settings(),
            KeyCode::Char('s') => self.save_user_settings_with_status(),
            KeyCode::Char('j') | KeyCode::Down => {
                if self.settings_cursor + 1 < SETTINGS_ROWS {
                    self.settings_cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.settings_cursor = self.settings_cursor.saturating_sub(1);
            }
            KeyCode::Char('h')
            | KeyCode::Left
            | KeyCode::Char('l')
            | KeyCode::Right
            | KeyCode::Char(' ')
            | KeyCode::Enter => {
                let forward = !matches!(key.code, KeyCode::Char('h') | KeyCode::Left);
                self.settings_status = None;
                match self.settings_cursor {
                    0 => {
                        let new_loc = i18n::next_locale(&self.config.ui_language, forward);
                        self.config.ui_language = new_loc.clone();
                        self.config.ui_language_explicit = true;
                        let disp = i18n::display_name(&new_loc);
                        self.status_message = rust_i18n::t!(
                            "status.settings_ui_lang_hint",
                            locale = new_loc.as_str(),
                            lang = disp
                        )
                        .into_owned();
                    }
                    1 => {
                        let next_lang = i18n::next_locale(&self.config.review_language, forward);
                        self.config.review_language = next_lang;
                        self.settings_ai_dirty = true;
                    }
                    2 => {
                        if matches!(key.code, KeyCode::Enter) {
                            self.overlay = OverlayKind::AiPicker;
                            self.ai_picker_cursor = 0;
                            return;
                        }
                        let mut choices = vec!["auto".to_string()];
                        for p in &self.available_providers {
                            if !choices.iter().any(|c| c.eq_ignore_ascii_case(&p.id)) {
                                choices.push(p.id.clone());
                            }
                        }
                        choices.push("none".to_string());
                        let cur_idx = choices
                            .iter()
                            .position(|c| c.eq_ignore_ascii_case(&self.active_ai_provider_id))
                            .unwrap_or(0);
                        let next_idx = if forward {
                            (cur_idx + 1) % choices.len()
                        } else if cur_idx == 0 {
                            choices.len() - 1
                        } else {
                            cur_idx - 1
                        };
                        let new_id = choices[next_idx].clone();
                        self.config.switch_provider(&new_id);
                        self.active_ai_provider_id = new_id;
                        self.settings_ai_dirty = true;
                    }
                    3 => {
                        let next = if forward {
                            self.display_mode.next()
                        } else {
                            match self.display_mode {
                                DifftDisplayMode::SideBySide => DifftDisplayMode::SideBySideShowBoth,
                                DifftDisplayMode::SideBySideShowBoth => DifftDisplayMode::Inline,
                                DifftDisplayMode::Inline => DifftDisplayMode::SideBySide,
                            }
                        };
                        self.display_mode = next;
                        self.config.display_mode = next;
                        self.re_render_all_for_mode_or_width();
                    }
                    4 => {
                        let new_val = !self.config.ignore_comments;
                        self.config.ignore_comments = new_val;
                        match DifftEngine::new(&self.config.difft_bin, new_val) {
                            Ok(engine) => {
                                self.difft_engine = Arc::new(engine);
                                self.re_render_all_for_mode_or_width();
                            }
                            Err(e) => {
                                self.config.ignore_comments = !new_val;
                                self.settings_status = Some(format!("{}", e));
                            }
                        }
                    }
                    5 => {
                        self.config.cache_ai_results = !self.config.cache_ai_results;
                    }
                    6 => {
                        self.config.cache_history = !self.config.cache_history;
                    }
                    7 => {
                        self.config.reuse_session = !self.config.reuse_session;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn resolve_cached_externals(&mut self) -> Vec<String> {
        if !self.cached_external_prefixes.is_empty()
            || !matches!(self.vcs.kind(), crate::model::VcsKind::Svn)
        {
            self.cached_external_prefixes.clone()
        } else {
            let exts = self.vcs.external_prefixes();
            self.cached_external_prefixes = exts.clone();
            exts
        }
    }

    pub fn open_commit_confirm(&mut self) {
        if self.bundle.target != DiffTarget::Uncommitted {
            self.status_message =
                rust_i18n::t!("commit.only_uncommitted", locale = self.locale()).into_owned();
            return;
        }
        if self.diff_loading {
            self.status_message =
                rust_i18n::t!("commit.loading_diff", locale = self.locale()).into_owned();
            return;
        }
        if self.bundle.changes.is_empty() {
            self.status_message =
                rust_i18n::t!("commit.no_changes", locale = self.locale()).into_owned();
            return;
        }

        let externals = self.resolve_cached_externals();
        self.commit_files = self
            .bundle
            .changes
            .iter()
            .map(|c| {
                let is_ext = externals
                    .iter()
                    .any(|e| c.path == *e || c.path.starts_with(&format!("{}/", e)));
                CommitFileEntry {
                    path: c.path.clone(),
                    badge: c.kind.short_badge().to_string(),
                    is_external: is_ext,
                    included: true,
                }
            })
            .collect();

        self.rebuild_commit_groups_from_files();
        self.commit_files_cursor = 0;
        self.commit_modal_focus = CommitModalFocus::Files;
        self.commit_editing_msg = false;
        self.commit_executing = false;
        self.commit_error = None;
        self.commit_hint_notice = None;
        self.commit_msg_dirty = false;
        self.commit_msg_needs_regen = false;
        if self.resolved_ai_provider_id.is_none() {
            let fast_cfg = self.config.fast_config();
            self.resolved_ai_provider_id =
                ai::resolve_provider(&self.active_ai_provider_id, &fast_cfg)
                    .map(|p| p.id().to_string());
        }

        let selected_paths: Vec<String> = self
            .commit_files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.path.clone())
            .collect();
        self.commit_message = ai::build_fallback_commit_message(
            &self.bundle,
            &selected_paths,
            self.ai_review.as_ref(),
        );
        self.overlay = OverlayKind::CommitConfirm;
        self.regenerate_commit_message_with_cache(false);
    }

    pub fn rebuild_commit_groups_from_files(&mut self) {
        self.commit_groups.clear();
        let mut path_to_idx: HashMap<&str, usize> = HashMap::new();
        for (i, f) in self.commit_files.iter().enumerate() {
            path_to_idx.insert(f.path.as_str(), i);
        }
        let mut covered = HashSet::new();

        if let Some(ai) = &self.ai_review {
            let mut groups_sorted: Vec<&crate::model::ReviewGroup> = ai.groups.iter().collect();
            groups_sorted.sort_by_key(|g| g.review_order);
            for g in groups_sorted {
                let mut indices = Vec::new();
                for fp in &g.files {
                    if let Some(&idx) = path_to_idx.get(fp.as_str()) {
                        if covered.insert(idx) {
                            indices.push(idx);
                        }
                    }
                }
                if !indices.is_empty() {
                    self.commit_groups.push(CommitGroupSection {
                        id: g.id.clone(),
                        title: g.title.clone(),
                        file_indices: indices,
                        expanded: true,
                    });
                }
            }
        }

        let remaining: Vec<usize> = (0..self.commit_files.len())
            .filter(|i| !covered.contains(i))
            .collect();
        if !remaining.is_empty() {
            let title = if self.commit_groups.is_empty() {
                rust_i18n::t!("commit.group_all", locale = self.locale()).into_owned()
            } else {
                rust_i18n::t!("commit.group_other", locale = self.locale()).into_owned()
            };
            self.commit_groups.push(CommitGroupSection {
                id: "ungrouped".to_string(),
                title,
                file_indices: remaining,
                expanded: true,
            });
        }

        self.rebuild_commit_rows();
    }

    pub fn rebuild_commit_rows(&mut self) {
        self.commit_rows.clear();
        for (gi, g) in self.commit_groups.iter().enumerate() {
            self.commit_rows.push(CommitListRow::GroupHeader(gi));
            if g.expanded {
                for &fi in &g.file_indices {
                    self.commit_rows.push(CommitListRow::File(fi));
                }
            }
        }
        if self.commit_rows.is_empty() {
            self.commit_files_cursor = 0;
        } else {
            self.commit_files_cursor = self.commit_files_cursor.min(self.commit_rows.len() - 1);
        }
    }

    fn on_commit_selection_changed(&mut self) {
        self.commit_generation += 1;
        self.cancel_commit_msg_task();
        self.commit_msg_generating = false;
        self.pending_commit_regen_at = None;
        self.commit_hint_notice = None;
        let selected_paths: Vec<String> = self
            .commit_files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.path.clone())
            .collect();
        if selected_paths.is_empty() {
            self.commit_error =
                Some(rust_i18n::t!("commit.err_select_file", locale = self.locale()).into_owned());
            self.commit_msg_needs_regen = false;
            if !self.commit_msg_dirty {
                self.commit_message.clear();
            }
            return;
        }
        self.commit_error = None;
        if !self.commit_msg_dirty {
            let fast_cfg = self.config.fast_config();
            let cache_key = self.build_commit_cache_key(&selected_paths, &fast_cfg);
            if let Some(cached) = self.commit_msg_cache.get(&cache_key).cloned() {
                self.commit_msg_needs_regen = false;
                self.commit_progress_stage =
                    rust_i18n::t!("commit.cache_hit", locale = self.locale()).into_owned();
                self.commit_message = cached;
                self.commit_msg_cursor = self.commit_message.len();
                return;
            }
            self.commit_message = ai::build_fallback_commit_message(
                &self.bundle,
                &selected_paths,
                self.ai_review.as_ref(),
            );
            if ai::resolve_provider(&self.active_ai_provider_id, &fast_cfg).is_some() {
                self.commit_msg_needs_regen = true;
                // Debounce slightly so rapid Space toggles coalesce into one AI regeneration
                self.pending_commit_regen_at = Some(Instant::now() + Duration::from_millis(350));
            } else {
                self.commit_msg_needs_regen = false;
            }
        }
    }

    fn mark_commit_message_edited(&mut self) {
        self.commit_msg_dirty = true;
        self.commit_msg_needs_regen = false;
        self.pending_commit_regen_at = None;
        self.commit_hint_notice = None;
        if self.commit_msg_generating {
            self.commit_generation += 1;
            self.cancel_commit_msg_task();
            self.commit_msg_generating = false;
            self.commit_started_at = None;
        }
    }

    pub fn open_commit_double_confirm(&mut self) {
        if self.commit_executing {
            return;
        }
        let has_selected = self.commit_files.iter().any(|f| f.included);
        if !has_selected {
            self.commit_error =
                Some(rust_i18n::t!("commit.err_select_file", locale = self.locale()).into_owned());
            return;
        }
        if self.commit_message.trim().is_empty() {
            self.commit_error =
                Some(rust_i18n::t!("commit.err_empty_msg", locale = self.locale()).into_owned());
            return;
        }
        self.commit_error = None;
        self.commit_hint_notice = None;
        self.pending_commit_regen_at = None;
        if self.commit_msg_needs_regen && !self.commit_msg_dirty {
            self.regenerate_commit_message();
        }
        self.overlay = OverlayKind::CommitDoubleConfirm;
    }

    pub fn regenerate_commit_message(&mut self) {
        self.regenerate_commit_message_with_cache(false);
    }

    pub fn regenerate_commit_message_with_cache(&mut self, bypass_cache: bool) {
        self.pending_commit_regen_at = None;
        self.commit_hint_notice = None;
        let selected_paths: Vec<String> = self
            .commit_files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.path.clone())
            .collect();
        if selected_paths.is_empty() {
            self.commit_error =
                Some(rust_i18n::t!("commit.err_select_file", locale = self.locale()).into_owned());
            return;
        }
        let fast_cfg = self.config.fast_config();
        let cache_key = self.build_commit_cache_key(&selected_paths, &fast_cfg);
        if !bypass_cache {
            if let Some(cached) = self.commit_msg_cache.get(&cache_key).cloned() {
                self.commit_generation += 1;
                self.cancel_commit_msg_task();
                self.commit_msg_generating = false;
                self.commit_started_at = None;
                self.commit_msg_dirty = false;
                self.commit_msg_needs_regen = false;
                self.commit_error = None;
                self.commit_progress_stage =
                    rust_i18n::t!("commit.cache_hit", locale = self.locale()).into_owned();
                self.commit_stream_logs.clear();
                self.commit_stream_thinking.clear();
                self.commit_stream_text.clear();
                self.commit_message = cached;
                self.commit_msg_cursor = self.commit_message.len();
                return;
            }
        }

        let Some(provider) = ai::resolve_provider(&self.active_ai_provider_id, &fast_cfg) else {
            self.commit_msg_generating = false;
            self.commit_msg_needs_regen = false;
            self.commit_message = ai::build_fallback_commit_message(
                &self.bundle,
                &selected_paths,
                self.ai_review.as_ref(),
            );
            return;
        };
        self.resolved_ai_provider_id = Some(provider.id().to_string());

        self.commit_generation += 1;
        let current_gen = self.commit_generation;
        self.cancel_commit_msg_task();
        let cancel_flag = Arc::clone(&self.commit_cancel_flag);
        self.commit_msg_generating = true;
        self.commit_started_at = Some(Instant::now());
        let model_label = fast_cfg
            .ai_model
            .as_deref()
            .unwrap_or_else(|| provider.id());
        self.commit_progress_stage = rust_i18n::t!(
            "commit.preparing",
            locale = self.locale(),
            model = model_label,
            count = selected_paths.len()
        )
        .into_owned();
        self.commit_stream_logs.clear();
        self.commit_stream_thinking.clear();
        self.commit_stream_text.clear();
        self.commit_msg_dirty = false;
        self.commit_msg_needs_regen = false;
        self.commit_error = None;

        let bundle = self.bundle.clone();
        let diffs = self.rendered_diffs.clone();
        let ai_review = self.ai_review.clone();
        let tx = self.bg_tx.clone();
        let tx_prog = tx.clone();

        thread::spawn(move || {
            let progress_cb: ai::AiProgressCallback = Arc::new(move |update| {
                let _ = tx_prog.send(BgMessage::CommitMsgProgress {
                    generation: current_gen,
                    update,
                });
            });
            let res = i18n::run_with_locale(&fast_cfg.ui_language, || {
                ai::run_with_ai_hooks(cancel_flag, Some(progress_cb), || {
                    ai::generate_commit_message(
                        provider.as_ref(),
                        &bundle,
                        &diffs,
                        &selected_paths,
                        ai_review.as_ref(),
                        &fast_cfg,
                    )
                })
            })
            .map_err(|e| format!("{:#}", e));
            let _ = tx.send(BgMessage::CommitMsgGenerated {
                generation: current_gen,
                cache_key,
                result: res,
            });
        });
    }

    fn build_commit_cache_key(&self, selected_paths: &[String], fast_cfg: &AppConfig) -> String {
        let mut sorted = selected_paths.to_vec();
        sorted.sort();
        format!(
            "{}|{}|{}|{}|{}",
            self.diff_generation,
            self.active_ai_provider_id,
            fast_cfg.ai_model.as_deref().unwrap_or(""),
            fast_cfg.ai_effort.as_deref().unwrap_or(""),
            sorted.join("\n")
        )
    }

    pub fn execute_commit_confirm(&mut self) {
        if self.commit_executing {
            return;
        }
        let paths: Vec<String> = self
            .commit_files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.path.clone())
            .collect();
        let locale = self.locale().to_string();
        if paths.is_empty() {
            self.commit_error =
                Some(rust_i18n::t!("commit.err_select_file", locale = locale.as_str()).into_owned());
            return;
        }
        if self.commit_message.trim().is_empty() {
            self.commit_error =
                Some(rust_i18n::t!("commit.err_empty_msg", locale = locale.as_str()).into_owned());
            return;
        }

        self.commit_executing = true;
        self.commit_error = None;
        let vcs = Arc::clone(&self.vcs);
        let msg = self.commit_message.clone();
        let tx = self.bg_tx.clone();

        thread::spawn(move || {
            let outcome = i18n::run_with_locale(&locale, || vcs.commit_files_detailed(&paths, &msg));
            let _ = tx.send(BgMessage::CommitDone(outcome));
        });
    }

    fn handle_commit_confirm_key(&mut self, key: KeyEvent) {
        if self.commit_executing {
            return;
        }

        // Ctrl+S advances to double confirmation anytime inside CommitConfirm modal
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.commit_editing_msg = false;
            self.open_commit_double_confirm();
            return;
        }

        if self.commit_editing_msg {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.commit_editing_msg = false;
                    if self.commit_msg_needs_regen
                        && !self.commit_msg_dirty
                        && self.pending_commit_regen_at.is_none()
                        && !self.commit_msg_generating
                    {
                        self.regenerate_commit_message();
                    }
                }
                KeyCode::Tab => {
                    self.commit_editing_msg = false;
                    self.commit_modal_focus = CommitModalFocus::Files;
                    if self.commit_msg_needs_regen
                        && !self.commit_msg_dirty
                        && self.pending_commit_regen_at.is_none()
                        && !self.commit_msg_generating
                    {
                        self.regenerate_commit_message();
                    }
                }
                _ => {
                    let prev_msg = self.commit_message.clone();
                    if crate::ui::text_input::handle_key(
                        &mut self.commit_message,
                        &mut self.commit_msg_cursor,
                        key,
                    ) && self.commit_message != prev_msg
                    {
                        self.mark_commit_message_edited();
                    }
                }
            }
            return;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.commit_generation += 1;
                self.pending_commit_regen_at = None;
                self.commit_msg_generating = false;
                self.commit_started_at = None;
                self.commit_hint_notice = None;
                self.cancel_commit_msg_task();
                self.overlay = OverlayKind::None;
            }
            KeyCode::Tab => {
                self.commit_modal_focus = match self.commit_modal_focus {
                    CommitModalFocus::Files => CommitModalFocus::Message,
                    CommitModalFocus::Message => CommitModalFocus::Files,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.commit_modal_focus == CommitModalFocus::Files
                    && self.commit_files_cursor + 1 < self.commit_rows.len()
                {
                    self.commit_files_cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.commit_modal_focus == CommitModalFocus::Files {
                    self.commit_files_cursor = self.commit_files_cursor.saturating_sub(1);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if self.commit_modal_focus == CommitModalFocus::Files {
                    match self.commit_rows.get(self.commit_files_cursor).copied() {
                        Some(CommitListRow::GroupHeader(gi)) => {
                            if self.commit_groups[gi].expanded {
                                self.commit_groups[gi].expanded = false;
                                self.rebuild_commit_rows();
                            }
                        }
                        Some(CommitListRow::File(fi)) => {
                            if let Some(gi) = self
                                .commit_groups
                                .iter()
                                .position(|g| g.file_indices.contains(&fi))
                            {
                                self.commit_groups[gi].expanded = false;
                                self.rebuild_commit_rows();
                                if let Some(pos) = self
                                    .commit_rows
                                    .iter()
                                    .position(|r| matches!(r, CommitListRow::GroupHeader(idx) if *idx == gi))
                                {
                                    self.commit_files_cursor = pos;
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if self.commit_modal_focus == CommitModalFocus::Files {
                    if let Some(CommitListRow::GroupHeader(gi)) =
                        self.commit_rows.get(self.commit_files_cursor).copied()
                    {
                        if !self.commit_groups[gi].expanded {
                            self.commit_groups[gi].expanded = true;
                            self.rebuild_commit_rows();
                        }
                    }
                }
            }
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                if self.commit_modal_focus == CommitModalFocus::Files {
                    match self.commit_rows.get(self.commit_files_cursor).copied() {
                        Some(CommitListRow::GroupHeader(gi)) => {
                            let all_included = self.commit_groups[gi]
                                .file_indices
                                .iter()
                                .all(|&idx| self.commit_files[idx].included);
                            let target = !all_included;
                            for &idx in &self.commit_groups[gi].file_indices {
                                self.commit_files[idx].included = target;
                            }
                            self.on_commit_selection_changed();
                        }
                        Some(CommitListRow::File(fi)) => {
                            if let Some(entry) = self.commit_files.get_mut(fi) {
                                entry.included = !entry.included;
                            }
                            self.on_commit_selection_changed();
                        }
                        None => {}
                    }
                }
            }
            KeyCode::Char('a') => {
                if self.commit_modal_focus == CommitModalFocus::Files && !self.commit_files.is_empty()
                {
                    let all_included = self.commit_files.iter().all(|f| f.included);
                    let target = !all_included;
                    for f in &mut self.commit_files {
                        f.included = target;
                    }
                    self.on_commit_selection_changed();
                }
            }
            KeyCode::Char('e') | KeyCode::Char('i') => {
                self.commit_modal_focus = CommitModalFocus::Message;
                self.commit_msg_cursor = self.commit_message.len();
                self.commit_editing_msg = true;
            }
            KeyCode::Char('g') => {
                self.commit_msg_dirty = false;
                self.regenerate_commit_message_with_cache(true);
            }
            KeyCode::Char('y') => {
                self.open_commit_double_confirm();
            }
            KeyCode::Enter => {
                if self.commit_modal_focus == CommitModalFocus::Message {
                    self.commit_msg_cursor = self.commit_message.len();
                    self.commit_editing_msg = true;
                } else {
                    self.open_commit_double_confirm();
                }
            }
            _ => {}
        }
    }

    fn handle_commit_double_confirm_key(&mut self, key: KeyEvent) {
        if self.commit_executing {
            return;
        }
        match key.code {
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                self.commit_error = None;
                self.commit_hint_notice = None;
                self.overlay = OverlayKind::CommitConfirm;
            }
            KeyCode::Char('f') => {
                if !self.commit_msg_generating {
                    return;
                }
                self.commit_generation += 1;
                self.cancel_commit_msg_task();
                self.commit_msg_generating = false;
                self.commit_started_at = None;
                self.commit_hint_notice = None;
                self.execute_commit_confirm();
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                if self.commit_msg_generating {
                    self.commit_hint_notice =
                        Some(rust_i18n::t!("commit.wait_hint", locale = self.locale()).into_owned());
                    return;
                }
                self.commit_hint_notice = None;
                self.execute_commit_confirm();
            }
            _ => {}
        }
    }

    pub fn open_revert_confirm(&mut self) {
        if self.bundle.target != DiffTarget::Uncommitted {
            self.status_message =
                rust_i18n::t!("revert.only_uncommitted", locale = self.locale()).into_owned();
            return;
        }
        if self.diff_loading {
            self.status_message =
                rust_i18n::t!("revert.loading_diff", locale = self.locale()).into_owned();
            return;
        }
        if self.bundle.changes.is_empty() {
            self.status_message =
                rust_i18n::t!("revert.no_changes", locale = self.locale()).into_owned();
            return;
        }

        let (title, indices): (String, Vec<usize>) = match self.focus {
            PaneFocus::Tree => match self.tree_rows.get(self.tree_cursor).cloned() {
                Some(TreeRow::GroupHeader {
                    group_id, title, ..
                }) => {
                    let mut idxs = Vec::new();
                    let mut seen = HashSet::new();
                    if let Some(ai) = &self.ai_review {
                        if let Some(g) = ai.groups.iter().find(|g| g.id == group_id) {
                            for p in &g.files {
                                if let Some(&idx) = self.path_to_index.get(p) {
                                    if seen.insert(idx) {
                                        idxs.push(idx);
                                    }
                                }
                            }
                        }
                    }
                    let label = rust_i18n::t!(
                        "revert.target_group",
                        locale = self.locale(),
                        title = title
                    )
                    .into_owned();
                    (label, idxs)
                }
                Some(TreeRow::FileItem {
                    path, change_idx, ..
                }) => {
                    let label = rust_i18n::t!(
                        "revert.target_file",
                        locale = self.locale(),
                        path = path
                    )
                    .into_owned();
                    (label, vec![change_idx])
                }
                None => (String::new(), Vec::new()),
            },
            PaneFocus::Diff => {
                let y = self
                    .diff_cursor_y
                    .min(self.continuous.lines.len().saturating_sub(1));
                let idx = self
                    .continuous
                    .line_to_file
                    .get(y)
                    .copied()
                    .flatten()
                    .or(self.selected_file_idx);
                if let Some(cidx) = idx.filter(|&i| i < self.bundle.changes.len()) {
                    let label = rust_i18n::t!(
                        "revert.target_file",
                        locale = self.locale(),
                        path = self.bundle.changes[cidx].path
                    )
                    .into_owned();
                    (label, vec![cidx])
                } else {
                    (String::new(), Vec::new())
                }
            }
        };

        if indices.is_empty() {
            self.status_message =
                rust_i18n::t!("revert.no_target", locale = self.locale()).into_owned();
            return;
        }

        let externals = self.resolve_cached_externals();
        self.revert_title = title;
        self.revert_files = indices
            .into_iter()
            .filter_map(|idx| self.bundle.changes.get(idx))
            .map(|c| {
                let is_ext = externals
                    .iter()
                    .any(|e| c.path == *e || c.path.starts_with(&format!("{}/", e)));
                CommitFileEntry {
                    path: c.path.clone(),
                    badge: c.kind.short_badge().to_string(),
                    is_external: is_ext,
                    included: true,
                }
            })
            .collect();
        self.revert_files_cursor = 0;
        self.revert_executing = false;
        self.revert_error = None;
        self.overlay = OverlayKind::RevertConfirm;
    }

    fn execute_revert_confirm(&mut self) {
        if self.revert_executing {
            return;
        }
        let paths: Vec<String> = self
            .revert_files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.path.clone())
            .collect();
        let locale = self.locale().to_string();
        if paths.is_empty() {
            self.revert_error =
                Some(rust_i18n::t!("revert.err_keep_file", locale = locale.as_str()).into_owned());
            return;
        }
        self.revert_executing = true;
        self.revert_error = None;
        let vcs = Arc::clone(&self.vcs);
        let tx = self.bg_tx.clone();
        thread::spawn(move || {
            let outcome = i18n::run_with_locale(&locale, || vcs.revert_files_detailed(&paths));
            let _ = tx.send(BgMessage::RevertDone(outcome));
        });
    }

    fn handle_revert_confirm_key(&mut self, key: KeyEvent) {
        if self.revert_executing {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => {
                self.revert_error = None;
                self.overlay = OverlayKind::None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.revert_files_cursor + 1 < self.revert_files.len() {
                    self.revert_files_cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.revert_files_cursor = self.revert_files_cursor.saturating_sub(1);
            }
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                if let Some(entry) = self.revert_files.get_mut(self.revert_files_cursor) {
                    entry.included = !entry.included;
                    self.revert_error = None;
                }
            }
            KeyCode::Char('a') => {
                if !self.revert_files.is_empty() {
                    let all_included = self.revert_files.iter().all(|f| f.included);
                    let target = !all_included;
                    for f in &mut self.revert_files {
                        f.included = target;
                    }
                    self.revert_error = None;
                }
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                if !self.revert_files.iter().any(|f| f.included) {
                    self.revert_error = Some(
                        rust_i18n::t!("revert.err_keep_file", locale = self.locale()).into_owned(),
                    );
                    return;
                }
                self.revert_error = None;
                self.overlay = OverlayKind::RevertDoubleConfirm;
            }
            _ => {}
        }
    }

    fn handle_revert_double_confirm_key(&mut self, key: KeyEvent) {
        if self.revert_executing {
            return;
        }
        match key.code {
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                self.revert_error = None;
                self.overlay = OverlayKind::RevertConfirm;
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                self.execute_revert_confirm();
            }
            _ => {}
        }
    }

    fn handle_input_mode_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
                self.input_cursor = 0;
            }
            KeyCode::Enter => {
                let text = self.input_buffer.trim().to_string();
                let mode = self.input_mode;
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
                self.input_cursor = 0;
                match mode {
                    InputMode::Command => self.execute_ex_command(&text),
                    InputMode::Search => {
                        self.search_query = text;
                        self.recompute_search_matches();
                        self.jump_to_current_search_match();
                    }
                    InputMode::AddNote => {
                        if !text.is_empty() {
                            if let Some(fd) = self.current_file() {
                                let (_, _, src_ln, _, _) = self.active_hunk_context();
                                self.session.notes.push(UserNote {
                                    file: fd.path.clone(),
                                    line: src_ln,
                                    text,
                                    created_at: Local::now().format("%H:%M:%S").to_string(),
                                });
                                self.show_inspector = true;
                                self.status_message =
                                    rust_i18n::t!("status.note_added", locale = self.locale())
                                        .into_owned();
                            }
                        }
                    }
                    InputMode::AskAi => {
                        if !text.is_empty() {
                            self.run_ai_followup(text);
                        }
                    }
                    InputMode::Normal => {}
                }
            }
            _ => {
                crate::ui::text_input::handle_key(
                    &mut self.input_buffer,
                    &mut self.input_cursor,
                    key,
                );
            }
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        // Multi-key Vim sequences: g, ], [, z
        if let Some(prefix) = self.pending_key.take() {
            match (prefix, key.code) {
                ('g', KeyCode::Char('g')) => {
                    self.jump_top();
                    return;
                }
                (']', KeyCode::Char('c')) => {
                    self.jump_hunk(true);
                    return;
                }
                ('[', KeyCode::Char('c')) => {
                    self.jump_hunk(false);
                    return;
                }
                (']', KeyCode::Char('f')) => {
                    self.step_file(true);
                    return;
                }
                ('[', KeyCode::Char('f')) => {
                    self.step_file(false);
                    return;
                }
                (']', KeyCode::Char('g')) => {
                    self.step_group(true);
                    return;
                }
                ('[', KeyCode::Char('g')) => {
                    self.step_group(false);
                    return;
                }
                (']', KeyCode::Char('i')) => {
                    self.step_finding(true);
                    return;
                }
                ('[', KeyCode::Char('i')) => {
                    self.step_finding(false);
                    return;
                }
                ('z', KeyCode::Char('a')) => {
                    self.toggle_current_group();
                    return;
                }
                ('z', KeyCode::Char('M')) => {
                    if let Some(ai) = &self.ai_review {
                        for g in &ai.groups {
                            self.collapsed_groups.insert(g.id.clone());
                        }
                        self.rebuild_tree_rows();
                    }
                    return;
                }
                ('z', KeyCode::Char('R')) => {
                    self.collapsed_groups.clear();
                    self.rebuild_tree_rows();
                    return;
                }
                _ => {
                    self.status_message = rust_i18n::t!(
                        "status.unknown_key_seq",
                        locale = self.locale(),
                        prefix = prefix,
                        key = format!("{:?}", key.code)
                    )
                    .into_owned();
                    return;
                }
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    self.scroll_half_page(true);
                    return;
                }
                KeyCode::Char('u') => {
                    self.scroll_half_page(false);
                    return;
                }
                KeyCode::Char('f') => {
                    self.scroll_full_page(true);
                    return;
                }
                KeyCode::Char('b') => {
                    self.scroll_full_page(false);
                    return;
                }
                KeyCode::Char('w') => {
                    self.toggle_focus_pane();
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => {
                self.help_scroll_y = 0;
                self.overlay = OverlayKind::Help;
            }
            KeyCode::Char(':') => {
                self.input_mode = InputMode::Command;
                self.input_buffer.clear();
                self.input_cursor = 0;
            }
            KeyCode::Char('/') => {
                self.input_mode = InputMode::Search;
                self.input_buffer.clear();
                self.input_cursor = 0;
            }
            // n / N: next / previous diff hunk across continuous diff
            KeyCode::Char('n') => self.jump_hunk(true),
            KeyCode::Char('N') => self.jump_hunk(false),
            // ; / , : search match navigation
            KeyCode::Char(';') => self.jump_to_search_match(true),
            KeyCode::Char(',') => self.jump_to_search_match(false),
            KeyCode::Char('c') => {
                if self.current_file().is_none() {
                    self.status_message =
                        rust_i18n::t!("status.no_file_to_note", locale = self.locale()).into_owned();
                } else {
                    self.input_mode = InputMode::AddNote;
                    self.input_buffer.clear();
                    self.input_cursor = 0;
                }
            }
            KeyCode::Char('A') => {
                self.input_mode = InputMode::AskAi;
                self.input_buffer.clear();
                self.input_cursor = 0;
            }
            KeyCode::Char('J') => {
                self.show_inspector = true;
                self.inspector_scroll_y = self.inspector_scroll_y.saturating_add(2);
            }
            KeyCode::Char('K') => {
                self.show_inspector = true;
                self.inspector_scroll_y = self.inspector_scroll_y.saturating_sub(2);
            }
            KeyCode::Char('a') => {
                self.user_forced_flat_tree = false;
                self.trigger_ai_analysis(true);
            }
            KeyCode::Char('O') => {
                self.open_settings();
            }
            KeyCode::Char('P') => {
                self.available_providers = ai::detect_available_providers(&self.config);
                self.overlay = OverlayKind::AiPicker;
                self.ai_picker_cursor = 0;
            }
            KeyCode::Char('C') => {
                self.open_commit_picker();
            }
            KeyCode::Char('R') => {
                let target = self.bundle.target.clone();
                if let Err(e) = self.reload_target_with_options(target, true) {
                    self.status_message = rust_i18n::t!(
                        "status.refresh_failed",
                        locale = self.locale(),
                        err = e
                    )
                    .into_owned();
                }
                self.spawn_commits_refresh(true);
            }
            KeyCode::Char('d') => {
                self.display_mode = self.display_mode.next();
                self.re_render_all_for_mode_or_width();
                self.status_message = rust_i18n::t!(
                    "status.difft_mode",
                    locale = self.locale(),
                    mode = self.display_mode.label()
                )
                .into_owned();
            }
            KeyCode::Char('t') => {
                self.tree_mode = match self.tree_mode {
                    TreeMode::AiGrouped => {
                        self.user_forced_flat_tree = true;
                        TreeMode::FlatFiles
                    }
                    TreeMode::FlatFiles if self.ai_review.is_some() => {
                        self.user_forced_flat_tree = false;
                        TreeMode::AiGrouped
                    }
                    TreeMode::FlatFiles => {
                        self.status_message =
                            rust_i18n::t!("status.ai_groups_not_ready", locale = self.locale())
                                .into_owned();
                        TreeMode::FlatFiles
                    }
                };
                self.rebuild_tree_and_continuous();
                self.sync_tree_to_diff_cursor();
            }
            KeyCode::Char('B') => {
                self.show_sidebar = !self.show_sidebar;
                if !self.show_sidebar {
                    self.focus = PaneFocus::Diff;
                }
            }
            KeyCode::Char('I') => {
                self.show_inspector = !self.show_inspector;
            }
            KeyCode::Char('S') => {
                self.open_commit_confirm();
            }
            KeyCode::Char('U') => {
                self.open_revert_confirm();
            }
            KeyCode::Char('e') => {
                let path = self.bundle.repo_root.join("codeduck-review.md");
                match export_review_markdown_with_locale(
                    &path,
                    &self.bundle,
                    self.ai_review.as_ref(),
                    &self.session,
                    self.locale(),
                ) {
                    Ok(()) => {
                        self.status_message = rust_i18n::t!(
                            "status.exported_to",
                            locale = self.locale(),
                            path = path.display()
                        )
                        .into_owned();
                    }
                    Err(e) => {
                        self.status_message = rust_i18n::t!(
                            "status.export_failed",
                            locale = self.locale(),
                            err = e
                        )
                        .into_owned();
                    }
                }
            }
            KeyCode::Tab => self.toggle_focus_pane(),
            KeyCode::Char('H') => {
                if self.show_sidebar {
                    self.inspector_scroll_y = 0;
                    self.focus = PaneFocus::Tree;
                }
            }
            KeyCode::Char('L') => {
                self.inspector_scroll_y = 0;
                self.focus = PaneFocus::Diff;
            }
            KeyCode::Char('g') => self.pending_key = Some('g'),
            KeyCode::Char(']') => self.pending_key = Some(']'),
            KeyCode::Char('[') => self.pending_key = Some('['),
            KeyCode::Char('z') => self.pending_key = Some('z'),
            KeyCode::Char('G') => self.jump_bottom(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(1),
            KeyCode::Char('h') | KeyCode::Left => self.move_left(),
            KeyCode::Char('l') | KeyCode::Right => self.move_right(),
            KeyCode::Char('0') => self.diff_scroll_x = 0,
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_selection(),
            _ => {}
        }
    }

    fn toggle_focus_pane(&mut self) {
        self.inspector_scroll_y = 0;
        if !self.show_sidebar {
            self.focus = PaneFocus::Diff;
        } else {
            self.focus = self.focus.toggle();
        }
    }

    fn move_down(&mut self, amount: usize) {
        self.inspector_scroll_y = 0;
        match self.focus {
            PaneFocus::Tree => {
                if !self.tree_rows.is_empty() {
                    self.tree_cursor = (self.tree_cursor + amount).min(self.tree_rows.len() - 1);
                    self.sync_diff_to_tree_selection();
                }
            }
            PaneFocus::Diff => {
                if !self.continuous.lines.is_empty() {
                    self.diff_cursor_y =
                        (self.diff_cursor_y + amount).min(self.continuous.lines.len() - 1);
                    self.ensure_cursor_visible();
                    self.sync_tree_to_diff_cursor();
                }
            }
        }
    }

    fn move_up(&mut self, amount: usize) {
        self.inspector_scroll_y = 0;
        match self.focus {
            PaneFocus::Tree => {
                self.tree_cursor = self.tree_cursor.saturating_sub(amount);
                self.sync_diff_to_tree_selection();
            }
            PaneFocus::Diff => {
                self.diff_cursor_y = self.diff_cursor_y.saturating_sub(amount);
                self.ensure_cursor_visible();
                self.sync_tree_to_diff_cursor();
            }
        }
    }

    fn move_left(&mut self) {
        match self.focus {
            PaneFocus::Diff => {
                if self.diff_scroll_x > 0 {
                    self.diff_scroll_x = self.diff_scroll_x.saturating_sub(4);
                } else if self.show_sidebar {
                    self.inspector_scroll_y = 0;
                    self.focus = PaneFocus::Tree;
                }
            }
            PaneFocus::Tree => {
                self.collapse_active_tree_group();
            }
        }
    }

    fn collapse_active_tree_group(&mut self) {
        match self.tree_rows.get(self.tree_cursor).cloned() {
            Some(TreeRow::GroupHeader {
                group_id,
                expanded: true,
                ..
            }) => {
                self.collapsed_groups.insert(group_id);
                self.rebuild_tree_rows();
            }
            Some(TreeRow::FileItem { in_group: true, .. }) => {
                if let Some((pos, gid)) = self.tree_rows[..self.tree_cursor]
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(i, r)| match r {
                        TreeRow::GroupHeader { group_id, .. } => Some((i, group_id.clone())),
                        _ => None,
                    })
                {
                    self.collapsed_groups.insert(gid);
                    self.rebuild_tree_rows();
                    self.tree_cursor = pos.min(self.tree_rows.len().saturating_sub(1));
                }
            }
            _ => {}
        }
    }

    fn move_right(&mut self) {
        match self.focus {
            PaneFocus::Diff => {
                self.diff_scroll_x = self.diff_scroll_x.saturating_add(4);
            }
            PaneFocus::Tree => {
                if let Some(TreeRow::GroupHeader {
                    group_id,
                    expanded: false,
                    ..
                }) = self.tree_rows.get(self.tree_cursor)
                {
                    self.collapsed_groups.remove(group_id);
                    self.rebuild_tree_rows();
                } else {
                    self.focus = PaneFocus::Diff;
                }
            }
        }
    }

    fn scroll_half_page(&mut self, down: bool) {
        let step = (self.diff_pane_height / 2).max(5) as usize;
        if down {
            self.move_down(step);
        } else {
            self.move_up(step);
        }
    }

    fn scroll_full_page(&mut self, down: bool) {
        let step = self.diff_pane_height.saturating_sub(2).max(10) as usize;
        if down {
            self.move_down(step);
        } else {
            self.move_up(step);
        }
    }

    fn jump_top(&mut self) {
        self.inspector_scroll_y = 0;
        match self.focus {
            PaneFocus::Tree => {
                self.tree_cursor = 0;
                self.sync_diff_to_tree_selection();
            }
            PaneFocus::Diff => {
                self.diff_cursor_y = 0;
                self.diff_scroll_y = 0;
                self.sync_tree_to_diff_cursor();
            }
        }
    }

    fn jump_bottom(&mut self) {
        self.inspector_scroll_y = 0;
        match self.focus {
            PaneFocus::Tree => {
                if !self.tree_rows.is_empty() {
                    self.tree_cursor = self.tree_rows.len() - 1;
                    self.sync_diff_to_tree_selection();
                }
            }
            PaneFocus::Diff => {
                if !self.continuous.lines.is_empty() {
                    self.diff_cursor_y = self.continuous.lines.len() - 1;
                    self.ensure_cursor_visible();
                    self.sync_tree_to_diff_cursor();
                }
            }
        }
    }

    fn activate_selection(&mut self) {
        match self.focus {
            PaneFocus::Tree => {
                if let Some(row) = self.tree_rows.get(self.tree_cursor).cloned() {
                    match row {
                        TreeRow::GroupHeader { group_id, .. } => {
                            if !self.collapsed_groups.remove(&group_id) {
                                self.collapsed_groups.insert(group_id);
                            }
                            self.rebuild_tree_rows();
                        }
                        TreeRow::FileItem { change_idx, .. } => {
                            self.jump_continuous_to_file(change_idx);
                            self.focus = PaneFocus::Diff;
                        }
                    }
                }
            }
            PaneFocus::Diff => {
                self.jump_hunk(true);
            }
        }
    }

    fn sync_diff_to_tree_selection(&mut self) {
        let target_idx = match self.tree_rows.get(self.tree_cursor) {
            Some(TreeRow::FileItem { change_idx, .. }) => Some(*change_idx),
            Some(TreeRow::GroupHeader {
                first_change_idx, ..
            }) => *first_change_idx,
            None => None,
        };
        if let Some(idx) = target_idx {
            self.jump_continuous_to_file(idx);
        }
    }

    fn jump_continuous_to_file(&mut self, change_idx: usize) {
        self.selected_file_idx = Some(change_idx);
        if let Some(sec) = self
            .continuous
            .sections
            .iter()
            .find(|s| s.change_idx == change_idx)
        {
            self.diff_cursor_y = sec.start_y;
            self.diff_scroll_y = sec.start_y;
            self.diff_scroll_x = 0;
        }
    }

    fn sync_tree_to_diff_cursor(&mut self) {
        let Some(cidx) = self
            .continuous
            .line_to_file
            .get(self.diff_cursor_y)
            .copied()
            .flatten()
        else {
            return;
        };
        self.selected_file_idx = Some(cidx);
        if let Some(pos) = self.tree_rows.iter().position(|r| {
            matches!(r, TreeRow::FileItem { change_idx, .. } if *change_idx == cidx)
        }) {
            self.tree_cursor = pos;
        } else if let (Some(ai), Some(ch)) = (&self.ai_review, self.bundle.changes.get(cidx)) {
            if let Some(grp) = ai.groups.iter().find(|g| g.files.contains(&ch.path)) {
                if let Some(pos) = self.tree_rows.iter().position(|r| {
                    matches!(r, TreeRow::GroupHeader { group_id, .. } if group_id == &grp.id)
                }) {
                    self.tree_cursor = pos;
                }
            }
        }
    }

    fn ensure_cursor_visible(&mut self) {
        let h = self.diff_pane_height.max(1) as usize;
        let scroll = self.diff_scroll_y;
        let margin = 3.min(h / 4);

        if self.diff_cursor_y < scroll + margin {
            self.diff_scroll_y = self.diff_cursor_y.saturating_sub(margin);
        } else if self.diff_cursor_y + margin >= scroll + h {
            self.diff_scroll_y = (self.diff_cursor_y + margin + 1).saturating_sub(h);
        }
    }

    fn toggle_current_group(&mut self) {
        match self.tree_rows.get(self.tree_cursor).cloned() {
            Some(TreeRow::GroupHeader { group_id, .. }) => {
                if !self.collapsed_groups.remove(&group_id) {
                    self.collapsed_groups.insert(group_id);
                }
                self.rebuild_tree_rows();
            }
            Some(TreeRow::FileItem { .. }) => {
                self.collapse_active_tree_group();
            }
            None => {}
        }
    }

    pub fn add_ignore_pattern(&mut self, pattern: &str) {
        let matcher = IgnoreMatcher::load(self.vcs.repo_root(), self.vcs.scope_dir());
        match matcher.append_pattern(pattern) {
            Ok(_) => {
                let updated = IgnoreMatcher::load(self.vcs.repo_root(), self.vcs.scope_dir());
                let base_dir = self.bundle.path_base_dir.clone();
                let before_len = self.bundle.changes.len();

                let mut new_changes = Vec::with_capacity(before_len);
                let mut new_rendered = Vec::with_capacity(before_len);
                let mut new_texts = Vec::with_capacity(before_len);
                let mut new_path_map = HashMap::new();
                let mut removed_paths = HashSet::new();

                let old_changes = std::mem::take(&mut self.bundle.changes);
                let old_rendered = std::mem::take(&mut self.rendered_diffs);
                let old_texts = std::mem::take(&mut self.parsed_texts);

                for ((ch, rend), txt) in old_changes
                    .into_iter()
                    .zip(old_rendered.into_iter())
                    .zip(old_texts.into_iter())
                {
                    if updated.is_ignored(&base_dir, &ch.path) {
                        removed_paths.insert(ch.path);
                    } else {
                        let new_idx = new_changes.len();
                        new_path_map.insert(ch.path.clone(), new_idx);
                        new_changes.push(ch);
                        new_rendered.push(rend);
                        new_texts.push(txt);
                    }
                }

                self.bundle.changes = new_changes;
                self.rendered_diffs = new_rendered;
                self.parsed_texts = new_texts;
                self.path_to_index = new_path_map;
                // Invalidate in-flight width/mode rerenders and cached commit messages for the old file set
                self.rerender_generation += 1;
                self.pending_rerender_at = None;
                self.commit_generation += 1;
                self.cancel_commit_msg_task();
                self.commit_msg_generating = false;
                self.pending_commit_regen_at = None;
                self.commit_msg_cache.clear();

                for p in &removed_paths {
                    self.session.file_analysis_map.remove(p);
                }
                if let Some(ai) = &mut self.ai_review {
                    for g in &mut ai.groups {
                        g.files.retain(|p| !removed_paths.contains(p));
                    }
                    ai.groups.retain(|g| !g.files.is_empty());
                    ai.files.retain(|f| !removed_paths.contains(&f.path));
                    ai.hunks.retain(|h| !removed_paths.contains(&h.file));
                    ai.findings.retain(|f| !removed_paths.contains(&f.file));
                }

                self.selected_file_idx = if self.rendered_diffs.is_empty() {
                    None
                } else {
                    Some(
                        self.selected_file_idx
                            .unwrap_or(0)
                            .min(self.rendered_diffs.len() - 1),
                    )
                };
                self.rebuild_tree_and_continuous();
                if let Some(idx) = self.selected_file_idx {
                    self.jump_continuous_to_file(idx);
                }
                self.status_message = rust_i18n::t!(
                    "status.ignore_added",
                    locale = self.locale(),
                    pattern = pattern,
                    file = updated.primary_file().display(),
                    count = removed_paths.len()
                )
                .into_owned();
            }
            Err(e) => {
                self.status_message = rust_i18n::t!(
                    "status.ignore_failed",
                    locale = self.locale(),
                    err = e
                )
                .into_owned();
            }
        }
    }

    pub fn jump_hunk(&mut self, forward: bool) {
        if self.continuous.global_hunks.is_empty() {
            return;
        }
        self.inspector_scroll_y = 0;
        let cur = self.diff_cursor_y;
        let target = if forward {
            self.continuous
                .global_hunks
                .iter()
                .find(|h| h.global_y > cur)
                .unwrap_or(&self.continuous.global_hunks[0])
        } else {
            self.continuous
                .global_hunks
                .iter()
                .rev()
                .find(|h| h.global_y < cur)
                .unwrap_or(self.continuous.global_hunks.last().unwrap())
        };

        self.diff_cursor_y = target.global_y;
        let center_offset = (self.diff_pane_height / 4) as usize;
        self.diff_scroll_y = self.diff_cursor_y.saturating_sub(center_offset);
        self.sync_tree_to_diff_cursor();
    }

    fn step_file(&mut self, forward: bool) {
        if self.continuous.sections.is_empty() {
            return;
        }
        self.inspector_scroll_y = 0;
        let cur = self.selected_file_idx.unwrap_or(0);
        let pos = self
            .continuous
            .sections
            .iter()
            .position(|s| s.change_idx == cur)
            .unwrap_or(0);
        let next_pos = if forward {
            (pos + 1) % self.continuous.sections.len()
        } else if pos == 0 {
            self.continuous.sections.len() - 1
        } else {
            pos - 1
        };
        let next_idx = self.continuous.sections[next_pos].change_idx;
        self.jump_continuous_to_file(next_idx);
        self.sync_tree_to_diff_cursor();
    }

    fn step_group(&mut self, forward: bool) {
        let group_positions: Vec<usize> = self
            .tree_rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| matches!(r, TreeRow::GroupHeader { .. }).then_some(i))
            .collect();
        if group_positions.is_empty() {
            return;
        }
        self.inspector_scroll_y = 0;
        let next_pos = if forward {
            group_positions
                .iter()
                .copied()
                .find(|&p| p > self.tree_cursor)
                .unwrap_or(group_positions[0])
        } else {
            group_positions
                .iter()
                .copied()
                .rev()
                .find(|&p| p < self.tree_cursor)
                .unwrap_or(*group_positions.last().unwrap())
        };
        self.tree_cursor = next_pos;
        self.sync_diff_to_tree_selection();
    }

    fn step_finding(&mut self, forward: bool) {
        let Some(ai) = &self.ai_review else {
            return;
        };
        if ai.findings.is_empty() {
            return;
        }
        self.inspector_scroll_y = 0;
        let cur_y = self.diff_cursor_y;
        // Find target y for each finding in continuous diff, preserving deterministic order for same-line items
        let mut finding_targets: Vec<(usize, u32, usize, &ReviewFinding)> = ai
            .findings
            .iter()
            .enumerate()
            .filter_map(|(ord, f)| {
                let cidx = *self.path_to_index.get(&f.file)?;
                let sec = self
                    .continuous
                    .sections
                    .iter()
                    .find(|s| s.change_idx == cidx)?;
                let y = if let Some(ln) = f.line {
                    (sec.body_start_y..sec.end_y)
                        .find(|&gy| self.continuous.line_to_source_line[gy] == Some(ln))
                        .unwrap_or(sec.start_y)
                } else {
                    sec.start_y
                };
                Some((y, f.line.unwrap_or(0), ord, f))
            })
            .collect();
        finding_targets.sort_by_key(|(y, ln, ord, _)| (*y, *ln, *ord));
        if finding_targets.is_empty() {
            return;
        }

        let current_match_pos = self
            .last_stepped_finding_id
            .as_deref()
            .and_then(|last_id| {
                finding_targets
                    .iter()
                    .position(|(y, _, _, f)| *y == cur_y && f.id == last_id)
            });

        let chosen = if let Some(pos) = current_match_pos {
            let next_pos = if forward {
                (pos + 1) % finding_targets.len()
            } else if pos == 0 {
                finding_targets.len() - 1
            } else {
                pos - 1
            };
            &finding_targets[next_pos]
        } else if forward {
            finding_targets
                .iter()
                .find(|(y, _, _, _)| *y > cur_y)
                .unwrap_or(&finding_targets[0])
        } else {
            finding_targets
                .iter()
                .rev()
                .find(|(y, _, _, _)| *y < cur_y)
                .unwrap_or(finding_targets.last().unwrap())
        };

        let target_y = chosen.0;
        let finding_id = chosen.3.id.clone();
        let finding_title = chosen.3.title.clone();
        self.last_stepped_finding_id = Some(finding_id);
        self.diff_cursor_y = target_y;
        self.diff_scroll_y = self
            .diff_cursor_y
            .saturating_sub((self.diff_pane_height / 4) as usize);
        self.sync_tree_to_diff_cursor();
        self.status_message = rust_i18n::t!(
            "status.finding_status",
            locale = self.locale(),
            title = finding_title
        )
        .into_owned();
    }

    fn recompute_search_matches(&mut self) {
        self.search_matches.clear();
        self.search_match_cursor = 0;
        if self.search_query.is_empty() {
            return;
        }
        let q = self.search_query.to_ascii_lowercase();
        for (i, line) in self.continuous.lines.iter().enumerate() {
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if plain.to_ascii_lowercase().contains(&q) {
                self.search_matches.push(i);
            }
        }
        if let Some(pos) = self
            .search_matches
            .iter()
            .position(|&y| y >= self.diff_cursor_y)
        {
            self.search_match_cursor = pos;
        }
    }

    fn jump_to_current_search_match(&mut self) {
        if self.search_matches.is_empty() {
            if !self.search_query.is_empty() {
                self.status_message = rust_i18n::t!(
                    "status.search_not_found",
                    locale = self.locale(),
                    query = self.search_query
                )
                .into_owned();
            }
            return;
        }
        self.inspector_scroll_y = 0;
        let line = self.search_matches[self.search_match_cursor];
        self.diff_cursor_y = line;
        self.diff_scroll_y = line.saturating_sub(3);
        self.sync_tree_to_diff_cursor();
        self.status_message = rust_i18n::t!(
            "status.search_match",
            locale = self.locale(),
            idx = self.search_match_cursor + 1,
            total = self.search_matches.len(),
            query = self.search_query
        )
        .into_owned();
    }

    fn jump_to_search_match(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            if !self.search_query.is_empty() {
                self.status_message = rust_i18n::t!(
                    "status.search_not_found",
                    locale = self.locale(),
                    query = self.search_query
                )
                .into_owned();
            }
            return;
        }
        if forward {
            self.search_match_cursor =
                (self.search_match_cursor + 1) % self.search_matches.len();
        } else if self.search_match_cursor == 0 {
            self.search_match_cursor = self.search_matches.len() - 1;
        } else {
            self.search_match_cursor -= 1;
        }
        self.jump_to_current_search_match();
    }

    fn spawn_commits_refresh(&mut self, force_refresh: bool) {
        if self.commits_loading {
            if force_refresh {
                self.pending_commits_force_refresh = true;
            }
            return;
        }
        self.commits_loading = true;
        let vcs = Arc::clone(&self.vcs);
        let use_cache = self.config.cache_history;
        let tx = self.bg_tx.clone();
        thread::spawn(move || {
            let res = crate::vcs::recent_commits_cached(&*vcs, 30, use_cache, force_refresh)
                .map_err(|e| format!("{:#}", e));
            let _ = tx.send(BgMessage::CommitsLoaded(res));
        });
    }

    fn open_commit_picker(&mut self) {
        self.overlay = OverlayKind::CommitPicker;
        self.commit_picker_cursor = 0;
        if self.commits_cache.is_empty() && self.config.cache_history {
            if let Some(cached) = crate::vcs::load_history_disk_cache(&*self.vcs, 30) {
                self.commits_cache = cached;
            }
        }
        self.spawn_commits_refresh(false);
    }

    fn run_ai_followup(&mut self, question: String) {
        let Some(provider) = ai::resolve_provider(&self.active_ai_provider_id, &self.config) else {
            self.status_message =
                rust_i18n::t!("status.ai_no_provider", locale = self.locale()).into_owned();
            return;
        };
        let bundle = self.bundle.clone();
        let file_diff = self.current_file().cloned();
        let mut cfg = self.config.clone();
        if self.ai_deep_running {
            cfg.reuse_session = false;
        }
        let provider_id = provider.id().to_string();
        let tx = self.bg_tx.clone();
        self.cancel_followup_task();
        self.ai_followup_generation += 1;
        let followup_gen = self.ai_followup_generation;
        let diff_gen = self.diff_generation;
        let epoch = self.ai_session_epoch;
        let cancel_flag = Arc::clone(&self.ai_followup_cancel_flag);
        self.ai_followup_running = true;
        self.status_message = rust_i18n::t!(
            "status.ai_asking",
            locale = self.locale(),
            provider = provider.display_name()
        )
        .into_owned();

        thread::spawn(move || {
            let res = ai::run_with_cancel_flag(cancel_flag, || {
                ai::ask_followup(&*provider, &bundle, file_diff.as_ref(), &question, &cfg)
            })
            .map_err(|e| format!("{:#}", e));
            let _ = tx.send(BgMessage::AiFollowupDone {
                generation: followup_gen,
                diff_generation: diff_gen,
                session_epoch: epoch,
                provider_id,
                question,
                file_path: file_diff.map(|f| f.path),
                answer: res,
            });
        });
    }

    fn execute_ex_command(&mut self, raw: &str) {
        let parts: Vec<&str> = raw.split_whitespace().collect();
        let Some(&cmd) = parts.first() else {
            return;
        };
        match cmd {
            "q" | "quit" | "wq" => self.should_quit = true,
            "help" | "h" => {
                self.help_scroll_y = 0;
                self.overlay = OverlayKind::Help;
            }
            "uncommitted" | "wc" => {
                if let Err(e) = self.reload_target(DiffTarget::Uncommitted) {
                    self.status_message =
                        rust_i18n::t!("status.error_prefix", locale = self.locale(), err = e)
                            .into_owned();
                }
            }
            "rev" | "commit" | "c" => {
                if let Some(rev) = parts.get(1) {
                    let target = DiffTarget::parse(rev);
                    if let Err(e) = self.reload_target(target) {
                        self.status_message =
                            rust_i18n::t!("status.error_prefix", locale = self.locale(), err = e)
                                .into_owned();
                    }
                } else {
                    self.open_commit_picker();
                }
            }
            "commits" | "log" => self.open_commit_picker(),
            "ci" | "submit" => self.open_commit_confirm(),
            "revert" | "undo" => self.open_revert_confirm(),
            "config" | "settings" | "options" => self.open_settings(),
            "lang" | "language" => {
                if let Some(arg) = parts.get(1) {
                    if let Some(loc) = i18n::resolve_locale(arg) {
                        let prev_review = self.config.review_language.clone();
                        self.config.ui_language = loc.clone();
                        self.config.ui_language_explicit = true;
                        self.config.review_language = loc.clone();
                        let disp = i18n::display_name(&loc);
                        self.status_message = rust_i18n::t!(
                            "status.lang_both_updated",
                            locale = loc.as_str(),
                            lang = disp
                        )
                        .into_owned();
                        if prev_review != self.config.review_language {
                            self.trigger_ai_analysis(false);
                        }
                    } else {
                        self.config.review_language = (*arg).to_string();
                        self.status_message = rust_i18n::t!(
                            "status.lang_review_updated",
                            locale = self.locale(),
                            lang = arg
                        )
                        .into_owned();
                        self.trigger_ai_analysis(false);
                    }
                } else {
                    self.open_settings();
                }
            }
            "ui-lang" => {
                if let Some(arg) = parts.get(1) {
                    if let Some(loc) = i18n::resolve_locale(arg) {
                        self.config.ui_language = loc.clone();
                        self.config.ui_language_explicit = true;
                        let disp = i18n::display_name(&loc);
                        self.status_message = rust_i18n::t!(
                            "status.ui_lang_updated",
                            locale = loc.as_str(),
                            lang = disp
                        )
                        .into_owned();
                    } else {
                        let avail = i18n::available_summary();
                        self.status_message = rust_i18n::t!(
                            "status.ui_lang_unsupported",
                            locale = self.locale(),
                            lang = arg,
                            available = avail
                        )
                        .into_owned();
                    }
                } else {
                    let cur = self.config.ui_language.clone();
                    let avail = i18n::available_summary();
                    self.status_message = rust_i18n::t!(
                        "status.ui_lang_current",
                        locale = self.locale(),
                        lang = cur,
                        available = avail
                    )
                    .into_owned();
                }
            }
            "ai" => {
                if let Some(p) = parts.get(1) {
                    self.ai_session_epoch += 1;
                    self.ai_generation += 1;
                    self.ai_stage1_completed_gen = 0;
                    self.ai_followup_generation += 1;
                    self.commit_generation += 1;
                    self.clear_ai_runtime_flags();
                    self.config.switch_provider(p);
                    self.active_ai_provider_id = self.config.ai_provider.clone();
                    self.user_forced_flat_tree = false;
                    self.trigger_ai_analysis(true);
                } else {
                    self.available_providers = ai::detect_available_providers(&self.config);
                    self.ai_picker_cursor = 0;
                    self.overlay = OverlayKind::AiPicker;
                }
            }
            "new-session" | "reset-session" => {
                self.reset_ai_session();
            }
            "model" => {
                if let Some(m) = parts.get(1) {
                    self.config.ai_model = crate::config::normalize_optional_setting(m);
                    self.user_forced_flat_tree = false;
                    self.trigger_ai_analysis(true);
                } else {
                    let cur = self.config.ai_model.as_deref().unwrap_or("<default>");
                    self.status_message = rust_i18n::t!(
                        "status.model_current",
                        locale = self.locale(),
                        cur = cur
                    )
                    .into_owned();
                }
            }
            "fast-model" => {
                if let Some(fm) = parts.get(1) {
                    self.config.ai_fast_model = crate::config::normalize_optional_setting(fm);
                    self.commit_msg_cache.clear();
                    self.user_forced_flat_tree = false;
                    self.trigger_ai_analysis(true);
                } else {
                    let cur = self.config.ai_fast_model.as_deref().unwrap_or("<default>");
                    self.status_message = rust_i18n::t!(
                        "status.fast_model_current",
                        locale = self.locale(),
                        cur = cur
                    )
                    .into_owned();
                }
            }
            "effort" => {
                if let Some(e) = parts.get(1) {
                    match crate::config::normalize_optional_setting(e) {
                        None => {
                            self.config.ai_effort = None;
                            self.user_forced_flat_tree = false;
                            self.trigger_ai_analysis(true);
                        }
                        Some(v) => match crate::config::validate_effort(&v) {
                            Ok(valid) => {
                                self.config.ai_effort = Some(valid);
                                self.user_forced_flat_tree = false;
                                self.trigger_ai_analysis(true);
                            }
                            Err(err) => {
                                self.status_message = rust_i18n::t!(
                                    "status.error_prefix",
                                    locale = self.locale(),
                                    err = err
                                )
                                .into_owned();
                            }
                        },
                    }
                } else {
                    let cur = self.config.ai_effort.as_deref().unwrap_or("<default>");
                    self.status_message = rust_i18n::t!(
                        "status.effort_current",
                        locale = self.locale(),
                        cur = cur
                    )
                    .into_owned();
                }
            }
            "fast-effort" => {
                if let Some(fe) = parts.get(1) {
                    match crate::config::normalize_optional_setting(fe) {
                        None => {
                            self.config.ai_fast_effort = None;
                            self.commit_msg_cache.clear();
                            self.user_forced_flat_tree = false;
                            self.trigger_ai_analysis(true);
                        }
                        Some(v) => match crate::config::validate_effort(&v) {
                            Ok(valid) => {
                                self.config.ai_fast_effort = Some(valid);
                                self.commit_msg_cache.clear();
                                self.user_forced_flat_tree = false;
                                self.trigger_ai_analysis(true);
                            }
                            Err(err) => {
                                self.status_message = rust_i18n::t!(
                                    "status.error_prefix",
                                    locale = self.locale(),
                                    err = err
                                )
                                .into_owned();
                            }
                        },
                    }
                } else {
                    let cur = self.config.ai_fast_effort.as_deref().unwrap_or("<default>");
                    self.status_message = rust_i18n::t!(
                        "status.fast_effort_current",
                        locale = self.locale(),
                        cur = cur
                    )
                    .into_owned();
                }
            }
            "ask" => {
                let q = parts[1..].join(" ");
                if !q.is_empty() {
                    self.run_ai_followup(q);
                }
            }
            "ignore" => {
                let pat = parts[1..].join(" ");
                if pat.trim().is_empty() {
                    self.status_message =
                        rust_i18n::t!("status.ignore_usage", locale = self.locale()).into_owned();
                } else {
                    self.add_ignore_pattern(pat.trim());
                }
            }
            "export" => {
                let file = parts.get(1).copied().unwrap_or("codeduck-review.md");
                let path = PathBuf::from(file);
                let out = if path.is_absolute() {
                    path
                } else {
                    self.bundle.repo_root.join(path)
                };
                match export_review_markdown_with_locale(
                    &out,
                    &self.bundle,
                    self.ai_review.as_ref(),
                    &self.session,
                    self.locale(),
                ) {
                    Ok(()) => {
                        self.status_message = rust_i18n::t!(
                            "status.exported_to",
                            locale = self.locale(),
                            path = out.display()
                        )
                        .into_owned();
                    }
                    Err(e) => {
                        self.status_message = rust_i18n::t!(
                            "status.export_failed",
                            locale = self.locale(),
                            err = e
                        )
                        .into_owned();
                    }
                }
            }
            "inline" => {
                self.display_mode = DifftDisplayMode::Inline;
                self.re_render_all_for_mode_or_width();
            }
            "sbs" | "side-by-side" => {
                self.display_mode = DifftDisplayMode::SideBySide;
                self.re_render_all_for_mode_or_width();
            }
            "tree" => {
                self.show_sidebar = !self.show_sidebar;
                if !self.show_sidebar {
                    self.focus = PaneFocus::Diff;
                }
            }
            "inspector" => self.show_inspector = !self.show_inspector,
            "refresh" | "r" => {
                let t = self.bundle.target.clone();
                if let Err(e) = self.reload_target_with_options(t, true) {
                    self.status_message = rust_i18n::t!(
                        "status.refresh_failed",
                        locale = self.locale(),
                        err = e
                    )
                    .into_owned();
                }
                self.spawn_commits_refresh(true);
            }
            _ => {
                self.status_message = rust_i18n::t!(
                    "status.unknown_command",
                    locale = self.locale(),
                    cmd = raw
                )
                .into_owned();
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.ai_cancel_flag.store(true, Ordering::Relaxed);
        self.ai_followup_cancel_flag.store(true, Ordering::Relaxed);
        self.commit_cancel_flag.store(true, Ordering::Relaxed);
    }
}

fn render_all_files(
    engine: &DifftEngine,
    bundle: &DiffBundle,
    mode: DifftDisplayMode,
    width: u16,
) -> anyhow::Result<(
    Vec<FileDiffRendered>,
    Vec<Text<'static>>,
    HashMap<String, usize>,
)> {
    let rendered_diffs = engine.render_changes_bounded(&bundle.changes, mode, width)?;
    let mut parsed_texts = Vec::with_capacity(rendered_diffs.len());
    let mut path_to_index = HashMap::with_capacity(rendered_diffs.len());
    for (i, r) in rendered_diffs.iter().enumerate() {
        path_to_index.insert(r.path.clone(), i);
        parsed_texts.push(ansi_to_ratatui_text(&r.ansi_output));
    }

    Ok((rendered_diffs, parsed_texts, path_to_index))
}

fn match_hunk_analysis<'a>(
    hunks: &'a [HunkAnalysis],
    file_path: &str,
    active_hunk: Option<&GlobalHunkAnchor>,
    source_line: Option<u32>,
) -> Option<&'a HunkAnalysis> {
    let candidates: Vec<&HunkAnalysis> = hunks.iter().filter(|h| h.file == file_path).collect();
    if candidates.is_empty() {
        return None;
    }

    if let Some(ln) = source_line {
        if let Some(exact) = candidates.iter().copied().find(|h| {
            matches!((h.start_line, h.end_line), (Some(s), Some(e)) if ln >= s && ln <= e)
        }) {
            return Some(exact);
        }
    }

    if let Some(ah) = active_hunk {
        if let Some(by_idx) = candidates
            .iter()
            .copied()
            .find(|h| h.chunk_index == Some(ah.chunk_index))
        {
            return Some(by_idx);
        }
        if let Some(target_ln) = ah.start_line.or(source_line) {
            return candidates
                .into_iter()
                .min_by_key(|h| {
                    h.start_line
                        .map(|s| s.abs_diff(target_ln))
                        .unwrap_or(u32::MAX / 2)
                });
        }
    }

    candidates.into_iter().next()
}

pub fn ansi_to_ratatui_text(ansi: &str) -> Text<'static> {
    match ansi.as_bytes().into_text() {
        Ok(t) => t,
        Err(_) => Text::raw(ansi.to_string()),
    }
}

fn apply_progress_update(
    started_at: Option<Instant>,
    stage_buf: &mut String,
    logs: &mut Vec<String>,
    thinking: &mut String,
    text: &mut String,
    update: ai::AiProgressUpdate,
) -> Option<String> {
    let elapsed = started_at
        .map(|t| t.elapsed().as_secs_f32())
        .unwrap_or(0.0);
    match update {
        ai::AiProgressUpdate::Stage(stage) => {
            *stage_buf = stage.clone();
            logs.push(format!("[{:.1}s] {}", elapsed, stage));
            if logs.len() > 30 {
                let drain = logs.len() - 30;
                logs.drain(0..drain);
            }
            Some(stage)
        }
        ai::AiProgressUpdate::LogLine(line) => {
            logs.push(format!("[{:.1}s] {}", elapsed, line));
            if logs.len() > 30 {
                let drain = logs.len() - 30;
                logs.drain(0..drain);
            }
            None
        }
        ai::AiProgressUpdate::ThinkingDelta(delta) => {
            append_bounded_tail(thinking, &delta, 6000);
            None
        }
        ai::AiProgressUpdate::TextDelta(delta) => {
            append_bounded_tail(text, &delta, 12000);
            None
        }
    }
}

fn append_bounded_tail(buf: &mut String, delta: &str, max_bytes: usize) {
    buf.push_str(delta);
    if buf.len() > max_bytes {
        let mut cut = buf.len() - max_bytes;
        while cut < buf.len() && !buf.is_char_boundary(cut) {
            cut += 1;
        }
        buf.drain(..cut);
    }
}

fn truncate(s: &str, max_cols: usize) -> String {
    crate::ui::truncate_display_width(s, max_cols)
}
