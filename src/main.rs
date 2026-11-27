use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use codeduck::ai;
use codeduck::config::AppConfig;
use codeduck::difft::DifftEngine;
use codeduck::i18n;
use codeduck::ignore::IgnoreMatcher;
use codeduck::model::{DiffTarget, DifftDisplayMode, VcsKind};
use codeduck::ui;
use codeduck::ui::app::App;
use codeduck::ui::export::export_review_markdown_with_locale;
use codeduck::vcs;

#[derive(Parser, Debug)]
#[command(
    name = "codeduck",
    version,
    about = "Vim-style terminal code review powered by Difftastic and local AI CLI subscriptions"
)]
struct Cli {
    /// Repository or working copy path (defaults to current directory)
    #[arg(short = 'C', long, default_value = ".")]
    path: PathBuf,

    /// Force VCS backend (auto, svn, git)
    #[arg(long, default_value = "auto")]
    vcs: String,

    /// Review a specific commit/revision (e.g. 123 or r123 for SVN, commit SHA for Git)
    #[arg(short = 'r', short_alias = 'c', long)]
    rev: Option<String>,

    /// Review a revision range (e.g. 100:120 for SVN, main..HEAD for Git)
    #[arg(long)]
    range: Option<String>,

    /// Local AI CLI provider (auto, agy, codex, claude, gemini, none, or custom name)
    #[arg(long, alias = "provider")]
    ai: Option<String>,

    /// Model override passed to the local AI CLI
    #[arg(long)]
    model: Option<String>,

    /// Reasoning/thinking effort level (low, medium, high, xhigh, max)
    #[arg(long)]
    effort: Option<String>,

    /// Fast model override for Stage 1 grouping and commit message generation
    #[arg(long)]
    fast_model: Option<String>,

    /// Fast reasoning/thinking effort level for Stage 1 grouping and commit messages
    #[arg(long)]
    fast_effort: Option<String>,

    /// AI review output language (e.g. en, zh-CN)
    #[arg(long)]
    lang: Option<String>,

    /// UI display language (en, zh-CN)
    #[arg(long)]
    ui_lang: Option<String>,

    /// Difftastic display mode (side-by-side, side-by-side-show-both, inline)
    #[arg(long)]
    display: Option<String>,

    /// Path to global configs.toml (defaults to ~/.codeduck/configs.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Disable AI disk cache
    #[arg(long)]
    no_cache: bool,

    /// Start a fresh AI session instead of resuming saved session from ~/.codeduck/sessions.toml
    #[arg(long)]
    new_session: bool,

    /// Run AI analysis non-interactively and write Markdown report to stdout or file, without TUI
    #[arg(long)]
    headless_export: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = AppConfig::load(cli.config.as_deref())?;

    let preferred_vcs = match cli.vcs.to_ascii_lowercase().as_str() {
        "svn" => Some(VcsKind::Svn),
        "git" => Some(VcsKind::Git),
        _ => None,
    };
    let vcs_backend = vcs::open_vcs(&cli.path, preferred_vcs)?;

    // Merge project-level `.codeduck` TOML settings (global -> .codeduck -> CLI flags)
    config.apply_project_overrides(vcs_backend.repo_root(), vcs_backend.scope_dir());
    DifftEngine::check_available(&config.difft_bin)?;

    if let Some(ai_p) = cli.ai {
        config.switch_provider(&ai_p);
    }
    if let Some(m) = cli.model {
        config.ai_model = codeduck::config::normalize_optional_setting(&m);
    }
    if let Some(e) = cli.effort {
        config.ai_effort = match codeduck::config::normalize_optional_setting(&e) {
            Some(v) => Some(codeduck::config::validate_effort(&v)?),
            None => None,
        };
    }
    if let Some(fm) = cli.fast_model {
        config.ai_fast_model = codeduck::config::normalize_optional_setting(&fm);
    }
    if let Some(fe) = cli.fast_effort {
        config.ai_fast_effort = match codeduck::config::normalize_optional_setting(&fe) {
            Some(v) => Some(codeduck::config::validate_effort(&v)?),
            None => None,
        };
    }
    if let Some(l) = cli.lang {
        if !config.ui_language_explicit && cli.ui_lang.is_none() {
            if let Some(loc) = i18n::resolve_locale(&l) {
                config.ui_language = loc;
            }
        }
        config.review_language = l;
    }
    if let Some(ui_l) = cli.ui_lang {
        let loc = i18n::resolve_locale(&ui_l).ok_or_else(|| {
            anyhow::anyhow!(
                "Unsupported UI language `{}` (available: {})",
                ui_l,
                i18n::available_summary()
            )
        })?;
        config.ui_language = loc;
        config.ui_language_explicit = true;
    }
    if cli.no_cache {
        config.cache_ai_results = false;
        config.cache_history = false;
    }
    if cli.new_session {
        config.ai_sessions.clear();
        if !config.ai_provider.eq_ignore_ascii_case("none")
            && !config.ai_provider.eq_ignore_ascii_case("off")
        {
            if let Some(p) = ai::resolve_provider(&config.ai_provider, &config) {
                let _ = codeduck::config::save_project_session(
                    vcs_backend.repo_root(),
                    vcs_backend.scope_dir(),
                    p.id(),
                    None,
                );
            } else if !config.ai_provider.eq_ignore_ascii_case("auto") {
                let _ = codeduck::config::save_project_session(
                    vcs_backend.repo_root(),
                    vcs_backend.scope_dir(),
                    &config.ai_provider,
                    None,
                );
            }
        }
    }
    if let Some(disp) = cli.display {
        config.display_mode = match disp.as_str() {
            "inline" => DifftDisplayMode::Inline,
            "side-by-side-show-both" | "both" => DifftDisplayMode::SideBySideShowBoth,
            _ => DifftDisplayMode::SideBySide,
        };
    }

    let target = if let Some(range) = cli.range {
        DiffTarget::parse(&range)
    } else if let Some(rev) = cli.rev {
        DiffTarget::Commit(rev)
    } else {
        DiffTarget::Uncommitted
    };

    if let Some(export_path) = cli.headless_export {
        let matcher = IgnoreMatcher::load(vcs_backend.repo_root(), vcs_backend.scope_dir());
        let bundle = vcs::collect_changes_cached(
            vcs_backend.as_ref(),
            &target,
            Some(&matcher),
            config.cache_history,
            false,
        )?;
        let engine = DifftEngine::new(&config.difft_bin, config.ignore_comments)?;
        let need_json = !config.ai_provider.eq_ignore_ascii_case("none")
            && !config.ai_provider.eq_ignore_ascii_case("off");
        let rendered = engine.render_changes_bounded_with_options(
            &bundle.changes,
            config.display_mode,
            120,
            need_json,
        )?;
        let ai_res = if let Some(provider) = ai::resolve_provider(&config.ai_provider, &config) {
            let out = ai::analyze_diff_with_session(
                provider.as_ref(),
                &bundle,
                &rendered,
                &config,
                cli.new_session,
            )?;
            if config.reuse_session {
                if let Some(sid) = out.session_id.as_deref() {
                    if let Err(err) = codeduck::config::save_project_session(
                        vcs_backend.repo_root(),
                        vcs_backend.scope_dir(),
                        provider.id(),
                        Some(sid),
                    ) {
                        eprintln!("Warning: {}", err);
                    }
                }
            }
            Some(out.review)
        } else {
            None
        };
        let session = codeduck::model::ReviewSessionState::default();
        export_review_markdown_with_locale(
            &export_path,
            &bundle,
            ai_res.as_ref(),
            &session,
            &config.ui_language,
        )?;
        println!(
            "{}",
            i18n::format_written_report(&config.ui_language, &export_path.display().to_string())
        );
        return Ok(());
    }

    let (term_w, _) = crossterm::terminal::size().unwrap_or((140, 40));
    let mut app = App::new_async(vcs_backend, config, target, term_w)?;
    app.config_path_override = cli.config;
    if cli.new_session {
        app.bypass_next_ai_cache = true;
    }

    install_terminal_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let _terminal_guard = TerminalRestoreGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let run_result = run_tui_loop(&mut terminal, &mut app);
    let _ = terminal.show_cursor();
    drop(_terminal_guard);

    run_result
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn install_terminal_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut last_anim_draw = std::time::Instant::now();
    while !app.should_quit {
        let bg_updated = app.poll_background();
        let anim_tick = app.has_active_animation()
            && last_anim_draw.elapsed() >= std::time::Duration::from_millis(200);
        if bg_updated || app.dirty || anim_tick {
            terminal.draw(|f| ui::draw(f, app))?;
            app.dirty = false;
            last_anim_draw = std::time::Instant::now();
        }

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.handle_key(key);
                }
                Event::Paste(text) => {
                    app.handle_paste(&text);
                }
                Event::Resize(_, _) => {
                    app.dirty = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}
