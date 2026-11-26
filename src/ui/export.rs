use anyhow::Result;
use chrono::Local;
use std::path::Path;

use crate::model::{AiReviewResult, ReviewSessionState};
use crate::vcs::DiffBundle;

pub fn export_review_markdown(
    output_path: &Path,
    bundle: &DiffBundle,
    ai_review: Option<&AiReviewResult>,
    session: &ReviewSessionState,
) -> Result<()> {
    let mut md = String::new();
    let now = Local::now().format("%Y-%m-%d %H:%M:%S");

    md.push_str("# Code Review Report (codeduck)\n\n");
    md.push_str(&format!("- **Generated**: {}\n", now));
    md.push_str(&format!("- **VCS**: {}\n", bundle.vcs));
    md.push_str(&format!("- **Repository**: `{}`\n", bundle.repo_root.display()));
    md.push_str(&format!("- **Target**: {}\n", bundle.target));
    md.push_str(&format!(
        "- **Files**: {} ({} selected)\n",
        bundle.changes.len(),
        session.selected_files.len()
    ));

    if let Some(ci) = &bundle.commit_info {
        md.push_str(&format!(
            "- **Commit**: `{}` by {} ({})\n- **Message**: {}\n",
            ci.revision, ci.author, ci.date, ci.message
        ));
    }
    md.push('\n');

    if let Some(ai) = ai_review {
        md.push_str(&format!("## AI Analysis ({})\n\n", ai.provider_name));
        md.push_str(&format!("- **Overall Risk**: {}\n", ai.overall_risk.label()));
        md.push_str(&format!("- **Summary**: {}\n", ai.summary));
        if !ai.review_strategy.is_empty() {
            md.push_str(&format!("- **Review Strategy**: {}\n", ai.review_strategy));
        }
        md.push('\n');

        if !ai.groups.is_empty() {
            md.push_str("### Semantic Review Groups\n\n");
            for g in &ai.groups {
                md.push_str(&format!(
                    "#### {}. {} [{} | {}]\n",
                    g.review_order,
                    g.title,
                    g.category,
                    g.risk_level.label()
                ));
                if !g.description.is_empty() {
                    md.push_str(&format!("{}\n\n", g.description));
                }
                for f in &g.files {
                    let checked = if session.selected_files.contains(f) {
                        "x"
                    } else {
                        " "
                    };
                    md.push_str(&format!("- [{}] `{}`\n", checked, f));
                }
                if !g.key_points.is_empty() {
                    md.push_str("\nKey verification points:\n");
                    for kp in &g.key_points {
                        md.push_str(&format!("  - {}\n", kp));
                    }
                }
                md.push('\n');
            }
        }

        if !ai.findings.is_empty() {
            md.push_str("### AI Findings\n\n");
            for f in &ai.findings {
                let loc = match f.line {
                    Some(l) => format!("{}:{}", f.file, l),
                    None => f.file.clone(),
                };
                md.push_str(&format!(
                    "- **[{}] {}** (`{}`)\n  {}\n",
                    f.severity.label(),
                    f.title,
                    loc,
                    f.description
                ));
                if let Some(sugg) = &f.suggestion {
                    md.push_str(&format!("  - *Suggestion*: {}\n", sugg));
                }
            }
            md.push('\n');
        }
    }

    md.push_str("## Changed Files Checklist\n\n");
    for c in &bundle.changes {
        let checked = if session.selected_files.contains(&c.path) {
            "x"
        } else {
            " "
        };
        md.push_str(&format!(
            "- [{}] `[{}]` `{}`\n",
            checked,
            c.kind.short_badge(),
            c.path
        ));
    }
    md.push('\n');

    if !session.notes.is_empty() {
        md.push_str("## Reviewer Notes\n\n");
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

    std::fs::write(output_path, md)?;
    Ok(())
}
