pub mod app;
pub mod export;
pub mod text_input;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::model::{RiskLevel, Severity};
use crate::ui::app::{
    App, CommitModalFocus, InputMode, OverlayKind, PaneFocus, TreeMode, TreeRow,
};

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
        OverlayKind::CommitPicker => draw_commit_picker(frame, app, root),
        OverlayKind::AiPicker => draw_ai_picker(frame, app, root),
        OverlayKind::CommitConfirm => draw_commit_confirm(frame, app, root),
        OverlayKind::CommitDoubleConfirm => {
            draw_commit_confirm(frame, app, root);
            draw_commit_double_confirm(frame, app, root);
        }
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let total = app.bundle.changes.len();

    let (ai_badge, ai_deep_badge) = if app.ai_running {
        const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let elapsed = app.ai_started_at.map(|t| t.elapsed()).unwrap_or_default();
        let spin = SPINNER[((elapsed.as_millis() / 100) % 10) as usize];
        (
            Span::styled(
                format!(" {} 分组中 {}s ", spin, elapsed.as_secs()),
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
                format!(" {} 风险分析 {}s ", spin, elapsed.as_secs()),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ))
        } else {
            None
        };
        (
            Span::styled(
                format!(" {} [{}] ", short_prov, ai.overall_risk.label()),
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
                " 分析失败 (a重试/P切换) ",
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
            format!("{}", app.bundle.target),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(commit_hint, Style::default().fg(Color::Gray)),
    ]);

    let mut line2_spans = vec![
        Span::styled(
            format!("Files: {}", total),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  |  "),
        Span::styled(
            format!("Difft: {}", app.display_mode.label()),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  |  "),
        Span::styled(
            format!(
                "视图: {}",
                match app.tree_mode {
                    TreeMode::AiGrouped => "分组",
                    TreeMode::FlatFiles => "文件",
                }
            ),
            Style::default().fg(Color::Blue),
        ),
        Span::raw("  |  "),
        ai_badge,
    ];
    if let Some(deep) = ai_deep_badge {
        line2_spans.push(Span::raw(" "));
        line2_spans.push(deep);
    }
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
            format!("  model:{}", m),
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
            format!("  effort:{}", e),
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.config.reuse_session {
        if let Some(sid) = app
            .active_ai_session_id
            .as_ref()
            .or_else(|| app.config.ai_sessions.get(&app.active_ai_provider_id))
        {
            let short: String = sid.chars().take(8).collect();
            line2_spans.push(Span::styled(
                format!("  session:{}", short),
                Style::default().fg(Color::Green),
            ));
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
    let focused = app.focus == PaneFocus::Tree;
    let border_style = pane_border_style(focused);
    let max_desc_cols = area.width.saturating_sub(7).max(10) as usize;
    let max_path_cols = area.width.saturating_sub(10).max(12) as usize;
    let title = match app.tree_mode {
        TreeMode::AiGrouped => " 改动分组 (j/k:移动  l:Diff  S:提交) ",
        TreeMode::FlatFiles => " 文件列表 (t:分组  S:提交) ",
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

    let (title, text) = if app.diff_loading {
        (
            format!(" Loading {}... ", app.bundle.target),
            Text::raw(format!(
                "Fetching {} changes from {} and running Difftastic...\n\nYou can press q or Ctrl-C anytime to cancel.",
                app.bundle.target, app.bundle.vcs
            )),
        )
    } else if let Some(err) = &app.diff_error {
        (
            " Diff Load Error ".to_string(),
            Text::raw(format!(
                "Error loading diff:\n\n{}\n\nPress R to retry or C to pick another commit.",
                err
            )),
        )
    } else if app.continuous.lines.is_empty() {
        (
            " Continuous Diff ".to_string(),
            Text::raw("No changed files in this target.\n\nPress C to switch to another commit/revision, or R to refresh."),
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
        let title = format!(
            " {} | {}/{} (n/N跳块, hjkl移动) ",
            active_path, active_hunk_num, hunk_total
        );

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
    if let Some(fp) = file_path {
        let file_notes: Vec<_> = app.session.notes.iter().filter(|n| n.file == fp).collect();
        if !file_notes.is_empty() {
            lines.push(Line::raw(""));
            for n in file_notes {
                push_multiline(
                    lines,
                    &format!("批注 [{}]: ", n.created_at),
                    &n.text,
                    Style::default().fg(Color::Magenta),
                );
            }
        }
    }
    if app.ai_followup_running {
        lines.push(Line::from(Span::styled(
            "正在查询...",
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
            format!("问{}: {}", tag, last_qa.question),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        push_multiline(lines, "答: ", &last_qa.answer, Style::default().fg(Color::Cyan));
    }
}

fn draw_passive_info_pane(frame: &mut Frame, app: &App, area: Rect) {
    let (base_title, lines) = match app.focus {
        PaneFocus::Tree => build_tree_context_info(app),
        PaneFocus::Diff => build_diff_context_info(app),
    };
    let title = if app.inspector_scroll_y > 0 {
        format!("{} (J/K滚动: +{}) ", base_title.trim_end(), app.inspector_scroll_y)
    } else {
        format!("{} (J/K滚动) ", base_title.trim_end())
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
    let title = " 分组详情 ".to_string();
    let mut lines: Vec<Line<'static>> = Vec::new();

    if app.ai_running {
        lines.extend(build_ai_progress_lines(app));
        append_qa_and_notes(&mut lines, app, app.current_file().map(|f| f.path.as_str()));
        return (title, lines);
    }

    if let Some(err) = &app.ai_error {
        lines.push(Line::from(Span::styled(
            format!("分析出错: {}", err),
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
                format!("[{} | {}]", grp.category, grp.risk_level.label()),
                Style::default()
                    .fg(risk_color(grp.risk_level))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        if !grp.description.is_empty() {
            push_multiline(
                &mut lines,
                "说明: ",
                &grp.description,
                Style::default().fg(Color::Cyan),
            );
        }
        if !grp.key_points.is_empty() {
            lines.push(Line::from(Span::styled(
                "核查点:",
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
                    format!("{} [{}|{}]: ", fa.path, fa.category, fa.risk_level.label()),
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
                "概述: ",
                &ai.summary,
                Style::default().fg(Color::White),
            );
            if !ai.review_strategy.is_empty() {
                push_multiline(
                    &mut lines,
                    "建议: ",
                    &ai.review_strategy,
                    Style::default().fg(Color::Yellow),
                );
            }
        } else {
            lines.push(Line::from("暂无分析结果（按 a 分析，P 切换工具）。"));
        }
    }

    append_qa_and_notes(&mut lines, app, app.current_file().map(|f| f.path.as_str()));
    (title, lines)
}

fn build_diff_context_info(app: &App) -> (String, Vec<Line<'static>>) {
    let (fd, hunk, source_line, hunk_analysis, findings) = app.active_hunk_context();
    let loc_label = match (fd, source_line, hunk) {
        (Some(f), Some(ln), _) => format!("{} : L{}", f.path, ln),
        (Some(f), None, Some(h)) if h.start_line.is_some() => {
            format!("{} : L{}", f.path, h.start_line.unwrap())
        }
        (Some(f), _, _) => f.path.clone(),
        (None, _, _) => "未定位文件".to_string(),
    };
    let title = format!(" 代码说明 ({}) ", loc_label);
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
                format!("[{}] ", ha.risk_level.label()),
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
                    format!("[文件 {}] ", fa.risk_level.label()),
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
                    format!("[{}] ", f.severity.label()),
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
                push_multiline(&mut lines, "  建议: ", sugg, Style::default().fg(Color::Green));
            }
        }
    }

    append_qa_and_notes(&mut lines, app, fd.map(|f| f.path.as_str()));

    if lines.is_empty() {
        lines.push(Line::from(
            "当前片段暂无说明（按 A 提问，a 重跑分析）。",
        ));
    }

    (title, lines)
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mode_label = match app.input_mode {
        InputMode::Normal => (" NORMAL ", Color::Green),
        InputMode::Command => (" COMMAND ", Color::Yellow),
        InputMode::Search => (" SEARCH ", Color::Cyan),
        InputMode::AddNote => (" NOTE ", Color::Magenta),
        InputMode::AskAi => (" ASK AI ", Color::Blue),
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
            format!(" [{}]{} ", app.focus.label(), pending),
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
            "Tab:切窗  hjkl:移动  n/N:跳块  J/K:滚底  S:提交  R:刷新  d:视图  B/I:收展  ?:帮助  q:退出",
            Style::default().fg(Color::DarkGray),
        )),
        InputMode::Command => Line::from(format!(":{}", edited())),
        InputMode::Search => Line::from(format!("/{}", edited())),
        InputMode::AddNote => Line::from(format!("Note for current file > {}", edited())),
        InputMode::AskAi => Line::from(format!("Ask AI about current file > {}", edited())),
    };

    let footer = Paragraph::new(vec![status_line, cmd_line]);
    frame.render_widget(footer, area);
}

fn draw_help_modal(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(78, 84, area);
    frame.render_widget(Clear, popup);

    let help_text = vec![
        Line::from(Span::styled(
            "codeduck — 快捷键说明",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "窗口与浏览",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / Ctrl-w   切换焦点（左侧列表 / 右侧 Diff）"),
        Line::from("  H / L          直接切到左侧列表 / 右侧 Diff"),
        Line::from("  J / K          滚动底部详情面板（B/I 显隐侧栏/底栏）"),
        Line::raw(""),
        Line::from(Span::styled(
            "移动与跳转",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  j / k          上下移动"),
        Line::from("  h / l / 0      折叠展开分组 / 左右滚动 Diff / 回行首"),
        Line::from("  n / N          下/上一个差异块（]c / [c 亦可）"),
        Line::from("  ]f / [f        下/上一个文件"),
        Line::from("  ]g / [g        下/上一个分组"),
        Line::from("  ]i / [i        下/上一个风险提示"),
        Line::from("  Ctrl-d / u     半页上下；Ctrl-f / b 整页；gg / G 顶 / 底"),
        Line::from("  / 与 ; / ,     搜索与跳转下一/上一匹配"),
        Line::raw(""),
        Line::from(Span::styled(
            "审阅、提交与命令",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  d / t          切换 Diff 模式 / 切换分组与文件列表"),
        Line::from("  za / zM / zR   折叠当前组 / 全部折叠 / 全部展开"),
        Line::from("  S              打开提交面板（按分组勾选文件并生成英文提交说明）"),
        Line::from("  c / e          添加批注 / 导出 Markdown (codeduck-review.md)"),
        Line::from("  a / P / A      分析改动 / 切换工具 / 对当前文件提问"),
        Line::from("  C / R          切换 Commit 或未提交改动 / 强刷"),
        Line::from("  :ai / :model / :effort  切换工具、模型与推理强度"),
        Line::from("  :new-session   重置对话会话并重新分析（:reset-session 亦可）"),
        Line::from("  :ignore <pat>  添加忽略规则到 .codeduck"),
        Line::from("  :rev 123       查看 SVN r123（或 :rev 100:120, :uncommitted）"),
        Line::raw(""),
        Line::from(Span::styled(
            "j/k 或 Ctrl-d/u 滚动；Esc / q / ? 关闭。",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let widget = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Help (j/k 滚动) "),
        )
        .scroll((app.help_scroll_y, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, popup);
}

fn draw_commit_picker(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(76, 70, area);
    frame.render_widget(Clear, popup);

    let mut items = vec![ListItem::new(Line::from(vec![
        Span::styled(
            "[WC] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Uncommitted working copy changes"),
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
        " Select Target to Review (checking history... | r:refresh, Esc:cancel) "
    } else {
        " Select Target to Review (Enter:load, r:refresh, Esc:cancel) "
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
    let popup = centered_rect(60, 45, area);
    frame.render_widget(Clear, popup);

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
                " (active)"
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
        "Off (disable AI analysis)",
        Style::default().fg(Color::Gray),
    ))));

    let mut state = ListState::default();
    state.select(Some(app.ai_picker_cursor));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Select Local AI CLI Subscription "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(45, 60, 80))
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, popup, &mut state);
}

fn draw_commit_confirm(frame: &mut Frame, app: &App, area: Rect) {
    use crate::ui::app::CommitListRow;

    let popup = centered_rect(84, 82, area);
    frame.render_widget(Clear, popup);

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green))
        .title(format!(" 提交 ({}) ", app.bundle.vcs));
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    let has_commit_stream = !app.commit_editing_msg
        && (app.commit_msg_generating
            || !app.commit_stream_logs.is_empty()
            || !app.commit_stream_thinking.is_empty()
            || !app.commit_stream_text.is_empty());
    let msg_box_height = if app.commit_msg_generating {
        11
    } else if has_commit_stream {
        8
    } else {
        6
    };

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),              // Grouped file selection list
            Constraint::Length(msg_box_height), // Single-sentence editable commit message + AI stream output
            Constraint::Length(3),           // Error + action bar
        ])
        .split(inner);

    // 1. Grouped file selection list
    let files_focused = app.commit_modal_focus == CommitModalFocus::Files && !app.commit_editing_msg;
    let selected_cnt = app.commit_files.iter().filter(|f| f.included).count();
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
                    Span::styled(format!("{} ", check), Style::default().fg(check_color).add_modifier(Modifier::BOLD)),
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
                    Span::raw(f.path.clone()),
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
    let file_list = List::new(file_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(pane_border_style(files_focused))
                .title(format!(
                    " 待提交文件 ({}/{}) — j/k:移动  Space:勾选  h/l:折展  a:全选 ",
                    selected_cnt,
                    app.commit_files.len()
                )),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(38, 52, 74))
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(file_list, sections[0], &mut file_state);

    // 2. Commit message box (single sentence)
    let msg_focused = app.commit_modal_focus == CommitModalFocus::Message || app.commit_editing_msg;
    let msg_title = if app.commit_editing_msg {
        " 提交说明 [编辑中 — Enter/Esc:完成  Ctrl-S:下一步] "
    } else if app.commit_msg_generating {
        " 提交说明 [生成中...] "
    } else if app.commit_msg_needs_regen && !app.commit_msg_dirty {
        " 提交说明 [选区已变更，即将自动重生成 · g:立即生成  e/i:编辑] "
    } else {
        " 提交说明 [e/i:编辑  g:重新生成] "
    };

    let msg_lines: Vec<Line<'static>> = if app.commit_editing_msg {
        vec![Line::from(text_input::format_with_cursor(
            &app.commit_message,
            app.commit_msg_cursor,
            sections[1].width.saturating_sub(2) as usize,
        ))]
    } else {
        let mut lines = vec![Line::from(Span::styled(
            app.commit_message.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))];
        if has_commit_stream {
            let prov = app
                .resolved_ai_provider_id
                .as_deref()
                .unwrap_or(&app.active_ai_provider_id);
            append_stream_progress_lines(
                &mut lines,
                app.commit_msg_generating,
                app.commit_started_at,
                prov,
                &app.commit_progress_stage,
                &app.commit_stream_logs,
                &app.commit_stream_thinking,
                &app.commit_stream_text,
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
                .border_style(if app.commit_editing_msg {
                    Style::default().fg(Color::Yellow)
                } else {
                    pane_border_style(msg_focused)
                })
                .title(msg_title),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(msg_widget, sections[1]);

    // 3. Footer / error / next-step hint
    let mut footer_lines = Vec::new();
    if let Some(err) = &app.commit_error {
        footer_lines.push(Line::from(Span::styled(
            format!("错误: {}", err),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    } else {
        footer_lines.push(Line::from(Span::styled(
            "按 Space 勾选分组或单文件，确认前可随时返回。",
            Style::default().fg(Color::DarkGray),
        )));
    }
    footer_lines.push(Line::from(Span::styled(
        "y / Enter / Ctrl-S: 下一步  |  Space: 勾选  |  g: 重新生成  |  e/i: 编辑  |  Esc/q: 取消",
        Style::default().fg(Color::Cyan),
    )));
    frame.render_widget(
        Paragraph::new(footer_lines).wrap(Wrap { trim: false }),
        sections[2],
    );
}

fn draw_commit_double_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(72, 62, area);
    frame.render_widget(Clear, popup);

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" 确认提交 ({}) ", app.bundle.vcs));
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4), // Groups & repo split summary
            Constraint::Length(if app.commit_msg_generating { 9 } else { 6 }), // Fixed 1-sentence commit message + status + action bar
        ])
        .split(inner);

    let selected_files: Vec<_> = app.commit_files.iter().filter(|f| f.included).collect();
    let main_cnt = selected_files.iter().filter(|f| !f.is_external).count();
    let ext_cnt = selected_files.iter().filter(|f| f.is_external).count();

    let mut repo_buckets: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut exts = app.cached_external_prefixes.clone();
    exts.sort_by_key(|b| std::cmp::Reverse(b.len()));
    for f in &selected_files {
        let norm = f.path.trim_start_matches("./");
        if let Some(ext) = exts
            .iter()
            .find(|e| norm == **e || norm.starts_with(&format!("{}/", e)))
        {
            *repo_buckets
                .entry(format!("external[{}]", ext))
                .or_default() += 1;
        } else {
            *repo_buckets.entry("主仓库".to_string()).or_default() += 1;
        }
    }

    let mut top_lines = vec![Line::from(Span::styled(
        format!(
            "提交 {} 个文件（主仓库 {}, external {}）",
            selected_files.len(),
            main_cnt,
            ext_cnt
        ),
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
            format!(
                "将按仓库拆分为 {} 次 {} 提交（{}）。",
                repo_buckets.len(),
                app.bundle.vcs,
                bucket_desc.join("，")
            ),
            Style::default().fg(Color::Magenta),
        )));
    }

    top_lines.push(Line::raw(""));
    top_lines.push(Line::from(Span::styled(
        "包含分组：",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
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
    frame.render_widget(
        Paragraph::new(top_lines).wrap(Wrap { trim: false }),
        sections[0],
    );

    let mut bottom_lines = vec![
        Line::from(Span::styled(
            if app.commit_msg_generating {
                "提交说明（生成中...）："
            } else {
                "提交说明："
            },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {}", app.commit_message.trim()),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];

    if app.commit_msg_generating {
        let prov = app
            .resolved_ai_provider_id
            .as_deref()
            .unwrap_or(&app.active_ai_provider_id);
        append_stream_progress_lines(
            &mut bottom_lines,
            true,
            app.commit_started_at,
            prov,
            &app.commit_progress_stage,
            &app.commit_stream_logs,
            &app.commit_stream_thinking,
            &app.commit_stream_text,
            1,
            1,
            2,
        );
    } else if app.commit_executing {
        bottom_lines.push(Line::from(Span::styled(
            "正在执行提交，请稍候...",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    } else if let Some(err) = &app.commit_error {
        bottom_lines.push(Line::from(Span::styled(
            format!("提交失败: {}", err),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    } else {
        bottom_lines.push(Line::from(Span::styled(
            "确认文件与说明无误后提交：",
            Style::default().fg(Color::Gray),
        )));
    }
    bottom_lines.push(Line::from(Span::styled(
        if app.commit_msg_generating {
            "y/Enter: 生成后提交  |  f: 用当前说明提交  |  n/Esc: 返回"
        } else {
            "y/Enter: 确认提交  |  n/Esc/q: 返回"
        },
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    frame.render_widget(
        Paragraph::new(bottom_lines).wrap(Wrap { trim: false }),
        sections[1],
    );
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
                format!("{} 处理中 ({} · {:.1}s) ", spin, provider_id, elapsed.as_secs_f32()),
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
            "推理:",
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
            format!("输出 ({} 字):", text.chars().count()),
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
    let prov = app
        .resolved_ai_provider_id
        .as_deref()
        .unwrap_or(&app.active_ai_provider_id);
    let mut lines: Vec<Line<'static>> = Vec::new();
    append_stream_progress_lines(
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
    lines.push(Line::from(Span::styled(
        "可先用 j/k 或 Tab 浏览 Diff，完成后自动刷新分组。",
        Style::default().fg(Color::DarkGray),
    )));
    lines
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
