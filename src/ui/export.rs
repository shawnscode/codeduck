use anyhow::Result;
use chrono::Local;
use rust_i18n::t;
use std::path::Path;

use crate::i18n;
use crate::model::{AiReviewResult, ReviewSessionState};
use crate::vcs::DiffBundle;

pub fn export_review_markdown(
    output_path: &Path,
    bundle: &DiffBundle,
    ai_review: Option<&AiReviewResult>,
    session: &ReviewSessionState,
) -> Result<()> {
    export_review_markdown_with_locale(
        output_path,
        bundle,
        ai_review,
        session,
        i18n::DEFAULT_LOCALE,
    )
}

pub fn export_review_markdown_with_locale(
    output_path: &Path,
    bundle: &DiffBundle,
    ai_review: Option<&AiReviewResult>,
    session: &ReviewSessionState,
    locale: &str,
) -> Result<()> {
    let md = render_review_markdown_with_locale(bundle, ai_review, session, locale);
    std::fs::write(output_path, md)?;
    Ok(())
}

pub fn render_review_markdown_with_locale(
    bundle: &DiffBundle,
    ai_review: Option<&AiReviewResult>,
    session: &ReviewSessionState,
    locale: &str,
) -> String {
    let mut md = String::new();
    let now = Local::now().format("%Y-%m-%d %H:%M:%S");

    md.push_str(&t!("export.report_title", locale = locale));
    md.push_str(&format!(
        "- **{}**: {}\n",
        t!("export.generated", locale = locale),
        now
    ));
    md.push_str(&format!("- **VCS**: {}\n", bundle.vcs));
    md.push_str(&format!(
        "- **{}**: `{}`\n",
        t!("export.repository", locale = locale),
        bundle.repo_root.display()
    ));
    md.push_str(&format!(
        "- **{}**: {}\n",
        t!("export.target", locale = locale),
        bundle.target.localized_label(locale)
    ));
    md.push_str(&format!(
        "- **{}**: {}\n",
        t!("export.files", locale = locale),
        bundle.changes.len()
    ));

    if let Some(ci) = &bundle.commit_info {
        md.push_str(&format!(
            "- **{}**: `{}` ({}, {})\n- **{}**: {}\n",
            t!("export.commit", locale = locale),
            ci.revision,
            ci.author,
            ci.date,
            t!("export.message", locale = locale),
            ci.message
        ));
    }
    md.push('\n');

    if let Some(ai) = ai_review {
        md.push_str(&format!(
            "## {} ({})\n\n",
            t!("export.ai_analysis", locale = locale),
            ai.provider_name
        ));
        md.push_str(&format!(
            "- **{}**: {}\n",
            t!("export.overall_risk", locale = locale),
            ai.overall_risk.localized_label(locale)
        ));
        md.push_str(&format!(
            "- **{}**: {}\n",
            t!("export.summary", locale = locale),
            ai.summary
        ));
        if !ai.review_strategy.is_empty() {
            md.push_str(&format!(
                "- **{}**: {}\n",
                t!("export.review_strategy", locale = locale),
                ai.review_strategy
            ));
        }
        md.push('\n');

        if !ai.groups.is_empty() {
            md.push_str(&t!("export.semantic_groups", locale = locale));
            for g in &ai.groups {
                md.push_str(&format!(
                    "#### {}. {} [{} | {}]\n",
                    g.review_order,
                    g.title,
                    g.category,
                    g.risk_level.localized_label(locale)
                ));
                if !g.description.is_empty() {
                    md.push_str(&format!("{}\n\n", g.description));
                }
                for f in &g.files {
                    md.push_str(&format!("- `{}`\n", f));
                }
                if !g.key_points.is_empty() {
                    md.push_str(&t!("export.key_points", locale = locale));
                    for kp in &g.key_points {
                        md.push_str(&format!("  - {}\n", kp));
                    }
                }
                md.push('\n');
            }
        }

        if !ai.findings.is_empty() {
            md.push_str(&t!("export.ai_findings", locale = locale));
            for f in &ai.findings {
                let loc = match f.line {
                    Some(l) => format!("{}:{}", f.file, l),
                    None => f.file.clone(),
                };
                md.push_str(&format!(
                    "- **[{}] {}** (`{}`)\n  {}\n",
                    f.severity.localized_label(locale),
                    f.title,
                    loc,
                    f.description
                ));
                if let Some(sugg) = &f.suggestion {
                    md.push_str(&format!(
                        "  - *{}*: {}\n",
                        t!("export.suggestion", locale = locale),
                        sugg
                    ));
                }
            }
            md.push('\n');
        }

        let elevated_hunks: Vec<_> = ai
            .hunks
            .iter()
            .filter(|h| {
                h.risk_level.is_elevated()
                    && (!h.title.trim().is_empty() || !h.analysis.trim().is_empty())
            })
            .collect();
        if !elevated_hunks.is_empty() {
            md.push_str(&t!("export.ai_hunks", locale = locale));
            for h in elevated_hunks {
                let loc = match (h.start_line, h.end_line) {
                    (Some(s), Some(e)) if s != e => format!("{}:L{}-L{}", h.file, s, e),
                    (Some(s), _) => format!("{}:L{}", h.file, s),
                    _ => h.file.clone(),
                };
                let title = if h.title.trim().is_empty() {
                    h.analysis.as_str()
                } else {
                    h.title.as_str()
                };
                md.push_str(&format!(
                    "- **[{}] {}** (`{}`)\n  {}\n",
                    h.risk_level.localized_label(locale),
                    title,
                    loc,
                    h.analysis
                ));
            }
            md.push('\n');
        }
    }

    md.push_str(&t!("export.changed_files", locale = locale));
    for c in &bundle.changes {
        md.push_str(&format!("- `[{}]` `{}`\n", c.kind.short_badge(), c.path));
    }
    md.push('\n');

    if !session.notes.is_empty() {
        md.push_str(&t!("export.reviewer_notes", locale = locale));
        for note in &session.notes {
            let loc = match note.line {
                Some(l) => format!("{}:{}", note.file, l),
                None => note.file.clone(),
            };
            md.push_str(&format!(
                "- `{}` ({}): {}\n",
                loc, note.created_at, note.text
            ));
        }
        md.push('\n');
    }

    md
}
