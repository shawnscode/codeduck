pub mod app;
pub mod export;
pub mod text_input;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use std::time::Instant;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::i18n;
use crate::model::{RiskLevel, Severity};
use crate::ui::app::{
    App, CommitModalFocus, InputMode, OverlayKind, PaneFocus, TreeMode, TreeRow,
};
use rust_i18n::t;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let root = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Header bar
            Constraint::Min(10),   // Main workspace
            Constraint::Length(2), // Status + command bar
        ])
        .split(root);

    draw_header(frame, app, outer[0]);
    draw_workspace(frame, app, outer[1]);
    draw_footer(frame, app, outer[2]);

    match app.overlay {
        OverlayKind::None => {}
        OverlayKind::Help => draw_help_modal(frame, app, root),
        OverlayKind::Settings => draw_settings_modal(frame, app, root),
        OverlayKind::CommitPicker => draw_commit_picker(frame, app, root),
        OverlayKind::AiPicker => draw_ai_picker(frame, app, root),
        OverlayKind::CommitConfirm => draw_commit_confirm(frame, app, root),
        OverlayKind::CommitDoubleConfirm => {
            draw_commit_confirm(frame, app, root);
            draw_commit_double_confirm(frame, app, root);
        }
        OverlayKind::RevertConfirm => draw_revert_confirm(frame, app, root),
        OverlayKind::RevertDoubleConfirm => {
            draw_revert_confirm(frame, app, root);
            draw_revert_double_confirm(frame, app, root);
        }
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let total = app.bundle.changes.len();

    let (ai_badge, ai_deep_badge) = if app.ai_running {
        const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let elapsed = app.ai_started_at.map(|t| t.elapsed()).unwrap_or_default();
        let spin = SPINNER[((elapsed.as_millis() / 100) % 10) as usize];
        (
            Span::styled(
                t!(
                    "header.grouping",
                    locale = locale,
                    spin = spin,
                    secs = elapsed.as_secs()
                )
                .into_owned(),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        )
    } else if let Some(ai) = &app.ai_review {
        let short_prov = app
            .resolved_ai_provider_id
            .as_deref()
            .unwrap_or(&app.active_ai_provider_id);
        let deep = if app.ai_deep_running {
            const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let elapsed = app.ai_started_at.map(|t| t.elapsed()).unwrap_or_default();
            let spin = SPINNER[((elapsed.as_millis() / 100) % 10) as usize];
            Some(Span::styled(
                t!(
                    "header.risks",
                    locale = locale,
                    spin = spin,
                    secs = elapsed.as_secs()
                )
                .into_owned(),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ))
        } else {
            None
        };
        (
            Span::styled(
                format!(" {} [{}] ", short_prov, ai.overall_risk.localized_label(locale)),
                Style::default()
                    .fg(Color::Black)
                    .bg(risk_color(ai.overall_risk))
                    .add_modifier(Modifier::BOLD),
            ),
            deep,
        )
    } else if app.ai_error.is_some() {
        (
            Span::styled(
                t!("header.ai_failed", locale = locale).into_owned(),
                Style::default().fg(Color::White).bg(Color::Red),
            ),
            None,
        )
    } else {
        (
            Span::styled(
                format!(" {} ", app.active_ai_provider_id),
                Style::default().fg(Color::Gray).bg(Color::DarkGray),
            ),
            None,
        )
    };

    let commit_hint = app
        .bundle
        .commit_info
        .as_ref()
        .map(|ci| {
            let first_line = ci.message.lines().next().unwrap_or("");
            format!(" — {} ({})", first_line, ci.author)
        })
        .unwrap_or_default();

    let line1 = Line::from(vec![
        Span::styled(
            " codeduck ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[{}]", app.bundle.vcs),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            app.bundle.target.localized_label(locale),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(commit_hint, Style::default().fg(Color::Gray)),
    ]);

    let view_mode = match app.tree_mode {
        TreeMode::AiGrouped => t!("header.view_groups", locale = locale),
        TreeMode::FlatFiles => t!("header.view_files", locale = locale),
    };
    let mut line2_spans = vec![
        Span::styled(
            t!("header.files_count", locale = locale, count = total).into_owned(),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  |  "),
        Span::styled(
            t!(
                "header.difft_label",
                locale = locale,
                mode = app.display_mode.localized_label(locale)
            )
            .into_owned(),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  |  "),
        Span::styled(
            t!("header.view_label", locale = locale, mode = view_mode).into_owned(),
            Style::default().fg(Color::Blue),
        ),
        Span::raw("  |  "),
        ai_badge,
    ];
    if let Some(deep) = ai_deep_badge {
        line2_spans.push(Span::raw(" "));
        line2_spans.push(deep);
    }
    let ai_disabled = app.active_ai_provider_id.eq_ignore_ascii_case("none")
        || app.active_ai_provider_id.eq_ignore_ascii_case("off");
    if !ai_disabled {
        let shown_model = if app.ai_running {
            app.config
                .ai_fast_model
                .as_deref()
                .or(app.config.ai_model.as_deref())
        } else {
            app.config.ai_model.as_deref()
        };
        if let Some(m) = shown_model {
            line2_spans.push(Span::styled(
                t!("header.model_badge", locale = locale, model = m).into_owned(),
                Style::default().fg(Color::Magenta),
            ));
        }
        let shown_effort = if app.ai_running {
            app.config
                .ai_fast_effort
                .as_deref()
                .or_else(|| app.config.ai_fast_model.as_ref().map(|_| "low"))
                .or(app.config.ai_effort.as_deref())
        } else {
            app.config.ai_effort.as_deref()
        };
        if let Some(e) = shown_effort {
            line2_spans.push(Span::styled(
                t!("header.effort_badge", locale = locale, effort = e).into_owned(),
                Style::default().fg(Color::Yellow),
            ));
        }
        if app.config.reuse_session && !app.ai_running && !app.ai_deep_running {
            if let Some(sid) = app
                .active_ai_session_id
                .as_ref()
                .or_else(|| app.config.ai_sessions.get(&app.active_ai_provider_id))
            {
                let short: String = sid.chars().take(8).collect();
                line2_spans.push(Span::styled(
                    t!("header.session_badge", locale = locale, session = short).into_owned(),
                    Style::default().fg(Color::Green),
                ));
            }
        }
    }
    let line2 = Line::from(line2_spans);

    let header = Paragraph::new(vec![line1, line2]);
    frame.render_widget(header, area);
}

fn draw_workspace(frame: &mut Frame, app: &mut App, area: Rect) {
    let (tree_area, right_area) = if app.show_sidebar {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(area);
        (Some(cols[0]), cols[1])
    } else {
        (None, area)
    };

    if let Some(ta) = tree_area {
        draw_tree_pane(frame, app, ta);
    }

    if app.show_inspector {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
            .split(right_area);
        draw_diff_pane(frame, app, rows[0]);
        draw_passive_info_pane(frame, app, rows[1]);
    } else {
        draw_diff_pane(frame, app, right_area);
    }
}

fn draw_tree_pane(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let focused = app.focus == PaneFocus::Tree;
    let border_style = pane_border_style(focused);
    let max_desc_cols = area.width.saturating_sub(7).max(10) as usize;
    let max_path_cols = area.width.saturating_sub(10).max(12) as usize;
    let title = match app.tree_mode {
        TreeMode::AiGrouped => t!("tree.title_grouped", locale = locale).into_owned(),
        TreeMode::FlatFiles => t!("tree.title_flat", locale = locale).into_owned(),
    };

    let items: Vec<ListItem> = app
        .tree_rows
        .iter()
        .map(|row| match row {
            TreeRow::GroupHeader {
                title,
                description,
                category,
                risk_label,
                file_count,
                expanded,
                order,
                ..
            } => {
                let arrow = if *expanded { "▼" } else { "▶" };
                let line1 = Line::from(vec![
                    Span::styled(
                        format!("{} {}. ", arrow, order),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        title.clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("[{}|{}]", category, risk_label),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!(" ({})", file_count),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);

                if description.trim().is_empty() {
                    ListItem::new(vec![line1])
                } else {
                    let short_desc = truncate_display_width(description.trim(), max_desc_cols);
                    let line2 = Line::from(Span::styled(
                        format!("   {}", short_desc),
                        Style::default().fg(Color::Gray),
                    ));
                    ListItem::new(vec![line1, line2])
                }
            }
            TreeRow::FileItem {
                path,
                badge,
                finding_count,
                in_group,
                ..
            } => {
                let indent = if *in_group { "  " } else { "" };
                let badge_color = match badge.as_str() {
                    "A" => Color::Green,
                    "D" => Color::Red,
                    "M" | "R" => Color::Yellow,
                    _ => Color::Cyan,
                };
                let shown_path = truncate_path_middle(path, max_path_cols);
                let mut spans = vec![
                    Span::raw(indent),
                    Span::styled(
                        format!("[{}] ", badge),
                        Style::default()
                            .fg(badge_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(shown_path),
                ];
                if *finding_count > 0 {
                    spans.push(Span::styled(
                        format!(" ({}!)", finding_count),
                        Style::default()
                            .fg(Color::LightRed)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let total_rows = items.len();
    let mut state = ListState::default();
    if !app.tree_rows.is_empty() {
        state.select(Some(app.tree_cursor.min(app.tree_rows.len() - 1)));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .highlight_style(
            Style::default()
                .bg(if focused {
                    Color::Rgb(40, 58, 86)
                } else {
                    Color::Rgb(28, 34, 44)
                })
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, area, &mut state);
    if total_rows > area.height.saturating_sub(2) as usize {
        let mut sb_state = ScrollbarState::new(total_rows).position(app.tree_cursor);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb_state,
        );
    }
}

fn draw_diff_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == PaneFocus::Diff;
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    // Reserve 2 columns for leading cursor gutter ("▶ " / "  ") so full-width diffs are not clipped.
    app.ensure_diff_rendered_for_width(inner_w.saturating_sub(2), inner_h);
    let locale = app.locale();

    let (title, text) = if app.diff_loading {
        let target_label = app.bundle.target.localized_label(locale);
        (
            t!("diff.loading_title", locale = locale, target = target_label).into_owned(),
            Text::raw(
                t!(
                    "diff.loading_body",
                    locale = locale,
                    target = target_label,
                    vcs = app.bundle.vcs
                )
                .into_owned(),
            ),
        )
    } else if let Some(err) = &app.diff_error {
        (
            t!("diff.error_title", locale = locale).into_owned(),
            Text::raw(t!("diff.error_body", locale = locale, err = err).into_owned()),
        )
    } else if app.continuous.lines.is_empty() {
        (
            t!("diff.empty_title", locale = locale).into_owned(),
            Text::raw(t!("diff.empty_body", locale = locale).into_owned()),
        )
    } else {
        let active_path = app
            .current_file()
            .map(|f| f.path.as_str())
            .unwrap_or("");
        let hunk_total = app.continuous.global_hunks.len();
        let active_hunk_num = app
            .continuous
            .line_to_hunk
            .get(app.diff_cursor_y)
            .copied()
            .flatten()
            .map(|i| i + 1)
            .unwrap_or(0);
        let title = t!(
            "diff.pane_title",
            locale = locale,
            path = active_path,
            hunk = active_hunk_num,
            total = hunk_total
        )
        .into_owned();

        // Render visible slice with cursor row highlight when Diff pane is focused
        let start = app.diff_scroll_y.min(app.continuous.lines.len().saturating_sub(1));
        let visible_h = inner_h as usize;
        let end = (start + visible_h).min(app.continuous.lines.len());
        let mut visible_lines = Vec::with_capacity(end.saturating_sub(start));

        for y in start..end {
            let mut line = app.continuous.lines[y].clone();
            if y == app.diff_cursor_y {
                let cursor_bg = if focused {
                    Color::Rgb(38, 48, 66)
                } else {
                    Color::Rgb(28, 32, 42)
                };
                let mut spans = Vec::with_capacity(line.spans.len() + 1);
                spans.push(Span::styled(
                    "▶ ",
                    Style::default()
                        .fg(if focused { Color::Yellow } else { Color::DarkGray })
                        .bg(cursor_bg)
                        .add_modifier(Modifier::BOLD),
                ));
                for mut sp in line.spans {
                    sp.style = sp.style.bg(cursor_bg);
                    spans.push(sp);
                }
                line = Line::from(spans);
            } else {
                let mut spans = Vec::with_capacity(line.spans.len() + 1);
                spans.push(Span::raw("  "));
                spans.extend(line.spans);
                line = Line::from(spans);
            }
            visible_lines.push(line);
        }
        (title, Text::from(visible_lines))
    };

    // Since we already sliced `start..end` when rendering `continuous.lines`, vertical scroll is 0!
    let v_scroll = if app.diff_loading || app.diff_error.is_some() || app.continuous.lines.is_empty() {
        app.diff_scroll_y.min(u16::MAX as usize) as u16
    } else {
        0
    };

    let diff_widget = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(pane_border_style(focused))
                .title(title),
        )
        .scroll((v_scroll, app.diff_scroll_x));

    frame.render_widget(diff_widget, area);
    if !app.continuous.lines.is_empty()
        && app.continuous.lines.len() > area.height.saturating_sub(2) as usize
    {
        let mut sb_state =
            ScrollbarState::new(app.continuous.lines.len()).position(app.diff_cursor_y);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb_state,
        );
    }
}

fn push_multiline(lines: &mut Vec<Line<'static>>, prefix: &str, text: &str, style: Style) {
    let mut first = true;
    for l in text.lines() {
        let trimmed = l.trim_end();
        if first {
            lines.push(Line::from(Span::styled(
                format!("{}{}", prefix, trimmed),
                style,
            )));
            first = false;
        } else {
            lines.push(Line::from(Span::styled(
                format!("  {}", trimmed),
                style,
            )));
        }
    }
    if first {
        lines.push(Line::from(Span::styled(prefix.to_string(), style)));
    }
}

fn append_qa_and_notes(lines: &mut Vec<Line<'static>>, app: &App, file_path: Option<&str>) {
    let locale = app.locale();
    if let Some(fp) = file_path {
        let file_notes: Vec<_> = app.session.notes.iter().filter(|n| n.file == fp).collect();
        if !file_notes.is_empty() {
            lines.push(Line::raw(""));
            for n in file_notes {
                let prefix = t!("inspector.note_prefix", locale = locale, time = n.created_at);
                push_multiline(
                    lines,
                    &prefix,
                    &n.text,
                    Style::default().fg(Color::Magenta),
                );
            }
        }
    }
    if app.ai_followup_running {
        lines.push(Line::from(Span::styled(
            t!("inspector.querying_ai", locale = locale).into_owned(),
            Style::default().fg(Color::Yellow),
        )));
    } else if let Some(last_qa) = file_path
        .and_then(|fp| app.qa_history.iter().rev().find(|q| q.file.as_deref() == Some(fp)))
        .or_else(|| app.qa_history.last())
    {
        lines.push(Line::raw(""));
        let tag = last_qa
            .file
            .as_deref()
            .map(|p| format!(" [{}]", p))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            t!(
                "inspector.qa_question",
                locale = locale,
                tag = tag,
                question = last_qa.question
            )
            .into_owned(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        push_multiline(
            lines,
            &t!("inspector.qa_answer_prefix", locale = locale),
            &last_qa.answer,
            Style::default().fg(Color::Cyan),
        );
    }
}

fn draw_passive_info_pane(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let (base_title, lines) = match app.focus {
        PaneFocus::Tree => build_tree_context_info(app),
        PaneFocus::Diff => build_diff_context_info(app),
    };
    let title = if app.inspector_scroll_y > 0 {
        t!(
            "inspector.scroll_active",
            locale = locale,
            base = base_title.trim_end(),
            offset = app.inspector_scroll_y
        )
        .into_owned()
    } else {
        t!(
            "inspector.scroll_hint",
            locale = locale,
            base = base_title.trim_end()
        )
        .into_owned()
    };

    let widget = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(title),
        )
        .scroll((app.inspector_scroll_y, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(widget, area);
}

fn build_tree_context_info(app: &App) -> (String, Vec<Line<'static>>) {
    let locale = app.locale();
    let title = t!("inspector.group_details", locale = locale).into_owned();
    let mut lines: Vec<Line<'static>> = Vec::new();

    if app.ai_running {
        lines.extend(build_ai_progress_lines(app));
        append_qa_and_notes(&mut lines, app, app.current_file().map(|f| f.path.as_str()));
        return (title, lines);
    }

    if let Some(err) = &app.ai_error {
        lines.push(Line::from(Span::styled(
            t!("inspector.analysis_error", locale = locale, err = err).into_owned(),
            Style::default().fg(Color::Red),
        )));
        append_qa_and_notes(&mut lines, app, app.current_file().map(|f| f.path.as_str()));
        return (title, lines);
    }

    if let Some(grp) = app.active_group_in_tree() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("#{}. {} ", grp.review_order, grp.title),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("[{} | {}]", grp.category, grp.risk_level.localized_label(locale)),
                Style::default()
                    .fg(risk_color(grp.risk_level))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        if !grp.description.is_empty() {
            push_multiline(
                &mut lines,
                &t!("inspector.purpose_prefix", locale = locale),
                &grp.description,
                Style::default().fg(Color::Cyan),
            );
        }
        if !grp.key_points.is_empty() {
            lines.push(Line::from(Span::styled(
                t!("inspector.checkpoints_title", locale = locale).into_owned(),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            )));
            for kp in &grp.key_points {
                lines.push(Line::from(format!("  • {}", kp)));
            }
        }
    }

    // If cursor is on a FileItem, also show file-level summary
    if let Some(TreeRow::FileItem { path, .. }) = app.tree_rows.get(app.tree_cursor) {
        if let Some(fa) = app.session.file_analysis_map.get(path) {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "{} [{}|{}]: ",
                        fa.path,
                        fa.category,
                        fa.risk_level.localized_label(locale)
                    ),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(fa.summary.clone()),
            ]));
            for rf in &fa.review_focus {
                lines.push(Line::from(format!("  ◦ {}", rf)));
            }
        }
    }

    if lines.is_empty() {
        if let Some(ai) = &app.ai_review {
            push_multiline(
                &mut lines,
                &t!("inspector.summary_prefix", locale = locale),
                &ai.summary,
                Style::default().fg(Color::White),
            );
            if !ai.review_strategy.is_empty() {
                push_multiline(
                    &mut lines,
                    &t!("inspector.strategy_prefix", locale = locale),
                    &ai.review_strategy,
                    Style::default().fg(Color::Yellow),
                );
            }
        } else {
            lines.push(Line::from(t!("inspector.empty_tree", locale = locale).into_owned()));
        }
    }

    append_qa_and_notes(&mut lines, app, app.current_file().map(|f| f.path.as_str()));
    (title, lines)
}

fn build_diff_context_info(app: &App) -> (String, Vec<Line<'static>>) {
    let locale = app.locale();
    let (fd, hunk, source_line, hunk_analysis, findings) = app.active_hunk_context();
    let loc_label = match (fd, source_line, hunk) {
        (Some(f), Some(ln), _) => format!("{} : L{}", f.path, ln),
        (Some(f), None, Some(h)) if h.start_line.is_some() => {
            format!("{} : L{}", f.path, h.start_line.unwrap())
        }
        (Some(f), _, _) => f.path.clone(),
        (None, _, _) => t!("inspector.no_file", locale = locale).into_owned(),
    };
    let title = t!("inspector.code_inspector", locale = locale, loc = loc_label).into_owned();
    let mut lines: Vec<Line<'static>> = Vec::new();

    if app.ai_running
        || (app.ai_deep_running && hunk_analysis.is_none() && findings.is_empty())
    {
        lines.extend(build_ai_progress_lines(app));
        lines.push(Line::raw(""));
    }

    if let Some(ha) = hunk_analysis {
        let range_str = match (ha.start_line, ha.end_line) {
            (Some(s), Some(e)) if s != e => format!(" (L{}-L{})", s, e),
            (Some(s), _) => format!(" (L{})", s),
            _ => String::new(),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("[{}] ", ha.risk_level.localized_label(locale)),
                Style::default()
                    .fg(risk_color(ha.risk_level))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}{}", ha.title, range_str),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        if !ha.analysis.is_empty() {
            push_multiline(&mut lines, "", &ha.analysis, Style::default());
        }
    } else if let Some(f) = fd {
        if let Some(fa) = app.session.file_analysis_map.get(&f.path) {
            lines.push(Line::from(vec![
                Span::styled(
                    t!(
                        "inspector.file_badge",
                        locale = locale,
                        risk = fa.risk_level.localized_label(locale)
                    )
                    .into_owned(),
                    Style::default()
                        .fg(risk_color(fa.risk_level))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(fa.summary.clone()),
            ]));
            for rf in &fa.review_focus {
                lines.push(Line::from(format!("  • {}", rf)));
            }
        }
    }

    if !findings.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        for f in findings.iter().take(4) {
            let sev_color = match f.severity {
                Severity::Critical => Color::Red,
                Severity::Warning => Color::Yellow,
                Severity::Suggestion => Color::Cyan,
                Severity::Info => Color::Blue,
                Severity::Praise => Color::Green,
            };
            let line_tag = f.line.map(|l| format!("L{}: ", l)).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("[{}] ", f.severity.localized_label(locale)),
                    Style::default().fg(sev_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{}{}", line_tag, f.title),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            push_multiline(&mut lines, "  ", &f.description, Style::default());
            if let Some(sugg) = &f.suggestion {
                push_multiline(
                    &mut lines,
                    &t!("inspector.suggestion_prefix", locale = locale),
                    sugg,
                    Style::default().fg(Color::Green),
                );
            }
        }
    }

    append_qa_and_notes(&mut lines, app, fd.map(|f| f.path.as_str()));

    if lines.is_empty() {
        lines.push(Line::from(t!("inspector.empty_diff", locale = locale).into_owned()));
    }

    (title, lines)
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let mode_label = match app.input_mode {
        InputMode::Normal => (t!("mode.normal", locale = locale).into_owned(), Color::Green),
        InputMode::Command => (t!("mode.command", locale = locale).into_owned(), Color::Yellow),
        InputMode::Search => (t!("mode.search", locale = locale).into_owned(), Color::Cyan),
        InputMode::AddNote => (t!("mode.note", locale = locale).into_owned(), Color::Magenta),
        InputMode::AskAi => (t!("mode.ask_ai", locale = locale).into_owned(), Color::Blue),
    };

    let pending = app
        .pending_key
        .map(|k| format!(" [{}]", k))
        .unwrap_or_default();

    let status_line = Line::from(vec![
        Span::styled(
            mode_label.0,
            Style::default()
                .fg(Color::Black)
                .bg(mode_label.1)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" [{}]{} ", app.focus.label(locale), pending),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(&app.status_message),
    ]);

    let avail_w = area.width.saturating_sub(28) as usize;
    let edited = || text_input::format_with_cursor(&app.input_buffer, app.input_cursor, avail_w);
    let cmd_line = match app.input_mode {
        InputMode::Normal => Line::from(Span::styled(
            t!("footer.normal_hints", locale = locale).into_owned(),
            Style::default().fg(Color::DarkGray),
        )),
        InputMode::Command => Line::from(format!(":{}", edited())),
        InputMode::Search => Line::from(format!("/{}", edited())),
        InputMode::AddNote => Line::from(format!("{}{}", t!("footer.note_prompt", locale = locale), edited())),
        InputMode::AskAi => Line::from(format!("{}{}", t!("footer.ask_prompt", locale = locale), edited())),
    };

    let footer = Paragraph::new(vec![status_line, cmd_line]);
    frame.render_widget(footer, area);
}

fn draw_help_modal(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let popup = centered_rect(78, 84, area);
    frame.render_widget(Clear, popup);

    let mut help_text = vec![
        Line::from(Span::styled(
            t!("help.title", locale = locale).into_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            t!("help.window_title", locale = locale).into_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for l in t!("help.window_body", locale = locale).lines() {
        help_text.push(Line::from(l.to_string()));
    }
    help_text.push(Line::raw(""));
    help_text.push(Line::from(Span::styled(
        t!("help.nav_title", locale = locale).into_owned(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    for l in t!("help.nav_body", locale = locale).lines() {
        help_text.push(Line::from(l.to_string()));
    }
    help_text.push(Line::raw(""));
    help_text.push(Line::from(Span::styled(
        t!("help.ops_title", locale = locale).into_owned(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    for l in t!("help.ops_body", locale = locale).lines() {
        help_text.push(Line::from(l.to_string()));
    }
    help_text.push(Line::raw(""));
    help_text.push(Line::from(Span::styled(
        t!("help.close_hint", locale = locale).into_owned(),
        Style::default().fg(Color::DarkGray),
    )));

    let widget = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(t!("help.frame_title", locale = locale).into_owned()),
        )
        .scroll((app.help_scroll_y, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, popup);
}

fn draw_settings_modal(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let popup = centered_rect_sized(
        (area.width * 72 / 100).max(66.min(area.width)),
        17.min(area.height.saturating_sub(2)),
        area,
    );
    frame.render_widget(Clear, popup);

    let cfg_target = app
        .config_path_override
        .clone()
        .or_else(crate::config::AppConfig::config_path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.codeduck/configs.toml".to_string());

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(Color::Cyan))
        .title(t!("settings.title", locale = locale).into_owned());
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // Setting rows
            Constraint::Length(1), // Status / config file path
            Constraint::Length(1), // Action hints
        ])
        .split(inner);

    let on_off = |b: bool| -> String {
        if b {
            t!("settings.on", locale = locale).into_owned()
        } else {
            t!("settings.off", locale = locale).into_owned()
        }
    };

    let ui_lang_disp = i18n::display_name(&app.config.ui_language);
    let review_lang_disp = if i18n::resolve_locale(&app.config.review_language).is_some() {
        i18n::display_name(&app.config.review_language)
    } else {
        app.config.review_language.clone()
    };

    let rows_data: [(String, String); 8] = [
        (
            t!("settings.ui_language", locale = locale).into_owned(),
            format!("< {} >", ui_lang_disp),
        ),
        (
            t!("settings.ai_review_language", locale = locale).into_owned(),
            format!("< {} >", review_lang_disp),
        ),
        (
            t!("settings.ai_provider", locale = locale).into_owned(),
            format!(
                "< {} > ({})",
                app.active_ai_provider_id,
                t!("settings.enter_list", locale = locale)
            ),
        ),
        (
            t!("settings.display_mode", locale = locale).into_owned(),
            format!("< {} >", app.display_mode.localized_label(locale)),
        ),
        (
            t!("settings.ignore_comments", locale = locale).into_owned(),
            on_off(app.config.ignore_comments),
        ),
        (
            t!("settings.cache_ai", locale = locale).into_owned(),
            on_off(app.config.cache_ai_results),
        ),
        (
            t!("settings.cache_history", locale = locale).into_owned(),
            on_off(app.config.cache_history),
        ),
        (
            t!("settings.reuse_session", locale = locale).into_owned(),
            on_off(app.config.reuse_session),
        ),
    ];

    let label_col_w = 24usize;
    let items: Vec<ListItem> = rows_data
        .iter()
        .enumerate()
        .map(|(idx, (k, v))| {
            let selected = idx == app.settings_cursor;
            let k_pad = format!("{:<width$}", k, width = label_col_w);
            ListItem::new(Line::from(vec![
                Span::styled(
                    if selected { "▶ " } else { "  " },
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    k_pad,
                    Style::default()
                        .fg(if selected { Color::White } else { Color::Gray })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    v.clone(),
                    Style::default()
                        .fg(if selected { Color::Cyan } else { Color::Green })
                        .add_modifier(Modifier::BOLD),
                ),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.settings_cursor.min(rows_data.len() - 1)));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(t!("settings.preferences", locale = locale).into_owned()),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(38, 52, 74))
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, sections[0], &mut state);

    let status_line = if let Some(msg) = &app.settings_status {
        Line::from(Span::styled(
            msg.clone(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            t!("settings.config_file", locale = locale, path = cfg_target).into_owned(),
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(status_line), sections[1]);

    let footer = Line::from(Span::styled(
        t!("settings.actions", locale = locale).into_owned(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(footer), sections[2]);
}

fn draw_commit_picker(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let popup = centered_rect(76, 70, area);
    frame.render_widget(Clear, popup);

    let mut items = vec![ListItem::new(Line::from(vec![
        Span::styled(
            t!("commit_picker.wc_badge", locale = locale).into_owned(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(t!("commit_picker.wc_label", locale = locale).into_owned()),
    ]))];

    for ci in &app.commits_cache {
        let msg = ci.message.lines().next().unwrap_or("");
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("[{}] ", ci.revision),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{} ", msg)),
            Span::styled(
                format!("— {}", ci.author),
                Style::default().fg(Color::DarkGray),
            ),
        ])));
    }

    let mut state = ListState::default();
    state.select(Some(app.commit_picker_cursor));
    let title = if app.commits_loading {
        t!("commit_picker.title_loading", locale = locale).into_owned()
    } else {
        t!("commit_picker.title_ready", locale = locale).into_owned()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(title),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(45, 60, 80))
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, popup, &mut state);
}

fn draw_ai_picker(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let popup = centered_rect(60, 45, area);
    frame.render_widget(Clear, popup);

    let active_tag = t!("ai_picker.active_tag", locale = locale);
    let mut items: Vec<ListItem> = app
        .available_providers
        .iter()
        .map(|p| {
            let active = if app
                .resolved_ai_provider_id
                .as_deref()
                .unwrap_or(&app.active_ai_provider_id)
                == p.id
            {
                active_tag.as_ref()
            } else {
                ""
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", p.display_name),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("[bin: {}]{}", p.binary, active),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    items.push(ListItem::new(Line::from(Span::styled(
        t!("ai_picker.off_label", locale = locale).into_owned(),
        Style::default().fg(Color::Gray),
    ))));

    let mut state = ListState::default();
    state.select(Some(app.ai_picker_cursor));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(t!("ai_picker.title", locale = locale).into_owned()),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(45, 60, 80))
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, popup, &mut state);
}

fn commit_msg_box_title<'a>(
    locale: &str,
    has_split: bool,
    is_external: bool,
    editing: bool,
    generating: bool,
    needs_regen: bool,
    dirty: bool,
) -> String {
    let key = if !has_split {
        if editing {
            "commit.msg_editing"
        } else if generating {
            "commit.msg_generating"
        } else if needs_regen && !dirty {
            "commit.msg_pending"
        } else {
            "commit.msg_ready"
        }
    } else if is_external {
        if editing {
            "commit.msg_ext_editing"
        } else if generating {
            "commit.msg_ext_generating"
        } else if needs_regen && !dirty {
            "commit.msg_ext_pending"
        } else {
            "commit.msg_ext_ready"
        }
    } else if editing {
        "commit.msg_main_editing"
    } else if generating {
        "commit.msg_main_generating"
    } else if needs_regen && !dirty {
        "commit.msg_main_pending"
    } else {
        "commit.msg_main_ready"
    };
    t!(key, locale = locale).into_owned()
}

struct CommitMsgRenderParams<'a> {
    title: String,
    message: &'a str,
    cursor: usize,
    focused: bool,
    editing: bool,
    generating: bool,
    started_at: Option<Instant>,
    progress_stage: &'a str,
    stream_logs: &'a [String],
    stream_thinking: &'a str,
    stream_text: &'a str,
    provider_id: &'a str,
}

fn render_commit_msg_box(
    frame: &mut Frame,
    locale: &str,
    area: Rect,
    params: CommitMsgRenderParams<'_>,
) {
    let has_stream = !params.editing
        && (params.generating
            || !params.stream_logs.is_empty()
            || !params.stream_thinking.is_empty()
            || !params.stream_text.is_empty());
    let msg_lines: Vec<Line<'static>> = if params.editing {
        vec![Line::from(text_input::format_with_cursor(
            params.message,
            params.cursor,
            area.width.saturating_sub(4) as usize,
        ))]
    } else {
        let mut lines = vec![Line::from(Span::styled(
            params.message.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))];
        if has_stream {
            append_stream_progress_lines(
                locale,
                &mut lines,
                params.generating,
                params.started_at,
                params.provider_id,
                params.progress_stage,
                params.stream_logs,
                params.stream_thinking,
                params.stream_text,
                2,
                2,
                3,
            );
        }
        lines
    };
    let msg_widget = Paragraph::new(msg_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .padding(Padding::horizontal(1))
                .border_style(if params.editing {
                    Style::default().fg(Color::Yellow)
                } else {
                    pane_border_style(params.focused)
                })
                .title(params.title),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(msg_widget, area);
}

fn draw_commit_confirm(frame: &mut Frame, app: &App, area: Rect) {
    use crate::ui::app::CommitListRow;

    let selected_cnt = app.commit_files.iter().filter(|f| f.included).count();
    let ext_cnt = app
        .commit_files
        .iter()
        .filter(|f| f.included && f.is_external)
        .count();
    let main_cnt = selected_cnt.saturating_sub(ext_cnt);
    let has_ext = ext_cnt > 0;
    let is_git = matches!(app.bundle.vcs, crate::model::VcsKind::Git);
    let has_main_box = !has_ext || main_cnt > 0 || is_git;

    let main_has_stream = !(app.commit_editing_msg
        && app.commit_modal_focus == CommitModalFocus::Message)
        && (app.commit_msg_generating
            || !app.commit_stream_logs.is_empty()
            || !app.commit_stream_thinking.is_empty()
            || !app.commit_stream_text.is_empty());
    let ext_has_stream = !(app.commit_editing_msg
        && app.commit_modal_focus == CommitModalFocus::ExternalMessage)
        && (app.ext_commit_msg_generating
            || !app.ext_commit_stream_logs.is_empty()
            || !app.ext_commit_stream_thinking.is_empty()
            || !app.ext_commit_stream_text.is_empty());

    let main_box_h: u16 = if !has_main_box {
        0
    } else if app.commit_msg_generating {
        if has_ext { 7 } else { 9 }
    } else if main_has_stream {
        if has_ext { 5 } else { 7 }
    } else if has_ext {
        3
    } else {
        4
    };
    let ext_box_h: u16 = if !has_ext {
        0
    } else if app.ext_commit_msg_generating {
        if has_main_box { 7 } else { 9 }
    } else if ext_has_stream {
        if has_main_box { 5 } else { 7 }
    } else if has_main_box {
        3
    } else {
        4
    };
    let footer_h: u16 = if app.commit_error.is_some() { 2 } else { 1 };
    let desired_list_h = (app.commit_rows.len() as u16 + 2).clamp(5, 22);
    let desired_h = 2 + desired_list_h + main_box_h + ext_box_h + footer_h;
    let popup = modal_step1_rect(area, desired_h);
    frame.render_widget(Clear, popup);

    let locale = app.locale();
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(Color::Green))
        .title(t!("commit.modal_title", locale = locale, vcs = app.bundle.vcs).into_owned());
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    let mut constraints = vec![Constraint::Min(5)];
    if has_main_box {
        constraints.push(Constraint::Length(main_box_h));
    }
    if has_ext {
        constraints.push(Constraint::Length(ext_box_h));
    }
    constraints.push(Constraint::Length(footer_h));
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    let footer_sec = sections[sections.len() - 1];

    // 1. Grouped file selection list
    let files_focused = app.commit_modal_focus == CommitModalFocus::Files && !app.commit_editing_msg;
    let list_inner_w = sections[0].width.saturating_sub(4) as usize;
    let file_items: Vec<ListItem> = app
        .commit_rows
        .iter()
        .map(|row| match row {
            CommitListRow::GroupHeader(gi) => {
                let g = &app.commit_groups[*gi];
                let sel_in_group = g
                    .file_indices
                    .iter()
                    .filter(|&&idx| app.commit_files[idx].included)
                    .count();
                let total_in_group = g.file_indices.len();
                let (check, check_color) = if sel_in_group == total_in_group && total_in_group > 0 {
                    ("[x]", Color::Green)
                } else if sel_in_group > 0 {
                    ("[~]", Color::Yellow)
                } else {
                    ("[ ]", Color::DarkGray)
                };
                let arrow = if g.expanded { "▼" } else { "▶" };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", check),
                        Style::default().fg(check_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{} ", arrow), Style::default().fg(Color::Cyan)),
                    Span::styled(
                        g.title.clone(),
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" ({}/{})", sel_in_group, total_in_group),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            }
            CommitListRow::File(fi) => {
                let f = &app.commit_files[*fi];
                let check = if f.included { "[x]" } else { "[ ]" };
                let ext_len = if f.is_external { 11 } else { 0 };
                let shown_path = truncate_path_middle(
                    &f.path,
                    list_inner_w.saturating_sub(12 + ext_len).max(16),
                );
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{} ", check),
                        Style::default().fg(if f.included {
                            Color::Green
                        } else {
                            Color::DarkGray
                        }),
                    ),
                    Span::styled(
                        format!("[{}] ", f.badge),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(shown_path),
                ];
                if f.is_external {
                    spans.push(Span::styled(
                        " [external]",
                        Style::default().fg(Color::Magenta),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let mut file_state = ListState::default();
    if !app.commit_rows.is_empty() {
        file_state.select(Some(app.commit_files_cursor.min(app.commit_rows.len() - 1)));
    }
    let ext_hint = if ext_cnt > 0 {
        format!(" · external {}", ext_cnt)
    } else {
        String::new()
    };
    let file_list = List::new(file_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(pane_border_style(files_focused))
                .title(
                    t!(
                        "commit.files_title",
                        locale = locale,
                        sel = selected_cnt,
                        total = app.commit_files.len(),
                        ext = ext_hint
                    )
                    .into_owned(),
                ),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(38, 52, 74))
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(file_list, sections[0], &mut file_state);

    // 2. Commit message box(es)
    let prov = app
        .resolved_ai_provider_id
        .as_deref()
        .unwrap_or(&app.active_ai_provider_id);
    let mut sec_idx = 1;
    if has_main_box {
        let editing_main =
            app.commit_editing_msg && app.commit_modal_focus == CommitModalFocus::Message;
        let focused_main = app.commit_modal_focus == CommitModalFocus::Message;
        let main_title = commit_msg_box_title(
            locale,
            has_ext,
            false,
            editing_main,
            app.commit_msg_generating,
            app.commit_msg_needs_regen,
            app.commit_msg_dirty,
        );
        render_commit_msg_box(
            frame,
            locale,
            sections[sec_idx],
            CommitMsgRenderParams {
                title: main_title,
                message: &app.commit_message,
                cursor: app.commit_msg_cursor,
                focused: focused_main,
                editing: editing_main,
                generating: app.commit_msg_generating,
                started_at: app.commit_started_at,
                progress_stage: &app.commit_progress_stage,
                stream_logs: &app.commit_stream_logs,
                stream_thinking: &app.commit_stream_thinking,
                stream_text: &app.commit_stream_text,
                provider_id: prov,
            },
        );
        sec_idx += 1;
    }
    if has_ext {
        let editing_ext =
            app.commit_editing_msg && app.commit_modal_focus == CommitModalFocus::ExternalMessage;
        let focused_ext = app.commit_modal_focus == CommitModalFocus::ExternalMessage;
        let ext_title = commit_msg_box_title(
            locale,
            true,
            true,
            editing_ext,
            app.ext_commit_msg_generating,
            app.ext_commit_msg_needs_regen,
            app.ext_commit_msg_dirty,
        );
        render_commit_msg_box(
            frame,
            locale,
            sections[sec_idx],
            CommitMsgRenderParams {
                title: ext_title,
                message: &app.ext_commit_message,
                cursor: app.ext_commit_msg_cursor,
                focused: focused_ext,
                editing: editing_ext,
                generating: app.ext_commit_msg_generating,
                started_at: app.ext_commit_started_at,
                progress_stage: &app.ext_commit_progress_stage,
                stream_logs: &app.ext_commit_stream_logs,
                stream_thinking: &app.ext_commit_stream_thinking,
                stream_text: &app.ext_commit_stream_text,
                provider_id: prov,
            },
        );
    }

    // 3. Footer (error only if any + action bar)
    let mut footer_lines = Vec::new();
    if let Some(err) = &app.commit_error {
        footer_lines.push(Line::from(Span::styled(
            t!("commit.footer_error", locale = locale, err = err).into_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    let step1_key = if has_ext && has_main_box {
        "commit.step1_actions_split"
    } else {
        "commit.step1_actions"
    };
    footer_lines.push(Line::from(Span::styled(
        t!(step1_key, locale = locale).into_owned(),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(
        Paragraph::new(footer_lines).wrap(Wrap { trim: false }),
        footer_sec,
    );
}

fn render_double_confirm_msg_box(
    frame: &mut Frame,
    locale: &str,
    area: Rect,
    title: String,
    message: &str,
    generating: bool,
    started_at: Option<Instant>,
    progress_stage: &str,
    stream_logs: &[String],
    stream_thinking: &str,
    stream_text: &str,
    provider_id: &str,
) {
    let mut msg_lines = vec![Line::from(Span::styled(
        message.trim().to_string(),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ))];
    if generating {
        append_stream_progress_lines(
            locale,
            &mut msg_lines,
            true,
            started_at,
            provider_id,
            progress_stage,
            stream_logs,
            stream_thinking,
            stream_text,
            1,
            1,
            2,
        );
    }
    frame.render_widget(
        Paragraph::new(msg_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .padding(Padding::horizontal(1))
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(title),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_commit_double_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let selected_files: Vec<_> = app.commit_files.iter().filter(|f| f.included).collect();
    let main_cnt = selected_files.iter().filter(|f| !f.is_external).count();
    let ext_cnt = selected_files.iter().filter(|f| f.is_external).count();
    let has_ext = ext_cnt > 0;
    let is_git = matches!(app.bundle.vcs, crate::model::VcsKind::Git);
    let has_main_box = !has_ext || main_cnt > 0 || is_git;

    let mut ext_buckets: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut main_file_cnt = 0usize;
    let mut exts = app.cached_external_prefixes.clone();
    exts.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let main_repo_label = t!("commit.main_repo", locale = locale);
    for f in &selected_files {
        let norm = f.path.trim_start_matches("./");
        if let Some(ext) = exts.iter().find(|e| {
            if is_git {
                norm.starts_with(&format!("{}/", e))
            } else {
                norm == **e || norm.starts_with(&format!("{}/", e))
            }
        }) {
            *ext_buckets
                .entry(format!("external[{}]", ext))
                .or_default() += 1;
        } else {
            main_file_cnt += 1;
        }
    }

    let mut repo_buckets: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (k, v) in &ext_buckets {
        repo_buckets.insert(k.clone(), *v);
    }
    if main_file_cnt > 0 || (is_git && !ext_buckets.is_empty()) {
        let main_key = if is_git && !ext_buckets.is_empty() {
            t!(
                "commit.git_submodule_sync_note",
                locale = locale,
                main = main_repo_label,
                count = ext_buckets.len()
            )
            .into_owned()
        } else {
            main_repo_label.to_string()
        };
        let shown_cnt = if is_git && !ext_buckets.is_empty() && main_file_cnt == 0 {
            ext_buckets.len()
        } else {
            main_file_cnt
        };
        repo_buckets.insert(main_key, shown_cnt);
    }

    let scope_detail = if ext_cnt > 0 {
        t!(
            "commit.scope_with_ext",
            locale = locale,
            main = main_repo_label,
            main_cnt = main_cnt,
            ext_cnt = ext_cnt
        )
        .into_owned()
    } else {
        main_repo_label.to_string()
    };
    let mut top_lines = vec![Line::from(Span::styled(
        t!(
            "commit.summary_files",
            locale = locale,
            count = selected_files.len(),
            scope = scope_detail
        )
        .into_owned(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))];

    if repo_buckets.len() > 1 {
        let bucket_desc: Vec<String> = repo_buckets
            .iter()
            .map(|(k, v)| format!("{}: {}", k, v))
            .collect();
        top_lines.push(Line::from(Span::styled(
            t!(
                "commit.split_commits",
                locale = locale,
                count = repo_buckets.len(),
                desc = bucket_desc.join(", ")
            )
            .into_owned(),
            Style::default().fg(Color::Magenta),
        )));
    }

    top_lines.push(Line::raw(""));
    for g in &app.commit_groups {
        let sel = g
            .file_indices
            .iter()
            .filter(|&&idx| app.commit_files[idx].included)
            .count();
        if sel > 0 {
            top_lines.push(Line::from(format!(
                "  • {} ({}/{})",
                g.title,
                sel,
                g.file_indices.len()
            )));
        }
    }

    let status_h: u16 = if app.commit_executing
        || app.commit_error.is_some()
        || app.commit_hint_notice.is_some()
    {
        1
    } else {
        0
    };
    let top_line_cnt = top_lines.len().min(6) as u16;
    let main_msg_h: u16 = if !has_main_box {
        0
    } else if app.commit_msg_generating {
        6
    } else if has_ext {
        3
    } else {
        4
    };
    let ext_msg_h: u16 = if !has_ext {
        0
    } else if app.ext_commit_msg_generating {
        6
    } else if has_main_box {
        3
    } else {
        4
    };
    let desired_h = 2 + top_line_cnt + main_msg_h + ext_msg_h + status_h + 1;
    let popup = modal_step2_rect(area, desired_h);
    frame.render_widget(Clear, popup);

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(Color::Yellow))
        .title(t!("commit.confirm_title", locale = locale, vcs = app.bundle.vcs).into_owned());
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    let mut constraints = vec![Constraint::Min(3)];
    if has_main_box {
        constraints.push(Constraint::Length(main_msg_h));
    }
    if has_ext {
        constraints.push(Constraint::Length(ext_msg_h));
    }
    constraints.push(Constraint::Length(status_h));
    constraints.push(Constraint::Length(1));

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    frame.render_widget(
        Paragraph::new(top_lines).wrap(Wrap { trim: false }),
        sections[0],
    );

    let prov = app
        .resolved_ai_provider_id
        .as_deref()
        .unwrap_or(&app.active_ai_provider_id);
    let mut sec_idx = 1;
    if has_main_box {
        let main_title_key = match (has_ext, app.commit_msg_generating) {
            (false, true) => "commit.confirm_msg_generating",
            (false, false) => "commit.confirm_msg",
            (true, true) => "commit.confirm_msg_main_generating",
            (true, false) => "commit.confirm_msg_main",
        };
        render_double_confirm_msg_box(
            frame,
            locale,
            sections[sec_idx],
            t!(main_title_key, locale = locale).into_owned(),
            &app.commit_message,
            app.commit_msg_generating,
            app.commit_started_at,
            &app.commit_progress_stage,
            &app.commit_stream_logs,
            &app.commit_stream_thinking,
            &app.commit_stream_text,
            prov,
        );
        sec_idx += 1;
    }
    if has_ext {
        let ext_title_key = if app.ext_commit_msg_generating {
            "commit.confirm_msg_ext_generating"
        } else {
            "commit.confirm_msg_ext"
        };
        render_double_confirm_msg_box(
            frame,
            locale,
            sections[sec_idx],
            t!(ext_title_key, locale = locale).into_owned(),
            &app.ext_commit_message,
            app.ext_commit_msg_generating,
            app.ext_commit_started_at,
            &app.ext_commit_progress_stage,
            &app.ext_commit_stream_logs,
            &app.ext_commit_stream_thinking,
            &app.ext_commit_stream_text,
            prov,
        );
        sec_idx += 1;
    }

    let status_sec = sections[sec_idx];
    let actions_sec = sections[sec_idx + 1];

    if status_h > 0 {
        let status_widget = if app.commit_executing {
            Paragraph::new(Line::from(Span::styled(
                t!("commit.committing", locale = locale).into_owned(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )))
        } else if let Some(err) = &app.commit_error {
            Paragraph::new(Line::from(Span::styled(
                t!("commit.commit_failed", locale = locale, err = err).into_owned(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )))
        } else if let Some(hint) = &app.commit_hint_notice {
            Paragraph::new(Line::from(Span::styled(
                t!("commit.hint_prefix", locale = locale, hint = hint).into_owned(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )))
        } else {
            Paragraph::new(Line::raw(""))
        };
        frame.render_widget(status_widget.wrap(Wrap { trim: false }), status_sec);
    }

    let action_bar = Line::from(Span::styled(
        if app.any_commit_msg_generating() {
            t!("commit.step2_actions_wait", locale = locale).into_owned()
        } else {
            t!("commit.step2_actions", locale = locale).into_owned()
        },
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(action_bar), actions_sec);
}

fn draw_revert_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let selected: Vec<_> = app.revert_files.iter().filter(|f| f.included).collect();
    let selected_cnt = selected.len();
    let multi_file = app.revert_files.len() > 1;
    let mod_cnt = selected
        .iter()
        .filter(|f| f.badge == "M" || f.badge == "R" || f.badge == "D" || f.badge == "P")
        .count();
    let add_cnt = selected.iter().filter(|f| f.badge == "A").count();
    let ext_cnt = selected.iter().filter(|f| f.is_external).count();

    let effect = match (mod_cnt > 0, add_cnt > 0, app.bundle.vcs) {
        (true, true, crate::model::VcsKind::Git) => {
            t!("revert.effect_git_mod_add", locale = locale)
        }
        (true, true, crate::model::VcsKind::Svn) => {
            t!("revert.effect_svn_mod_add", locale = locale)
        }
        (false, true, crate::model::VcsKind::Git) => t!("revert.effect_git_add", locale = locale),
        (false, true, crate::model::VcsKind::Svn) => t!("revert.effect_svn_add", locale = locale),
        (_, _, crate::model::VcsKind::Git) => t!("revert.effect_git_mod", locale = locale),
        (_, _, crate::model::VcsKind::Svn) => t!("revert.effect_svn_mod", locale = locale),
    };
    let ext_note = if ext_cnt > 0 {
        format!(" · external {}", ext_cnt)
    } else {
        String::new()
    };
    let summary_text = t!(
        "revert.step1_summary",
        locale = locale,
        sel = selected_cnt,
        total = app.revert_files.len(),
        ext = ext_note,
        effect = effect
    )
    .into_owned();

    let desired_list_h = (app.revert_files.len() as u16 + 2).clamp(5, 22);
    let footer_h: u16 = if app.revert_error.is_some() { 2 } else { 1 };
    let desired_h = 2 + 1 + desired_list_h + footer_h;
    let popup = modal_step1_rect(area, desired_h);
    frame.render_widget(Clear, popup);

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(Color::Yellow))
        .title(
            t!(
                "revert.modal_title",
                locale = locale,
                vcs = app.bundle.vcs,
                target = app.revert_title
            )
            .into_owned(),
        );
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // Single crisp summary line
            Constraint::Min(4),           // Clean file list
            Constraint::Length(footer_h), // Error (optional) + action bar
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            summary_text,
            Style::default().fg(Color::Yellow),
        ))),
        sections[0],
    );

    let inner_list_w = sections[1].width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .revert_files
        .iter()
        .map(|f| {
            let check = if f.included { "[x]" } else { "[ ]" };
            let ext_len = if f.is_external { 11 } else { 0 };
            let shown_path = truncate_path_middle(
                &f.path,
                inner_list_w.saturating_sub(10 + ext_len).max(16),
            );
            let mut spans = vec![
                Span::styled(
                    format!("{} ", check),
                    Style::default().fg(if f.included {
                        Color::Red
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    format!("[{}] ", f.badge),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(shown_path),
            ];
            if f.is_external {
                spans.push(Span::styled(
                    " [external]",
                    Style::default().fg(Color::Magenta),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default();
    if !app.revert_files.is_empty() {
        state.select(Some(
            app.revert_files_cursor.min(app.revert_files.len() - 1),
        ));
    }
    let list_title = if multi_file {
        t!(
            "revert.files_title_multi",
            locale = locale,
            sel = selected_cnt,
            total = app.revert_files.len()
        )
        .into_owned()
    } else {
        t!("revert.files_title_single", locale = locale).into_owned()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(list_title),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(58, 32, 36))
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, sections[1], &mut state);

    let mut footer = Vec::new();
    if let Some(err) = &app.revert_error {
        footer.push(Line::from(Span::styled(
            t!("commit.footer_error", locale = locale, err = err).into_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    let action_hint = if multi_file {
        t!("revert.step1_actions_multi", locale = locale).into_owned()
    } else {
        t!("revert.step1_actions_single", locale = locale).into_owned()
    };
    footer.push(Line::from(Span::styled(
        action_hint,
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), sections[2]);
}

fn draw_revert_double_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let locale = app.locale();
    let selected: Vec<_> = app.revert_files.iter().filter(|f| f.included).collect();
    let has_added = selected.iter().any(|f| f.badge == "A");
    let ext_cnt = selected.iter().filter(|f| f.is_external).count();
    let note = if has_added && matches!(app.bundle.vcs, crate::model::VcsKind::Git) {
        t!("revert.note_git_add", locale = locale)
    } else {
        t!("revert.note_default", locale = locale)
    };

    let status_h: u16 = if app.revert_executing || app.revert_error.is_some() {
        1
    } else {
        0
    };
    let preview_cnt = selected.len().min(3) as u16;
    let has_more = u16::from(selected.len() > 3);
    let scope_line_cnt = 1u16; // 1 warning + 1 blank + 1 scope + preview + (more)
    let body_h = 2 + scope_line_cnt + preview_cnt + has_more;
    let desired_h = 2 + body_h + status_h + 1 + 1;
    let popup = modal_step2_rect(area, desired_h);
    let path_budget = (popup.width as usize).saturating_sub(20).max(16);

    let mut lines = vec![
        Line::from(Span::styled(
            t!(
                "revert.confirm_warning",
                locale = locale,
                count = selected.len(),
                note = note
            )
            .into_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                t!("revert.target_label", locale = locale).into_owned(),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                app.revert_title.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if ext_cnt > 0 {
                    t!("revert.incl_ext", locale = locale, count = ext_cnt).into_owned()
                } else {
                    String::new()
                },
                Style::default().fg(Color::Magenta),
            ),
        ]),
    ];
    for f in selected.iter().take(3) {
        let ext_tag = if f.is_external { " [external]" } else { "" };
        let shown_path = truncate_path_middle(
            &f.path,
            path_budget.saturating_sub(ext_tag.len()).max(14),
        );
        lines.push(Line::from(format!("  • [{}] {}{}", f.badge, shown_path, ext_tag)));
    }
    if selected.len() > 3 {
        lines.push(Line::from(Span::styled(
            t!("revert.more_files", locale = locale, count = selected.len()).into_owned(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(Clear, popup);

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(Color::Red))
        .title(t!("revert.confirm_title", locale = locale, vcs = app.bundle.vcs).into_owned());
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),           // Concise summary + up to 3 sample files
            Constraint::Length(status_h), // Active status / error only
            Constraint::Length(1),        // Action bar
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), sections[0]);

    if status_h > 0 {
        if app.revert_executing {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    t!("revert.reverting", locale = locale).into_owned(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))),
                sections[1],
            );
        } else if let Some(err) = &app.revert_error {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    t!("revert.revert_failed", locale = locale, err = err).into_owned(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )))
                .wrap(Wrap { trim: false }),
                sections[1],
            );
        }
    }

    let action_bar = Line::from(Span::styled(
        t!("revert.step2_actions", locale = locale).into_owned(),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(action_bar), sections[2]);
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn risk_color(risk: RiskLevel) -> Color {
    match risk {
        RiskLevel::Critical => Color::Red,
        RiskLevel::High => Color::LightRed,
        RiskLevel::Medium => Color::Yellow,
        RiskLevel::Low => Color::Green,
        RiskLevel::Info => Color::Cyan,
    }
}

pub fn truncate_display_width(s: &str, max_cols: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_cols {
        return s.to_string();
    }
    let budget = max_cols.saturating_sub(3);
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out.push_str("...");
    out
}

pub fn truncate_path_middle(path: &str, max_cols: usize) -> String {
    if UnicodeWidthStr::width(path) <= max_cols {
        return path.to_string();
    }
    if max_cols <= 4 {
        return truncate_display_width(path, max_cols);
    }
    let budget = max_cols.saturating_sub(1); // Reserve 1 col for leading '…'
    let mut tail = String::new();
    let mut w = 0usize;
    for ch in path.chars().rev() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        w += cw;
        tail.push(ch);
    }
    let tail: String = tail.chars().rev().collect();
    format!("…{}", tail)
}

#[allow(clippy::too_many_arguments)]
fn append_stream_progress_lines(
    locale: &str,
    lines: &mut Vec<Line<'static>>,
    running: bool,
    started_at: Option<std::time::Instant>,
    provider_id: &str,
    stage: &str,
    logs: &[String],
    thinking: &str,
    text: &str,
    max_logs: usize,
    max_thinking: usize,
    max_text: usize,
) {
    const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    if running {
        let elapsed = started_at.map(|t| t.elapsed()).unwrap_or_default();
        let spin = SPINNER[((elapsed.as_millis() / 100) % 10) as usize];
        lines.push(Line::from(vec![
            Span::styled(
                t!(
                    "progress.running",
                    locale = locale,
                    spin = spin,
                    provider = provider_id,
                    secs = format!("{:.1}", elapsed.as_secs_f32())
                )
                .into_owned(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(stage.to_string(), Style::default().fg(Color::White)),
        ]));
    } else if !stage.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("✓ {} ", provider_id),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(stage.to_string(), Style::default().fg(Color::DarkGray)),
        ]));
    }

    if !logs.is_empty() && max_logs > 0 {
        let start = logs.len().saturating_sub(max_logs);
        for log in &logs[start..] {
            lines.push(Line::from(Span::styled(
                format!("  • {}", log),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let thinking_lines: Vec<&str> = thinking
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if !thinking_lines.is_empty() && max_thinking > 0 {
        lines.push(Line::from(Span::styled(
            t!("progress.thinking", locale = locale).into_owned(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        let start = thinking_lines.len().saturating_sub(max_thinking);
        for l in &thinking_lines[start..] {
            lines.push(Line::from(Span::styled(
                format!("  {}", l),
                Style::default().fg(Color::Gray),
            )));
        }
    }

    let text_lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if !text_lines.is_empty() && max_text > 0 {
        lines.push(Line::from(Span::styled(
            t!(
                "progress.output_chars",
                locale = locale,
                count = text.chars().count()
            )
            .into_owned(),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )));
        let start = text_lines.len().saturating_sub(max_text);
        for l in &text_lines[start..] {
            lines.push(Line::from(Span::styled(
                format!("  {}", l),
                Style::default().fg(Color::White),
            )));
        }
    }
}

fn build_ai_progress_lines(app: &App) -> Vec<Line<'static>> {
    let locale = app.locale();
    let prov = app
        .resolved_ai_provider_id
        .as_deref()
        .unwrap_or(&app.active_ai_provider_id);
    let mut lines: Vec<Line<'static>> = Vec::new();
    append_stream_progress_lines(
        locale,
        &mut lines,
        true,
        app.ai_started_at,
        prov,
        &app.ai_progress_stage,
        &app.ai_stream_logs,
        &app.ai_stream_thinking,
        &app.ai_stream_text,
        4,
        3,
        4,
    );
    let stage_hint = if app.ai_deep_running && !app.ai_running {
        t!("progress.hint_deep", locale = locale).into_owned()
    } else {
        t!("progress.hint_grouping", locale = locale).into_owned()
    };
    lines.push(Line::from(Span::styled(
        stage_hint,
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

fn modal_step1_rect(area: Rect, desired_h: u16) -> Rect {
    let target_w = (area.width * 74 / 100).max(68.min(area.width));
    let min_h = (area.height * 56 / 100).min(area.height).max(14.min(area.height));
    let max_h = (area.height * 82 / 100).max(min_h);
    centered_rect_sized(target_w, desired_h.clamp(min_h, max_h), area)
}

fn modal_step2_rect(area: Rect, desired_h: u16) -> Rect {
    let target_w = (area.width * 54 / 100).max(52.min(area.width));
    let min_h = 10.min(area.height);
    let max_h = (area.height * 46 / 100).max(min_h);
    centered_rect_sized(target_w, desired_h.clamp(min_h, max_h), area)
}

fn centered_rect_sized(target_w: u16, target_h: u16, r: Rect) -> Rect {
    let w = target_w.min(r.width.saturating_sub(2)).max(24.min(r.width));
    let h = target_h.min(r.height.saturating_sub(2)).max(8.min(r.height));
    let x = r.x + r.width.saturating_sub(w) / 2;
    let y = r.y + r.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
