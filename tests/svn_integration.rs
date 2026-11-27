use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tempfile::TempDir;

use codeduck::ai;
use codeduck::config::{AppConfig, CustomAiConfig};
use codeduck::difft::DifftEngine;
use codeduck::model::{DiffTarget, DifftDisplayMode, VcsKind};
use codeduck::ui::app::{App, PaneFocus, TreeMode};
use codeduck::vcs;

fn run(cmd: &str, args: &[&str], cwd: &std::path::Path) {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {} {:?}: {}", cmd, args, e));
    assert!(
        out.status.success(),
        "`{} {:?}` failed:\nstdout: {}\nstderr: {}",
        cmd,
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn test_svn_uncommitted_and_commit_with_difft_and_mock_ai() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wc_dir = tmp.path().join("wc");
    fs::create_dir_all(&repo_dir).unwrap();

    // 1. Create a real local SVN repository and working copy
    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());

    // Commit r1: initial calculator module
    fs::create_dir_all(wc_dir.join("src")).unwrap();
    fs::write(
        wc_dir.join("src/calc.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    fs::write(wc_dir.join("README.txt"), "initial\n").unwrap();
    run("svn", &["add", "src", "README.txt"], &wc_dir);
    run("svn", &["commit", "-m", "r1: initial calc"], &wc_dir);

    // Commit r2: modify calc.rs and delete README.txt
    fs::write(
        wc_dir.join("src/calc.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a.saturating_add(b)\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
    )
    .unwrap();
    run("svn", &["delete", "README.txt"], &wc_dir);
    run("svn", &["commit", "-m", "r2: saturating add and sub"], &wc_dir);

    // Uncommitted working copy edits: modify calc.rs + add helper.rs
    fs::write(
        wc_dir.join("src/calc.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a.saturating_add(b)\n}\n\npub fn mul(a: i32, b: i32) -> i32 {\n    a.saturating_mul(b)\n}\n",
    )
    .unwrap();
    fs::write(
        wc_dir.join("src/helper.rs"),
        "pub fn is_zero(v: i32) -> bool {\n    v == 0\n}\n",
    )
    .unwrap();
    run("svn", &["add", "src/helper.rs"], &wc_dir);

    // 2. Test SVN backend detection and change collection
    let backend = vcs::open_vcs(&wc_dir, None).expect("should auto-detect SVN");
    assert_eq!(backend.kind(), VcsKind::Svn);

    let wc_bundle = backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert_eq!(wc_bundle.changes.len(), 2);
    assert_eq!(wc_bundle.changes[0].path, "src/calc.rs");
    assert_eq!(wc_bundle.changes[1].path, "src/helper.rs");

    let r2_bundle = backend
        .collect_changes(&DiffTarget::Commit("r2".to_string()))
        .unwrap();
    assert_eq!(r2_bundle.changes.len(), 2);
    assert_eq!(
        r2_bundle.commit_info.as_ref().unwrap().message,
        "r2: saturating add and sub"
    );

    let commits = backend.recent_commits(5).unwrap();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].revision, "2");

    // 3. Test Difftastic rendering + JSON parsing
    let engine = DifftEngine::new("difft", false).unwrap();
    let rendered = engine
        .render_file(&wc_bundle.changes[0], DifftDisplayMode::SideBySide, 100)
        .unwrap();
    assert_eq!(rendered.language, "Rust");
    assert!(!rendered.ansi_output.is_empty());
    assert!(rendered.json.is_some());
    assert!(!rendered.hunk_anchors.is_empty());
    assert!(rendered.ai_summary_diff.contains("src/calc.rs"));

    // 4. Test Mock local AI CLI subscription script
    let mock_ai = tmp.path().join("mock-ai.sh");
    fs::write(
        &mock_ai,
        r#"#!/bin/sh
cat >/dev/null
cat <<'EOF'
```json
{
  "summary": "Replaced sub with mul in calc.rs and added is_zero helper.",
  "overall_risk": "low",
  "review_strategy": "Inspect calc.rs API change first, then check helper.rs.",
  "groups": [
    {
      "id": "g1",
      "title": "Core Math API",
      "description": "Updated arithmetic functions in calc.rs",
      "category": "refactor",
      "risk_level": "medium",
      "review_order": 1,
      "files": ["src/calc.rs"],
      "key_points": ["Verify removing sub() does not break callers"]
    }
  ],
  "files": [
    {
      "path": "src/calc.rs",
      "category": "refactor",
      "risk_level": "medium",
      "summary": "Replaced sub with saturating mul",
      "review_focus": ["Check public API compatibility"]
    }
  ],
  "hunks": [
    {
      "file": "src/calc.rs",
      "chunk_index": 1,
      "start_line": 5,
      "end_line": 7,
      "title": "Switch sub to saturating_mul",
      "analysis": "Replaced sub() with saturating_mul(), changing public API surface.",
      "risk_level": "medium"
    }
  ],
  "findings": [
    {
      "id": "f1",
      "file": "src/calc.rs",
      "line": 5,
      "severity": "warning",
      "title": "Removed public function sub",
      "description": "pub fn sub was removed in working copy.",
      "suggestion": "Confirm all callers are migrated to mul or keep sub."
    }
  ]
}
```
EOF
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&mock_ai).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mock_ai, perms).unwrap();

    let mut cfg = AppConfig::default();
    cfg.ai_provider = "mock-sub".to_string();
    cfg.cache_ai_results = false;
    cfg.custom_ai.push(CustomAiConfig {
        name: "mock-sub".to_string(),
        command: mock_ai.to_string_lossy().to_string(),
        args: vec![],
    });

    let provider = ai::resolve_provider("mock-sub", &cfg).expect("mock provider should resolve");
    let rendered_all: Vec<_> = wc_bundle
        .changes
        .iter()
        .map(|c| engine.render_file(c, DifftDisplayMode::SideBySide, 100).unwrap())
        .collect();
    let ai_res = ai::analyze_diff(provider.as_ref(), &wc_bundle, &rendered_all, &cfg, true).unwrap();
    // g1 + auto-created fallback group for uncategorized src/helper.rs
    assert_eq!(ai_res.groups.len(), 2);
    assert_eq!(ai_res.findings.len(), 1);
    assert_eq!(ai_res.findings[0].file, "src/calc.rs");

    // 5. Test App state + Vim keybindings + Markdown export
    let backend2 = vcs::open_vcs(&wc_dir, Some(VcsKind::Svn)).unwrap();
    let mut app = App::new(backend2, cfg, DiffTarget::Uncommitted, 120).unwrap();

    // Wait briefly for two-stage background AI thread (Stage 1 groups -> Stage 2 findings)
    let mut saw_stage1_groups_early = false;
    for _ in 0..40 {
        app.poll_background();
        if !app.ai_running && app.ai_review.is_some() && app.ai_deep_running {
            saw_stage1_groups_early = true;
        }
        if !app.ai_running && !app.ai_deep_running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = saw_stage1_groups_early;
    assert!(app.ai_review.is_some());
    assert_eq!(app.tree_mode, TreeMode::AiGrouped);
    assert!(app.continuous.sections.len() == 2);
    assert!(app.continuous.global_hunks.len() >= 2);

    // Default focus stays in Diff; Tab / Ctrl-w / H never focus the left file list
    assert_eq!(app.focus, PaneFocus::Diff);
    assert_eq!(app.active_group_in_tree().unwrap().id, "g1");
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE));
    assert_eq!(app.focus, PaneFocus::Diff);

    // f / F step next / previous file while keeping focus in Diff
    let initial_file = app.selected_file_idx;
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
    assert_eq!(app.focus, PaneFocus::Diff);
    assert_ne!(app.selected_file_idx, initial_file);
    app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, PaneFocus::Diff);
    assert_eq!(app.selected_file_idx, initial_file);

    // When AI issue list is populated, n / N cycle through AI issues & elevated hunks
    let (_, _, _, hunk_ana, findings) = app.active_hunk_context();
    assert_eq!(hunk_ana.unwrap().title, "Switch sub to saturating_mul");
    assert_eq!(findings.len(), 1);

    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.status_message.contains("Removed public function sub"));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.status_message.contains("Switch sub to saturating_mul"));
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
    assert!(app.status_message.contains("Removed public function sub"));

    // Without AI issues, n / N fall back to jumping cross-file hunks
    let saved_ai = app.ai_review.take();
    let before_hunk_y = app.diff_cursor_y;
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_ne!(app.diff_cursor_y, before_hunk_y);
    app.ai_review = saved_ai;

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert_eq!(app.display_mode, DifftDisplayMode::Inline);

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
    let report = wc_dir.join("codeduck-review.md");
    assert!(report.exists());
    let report_text = fs::read_to_string(&report).unwrap();
    assert!(report_text.contains("Core Math API"));
    assert!(report_text.contains("Removed public function sub"));
}

#[test]
fn test_git_backend_uncommitted_and_commit() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    run("git", &["init"], repo);
    run("git", &["config", "user.email", "test@example.com"], repo);
    run("git", &["config", "user.name", "Test"], repo);

    fs::write(repo.join("main.py"), "def greet():\n    return 'hi'\n").unwrap();
    run("git", &["add", "main.py"], repo);
    run("git", &["commit", "-m", "initial commit"], repo);

    fs::write(repo.join("main.py"), "def greet(name):\n    return f'hi {name}'\n").unwrap();

    let backend = vcs::open_vcs(repo, None).expect("should auto-detect Git");
    assert_eq!(backend.kind(), VcsKind::Git);

    let wc = backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert_eq!(wc.changes.len(), 1);
    assert_eq!(wc.changes[0].path, "main.py");

    let commits = backend.recent_commits(5).unwrap();
    assert_eq!(commits.len(), 1);
    let c1 = backend
        .collect_changes(&DiffTarget::Commit(commits[0].revision.clone()))
        .unwrap();
    assert_eq!(c1.changes.len(), 1);
}

#[test]
fn test_uncommitted_scoped_to_current_subdirectory() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wc_dir = tmp.path().join("wc");
    fs::create_dir_all(&repo_dir).unwrap();

    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());

    fs::create_dir_all(wc_dir.join("module_a")).unwrap();
    fs::create_dir_all(wc_dir.join("module_b")).unwrap();
    fs::write(wc_dir.join("module_a/a.lua"), "return 1\n").unwrap();
    fs::write(wc_dir.join("module_b/b.lua"), "return 2\n").unwrap();
    run("svn", &["add", "module_a", "module_b"], &wc_dir);
    run("svn", &["commit", "-m", "r1: add modules"], &wc_dir);

    // Edit both module_a and module_b
    fs::write(wc_dir.join("module_a/a.lua"), "return 10\n").unwrap();
    fs::write(wc_dir.join("module_b/b.lua"), "return 20\n").unwrap();

    // Open VCS inside module_a -> uncommitted should only see a.lua
    let sub_svn = vcs::open_vcs(&wc_dir.join("module_a"), Some(VcsKind::Svn)).unwrap();
    let sub_bundle = sub_svn.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert_eq!(sub_bundle.changes.len(), 1);
    assert_eq!(sub_bundle.changes[0].path, "a.lua");

    // Git subdirectory scoping check
    let git_dir = tmp.path().join("git_repo");
    fs::create_dir_all(git_dir.join("dir1")).unwrap();
    fs::create_dir_all(git_dir.join("dir2")).unwrap();
    run("git", &["init"], &git_dir);
    run("git", &["config", "user.email", "test@example.com"], &git_dir);
    run("git", &["config", "user.name", "Test"], &git_dir);
    fs::write(git_dir.join("dir1/f1.rs"), "fn a() {}\n").unwrap();
    fs::write(git_dir.join("dir2/f2.rs"), "fn b() {}\n").unwrap();
    run("git", &["add", "."], &git_dir);
    run("git", &["commit", "-m", "init"], &git_dir);

    fs::write(git_dir.join("dir1/f1.rs"), "fn a() { let _ = 1; }\n").unwrap();
    fs::write(git_dir.join("dir2/f2.rs"), "fn b() { let _ = 2; }\n").unwrap();

    let sub_git = vcs::open_vcs(&git_dir.join("dir1"), Some(VcsKind::Git)).unwrap();
    let git_bundle = sub_git.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert_eq!(git_bundle.changes.len(), 1);
    assert_eq!(git_bundle.changes[0].path, "dir1/f1.rs");
}

#[test]
fn test_codeduck_ignore_file() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wc_dir = tmp.path().join("wc");
    fs::create_dir_all(&repo_dir).unwrap();

    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());

    fs::create_dir_all(wc_dir.join("src")).unwrap();
    fs::create_dir_all(wc_dir.join("3rds")).unwrap();
    fs::write(wc_dir.join("src/app.rs"), "fn main() {}\n").unwrap();
    fs::write(wc_dir.join("src/a.meta"), "guid: 1\n").unwrap();
    fs::write(wc_dir.join("src/keep.meta"), "guid: 2\n").unwrap();
    fs::write(wc_dir.join("3rds/lib.cs"), "class Lib {}\n").unwrap();
    run("svn", &["add", "src", "3rds"], &wc_dir);
    run("svn", &["commit", "-m", "r1: init"], &wc_dir);

    // Pre-populate TOML .codeduck with [ai] settings and [ignore].patterns
    fs::write(
        wc_dir.join(".codeduck"),
        r#"# project codeduck config
[ai]
provider = "agy"
model = "gemini-4-hydrogen-high"
effort = "medium"

[ignore]
patterns = [
  "*.meta",
  "!keep.meta",
  "3rds/",
]
"#,
    )
    .unwrap();

    // Modify all 4 files
    fs::write(wc_dir.join("src/app.rs"), "fn main() { let x = 1; }\n").unwrap();
    fs::write(wc_dir.join("src/a.meta"), "guid: 10\n").unwrap();
    fs::write(wc_dir.join("src/keep.meta"), "guid: 20\n").unwrap();
    fs::write(wc_dir.join("3rds/lib.cs"), "class Lib { int v; }\n").unwrap();

    let backend = vcs::open_vcs(&wc_dir, Some(VcsKind::Svn)).unwrap();
    let mut cfg = AppConfig::default();
    cfg.apply_project_overrides(backend.repo_root(), backend.scope_dir());
    assert_eq!(cfg.ai_provider, "agy");
    assert_eq!(cfg.ai_model.as_deref(), Some("gemini-4-hydrogen-high"));
    assert_eq!(cfg.ai_effort.as_deref(), Some("medium"));

    cfg.ai_provider = "none".to_string();
    let mut app = App::new(backend, cfg, DiffTarget::Uncommitted, 120).unwrap();

    // Only src/app.rs and src/keep.meta should remain (a.meta & 3rds/lib.cs ignored by .codeduck)
    let paths: Vec<_> = app.bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, vec!["src/app.rs", "src/keep.meta"]);

    // Pressing 'S' opens CommitConfirm with all non-ignored files checked by default
    app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::CommitConfirm);
    assert_eq!(app.commit_files.len(), 2);
    assert!(app.commit_files[0].included);
    assert!(app.commit_files[1].included);
    // Toggling inside CommitConfirm unchecks the first file
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(!app.commit_files[0].included);
    assert!(app.commit_files[1].included);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // ':ignore /src/app.rs' ex-command appends to TOML [ignore].patterns & preserves [ai]
    app.add_ignore_pattern("/src/app.rs");
    let paths_after: Vec<_> = app.bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths_after, vec!["src/keep.meta"]);

    let codeduck_content = fs::read_to_string(wc_dir.join(".codeduck")).unwrap();
    let parsed: codeduck::config::ProjectCodeduckConfig = toml::from_str(&codeduck_content).unwrap();
    assert_eq!(parsed.ai.provider.as_deref(), Some("agy"));
    assert_eq!(parsed.ai.model.as_deref(), Some("gemini-4-hydrogen-high"));
    assert_eq!(parsed.ai.effort.as_deref(), Some("medium"));
    assert!(parsed.ignore.patterns.iter().any(|p| p == "/src/app.rs"));
}

#[test]
fn test_svn_externals_uncommitted_diff_included() {
    let tmp = TempDir::new().unwrap();
    let main_repo = tmp.path().join("main_repo");
    let ext_repo = tmp.path().join("ext_repo");
    let ext_wc = tmp.path().join("ext_wc");
    let main_wc = tmp.path().join("main_wc");

    fs::create_dir_all(&main_repo).unwrap();
    fs::create_dir_all(&ext_repo).unwrap();

    run("svnadmin", &["create", main_repo.to_str().unwrap()], tmp.path());
    run("svnadmin", &["create", ext_repo.to_str().unwrap()], tmp.path());

    let main_url = format!("file://{}", main_repo.display());
    let ext_url = format!("file://{}", ext_repo.display());

    // Populate external repo
    run("svn", &["checkout", &ext_url, ext_wc.to_str().unwrap()], tmp.path());
    fs::write(ext_wc.join("ext_file.lua"), "return 1\n").unwrap();
    run("svn", &["add", "ext_file.lua"], &ext_wc);
    run("svn", &["commit", "-m", "ext r1"], &ext_wc);

    // Populate main repo with Assets directory and svn:externals pointing to ext_repo
    run("svn", &["checkout", &main_url, main_wc.to_str().unwrap()], tmp.path());
    fs::create_dir_all(main_wc.join("Assets")).unwrap();
    fs::write(main_wc.join("main.lua"), "return 2\n").unwrap();
    run("svn", &["add", "Assets", "main.lua"], &main_wc);
    run("svn", &["propset", "svn:externals", &format!("ExtRes {}\n", ext_url), "Assets"], &main_wc);
    run("svn", &["commit", "-m", "main r1 with externals"], &main_wc);
    run("svn", &["update"], &main_wc);

    // Modify both main.lua and external file Assets/ExtRes/ext_file.lua
    fs::write(main_wc.join("main.lua"), "return 20\n").unwrap();
    fs::write(main_wc.join("Assets/ExtRes/ext_file.lua"), "return 100\n").unwrap();

    let backend = vcs::open_vcs(&main_wc, Some(VcsKind::Svn)).unwrap();
    let bundle = backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    let paths: Vec<_> = bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, vec!["Assets/ExtRes/ext_file.lua", "main.lua"]);
}

#[test]
fn test_commit_confirm_modal_and_svn_git_commit() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wc_dir = tmp.path().join("wc");
    fs::create_dir_all(&repo_dir).unwrap();

    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());

    fs::write(wc_dir.join("a.lua"), "return 1\n").unwrap();
    fs::write(wc_dir.join("b.lua"), "return 2\n").unwrap();
    run("svn", &["add", "a.lua", "b.lua"], &wc_dir);
    run("svn", &["commit", "-m", "r1: init"], &wc_dir);

    fs::write(wc_dir.join("a.lua"), "return 10\n").unwrap();
    fs::write(wc_dir.join("b.lua"), "return 20\n").unwrap();

    let mock_ai = tmp.path().join("mock-commit-ai.sh");
    fs::write(
        &mock_ai,
        r#"#!/bin/sh
cat >/dev/null
echo "更新 a.lua 返回值"
echo ""
echo "- 调整 a.lua 逻辑为返回 10"
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&mock_ai).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mock_ai, perms).unwrap();

    let mut cfg = AppConfig::default();
    cfg.ai_provider = "mock-commit".to_string();
    cfg.cache_ai_results = false;
    cfg.custom_ai.push(CustomAiConfig {
        name: "mock-commit".to_string(),
        command: mock_ai.to_string_lossy().to_string(),
        args: vec![],
    });

    let backend = vcs::open_vcs(&wc_dir, Some(VcsKind::Svn)).unwrap();
    let mut app = App::new(backend, cfg, DiffTarget::Uncommitted, 120).unwrap();

    // Wait for initial AI review thread (optional, ok if fallback)
    for _ in 0..20 {
        app.poll_background();
        if !app.ai_running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    // Seed two AI semantic groups so we can verify group-level commit selection
    app.ai_review = Some(codeduck::model::AiReviewResult {
        provider_name: "mock".to_string(),
        summary: "two groups".to_string(),
        overall_risk: codeduck::model::RiskLevel::Low,
        review_strategy: String::new(),
        groups: vec![
            codeduck::model::ReviewGroup {
                id: "g1".to_string(),
                title: "逻辑分组A".to_string(),
                description: "更新 a.lua".to_string(),
                category: "feature".to_string(),
                risk_level: codeduck::model::RiskLevel::Low,
                review_order: 1,
                files: vec!["a.lua".to_string()],
                key_points: vec![],
            },
            codeduck::model::ReviewGroup {
                id: "g2".to_string(),
                title: "逻辑分组B".to_string(),
                description: "更新 b.lua".to_string(),
                category: "refactor".to_string(),
                risk_level: codeduck::model::RiskLevel::Low,
                review_order: 2,
                files: vec!["b.lua".to_string()],
                key_points: vec![],
            },
        ],
        files: vec![],
        hunks: vec![],
        findings: vec![],
    });

    // Press 'S' to open CommitConfirm modal
    app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::CommitConfirm);
    assert_eq!(app.commit_files.len(), 2);
    assert_eq!(app.commit_groups.len(), 2);
    // rows: [GroupHeader(0), File(0), GroupHeader(1), File(1)]
    assert_eq!(app.commit_rows.len(), 4);

    // Wait for AI commit message generation and confirm single-sentence normalization
    for _ in 0..30 {
        app.poll_background();
        if !app.commit_msg_generating {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert_eq!(app.commit_message, "更新 a.lua 返回值");
    assert!(!app.commit_message.contains('\n'));

    // Move to GroupHeader(1) ("逻辑分组B") and uncheck the entire group with Space
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(app.commit_files[0].included);
    assert!(!app.commit_files[1].included);
    // Fallback updates immediately to a single-sentence English summary without file counts
    assert!(!app.commit_message.contains('\n'));
    assert!(!app.commit_message.contains("个文件"));
    assert!(app.commit_message.is_ascii());
    // Wait briefly past the 350ms debounce and verify auto-regeneration starts automatically without pressing 'g'
    std::thread::sleep(std::time::Duration::from_millis(380));
    app.poll_background();
    assert!(app.commit_msg_generating || !app.commit_msg_needs_regen);
    for _ in 0..30 {
        app.poll_background();
        if !app.commit_msg_generating {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    // Toggle GroupHeader(1) back on then off to verify instant in-memory commit_msg_cache hits
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    std::thread::sleep(std::time::Duration::from_millis(380));
    app.poll_background();
    assert!(!app.commit_msg_generating);
    assert_eq!(app.commit_message, "更新 a.lua 返回值");
    assert!(!app.commit_msg_cache.is_empty());
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    std::thread::sleep(std::time::Duration::from_millis(380));
    app.poll_background();
    assert!(!app.commit_msg_generating);

    // Step 1 'y' opens double confirmation (does NOT commit yet)
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(
        app.overlay,
        codeduck::ui::app::OverlayKind::CommitDoubleConfirm
    );
    assert!(!app.commit_executing);

    // Pressing 'n' goes back to CommitConfirm without committing
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::CommitConfirm);

    // Confirm Step 1 -> CommitDoubleConfirm -> Step 2 waits for updated AI message before committing
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(
        app.overlay,
        codeduck::ui::app::OverlayKind::CommitDoubleConfirm
    );
    // Pressing 'y' while AI is regenerating the commit message is safely guarded
    if app.commit_msg_generating {
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(
            app.overlay,
            codeduck::ui::app::OverlayKind::CommitDoubleConfirm
        );
        for _ in 0..30 {
            app.poll_background();
            if !app.commit_msg_generating {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    for _ in 0..40 {
        app.poll_background();
        if !app.commit_executing && !app.diff_loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::None);
    // Only b.lua should remain uncommitted!
    let remaining: Vec<_> = app.bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(remaining, vec!["b.lua"]);

    // Verify SVN log recorded a single-line commit message
    let commits = app.vcs.recent_commits(1).unwrap();
    assert_eq!(commits[0].revision, "2");
    assert!(!commits[0].message.contains('\n'));

    // Verify GitBackend::commit_files too
    let git_dir = tmp.path().join("git_commit_repo");
    fs::create_dir_all(&git_dir).unwrap();
    run("git", &["init"], &git_dir);
    run("git", &["config", "user.email", "test@example.com"], &git_dir);
    run("git", &["config", "user.name", "Test"], &git_dir);
    fs::write(git_dir.join("x.txt"), "1\n").unwrap();
    run("git", &["add", "x.txt"], &git_dir);
    run("git", &["commit", "-m", "init"], &git_dir);
    fs::write(git_dir.join("x.txt"), "2\n").unwrap();

    let git_backend = vcs::open_vcs(&git_dir, Some(VcsKind::Git)).unwrap();
    git_backend
        .commit_files(&["x.txt".to_string()], "feat: update x.txt")
        .unwrap();
    let git_commits = git_backend.recent_commits(1).unwrap();
    assert_eq!(git_commits[0].message, "feat: update x.txt");
}

#[test]
fn test_ai_session_persistence_and_reuse() {
    use codeduck::ai::AiProvider;
    use std::sync::{Arc, Mutex};

    struct MockSessionProvider {
        calls: Arc<Mutex<Vec<(Option<String>, bool, bool)>>>,
        fail_on_stale: bool,
    }

    impl ai::AiProvider for MockSessionProvider {
        fn id(&self) -> &str {
            "mock-sess"
        }
        fn display_name(&self) -> &str {
            "Mock Session Provider"
        }
        fn run_prompt_with_session(
            &self,
            _prompt: &str,
            _work_dir: &std::path::Path,
            json_mode: bool,
            session_id: Option<&str>,
            persist_session: bool,
        ) -> anyhow::Result<ai::AiPromptOutput> {
            self.calls.lock().unwrap().push((
                session_id.map(str::to_string),
                persist_session,
                json_mode,
            ));
            if self.fail_on_stale && session_id == Some("stale-session-id") {
                anyhow::bail!("session not found: stale-session-id");
            }
            if json_mode {
                let sid = match session_id {
                    None => "sess-init-001",
                    Some(_) => "sess-init-002",
                };
                return Ok(ai::AiPromptOutput {
                    text: r#"{
                        "summary": "Mock review summary",
                        "overall_risk": "low",
                        "groups": [{"id":"g1","title":"Core","category":"logic","risk_level":"low","review_order":1,"description":"desc","files":["a.lua"]}],
                        "files": [{"path":"a.lua","category":"logic","risk_level":"low","summary":"ok","review_focus":["check"]}],
                        "hunks": [],
                        "findings": []
                    }"#
                    .to_string(),
                    session_id: Some(sid.to_string()),
                });
            }
            let next_sid = match session_id {
                Some("sess-init-001") => "sess-followup-002",
                Some("sess-followup-002") => "sess-commit-003",
                None => "sess-fresh-fallback",
                Some(other) => other,
            };
            Ok(ai::AiPromptOutput {
                text: format!("mock-answer-for-{:?}", session_id),
                session_id: Some(next_sid.to_string()),
            })
        }
    }

    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wc_dir = tmp.path().join("wc");
    fs::create_dir_all(&repo_dir).unwrap();
    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());

    fs::write(wc_dir.join("a.lua"), "return 1\n").unwrap();
    run("svn", &["add", "a.lua"], &wc_dir);
    run("svn", &["commit", "-m", "init"], &wc_dir);
    fs::write(wc_dir.join("a.lua"), "return 2\n").unwrap();

    // Write project .codeduck with comments & ignore rules
    let initial_toml = r#"# project config header
[ai]
provider = "mock-sess"
model = "test-model"
reuse_session = true

[ignore]
patterns = ["*.meta"]
"#;
    fs::write(wc_dir.join(".codeduck"), initial_toml).unwrap();

    let backend = vcs::open_vcs(&wc_dir, Some(VcsKind::Svn)).unwrap();
    let bundle = backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    let engine = DifftEngine::new("difft", false).unwrap();
    let rendered = vec![engine
        .render_file(&bundle.changes[0], DifftDisplayMode::SideBySide, 120)
        .unwrap()];

    let sessions_file = tmp.path().join("codeduck_home").join("sessions.toml");

    let mut cfg = AppConfig::default();
    cfg.cache_ai_results = false;
    cfg.apply_project_overrides_with_sessions(
        backend.repo_root(),
        backend.scope_dir(),
        Some(&sessions_file),
    );
    assert!(cfg.reuse_session);
    assert!(cfg.ai_sessions.is_empty());

    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider = MockSessionProvider {
        calls: calls.clone(),
        fail_on_stale: true,
    };

    // 1. Initial analysis gets new session ID
    let out1 = ai::analyze_diff_with_session(&provider, &bundle, &rendered, &cfg, false).unwrap();
    assert_eq!(out1.session_id.as_deref(), Some("sess-init-001"));

    // 2. Persist centrally into sessions.toml and verify project `.codeduck` stays untouched
    codeduck::config::save_project_session_at(
        backend.repo_root(),
        backend.scope_dir(),
        provider.id(),
        out1.session_id.as_deref(),
        Some(&sessions_file),
    )
    .unwrap();
    let project_toml_after = fs::read_to_string(wc_dir.join(".codeduck")).unwrap();
    assert_eq!(project_toml_after, initial_toml);
    let saved_sessions_raw = fs::read_to_string(&sessions_file).unwrap();
    assert!(saved_sessions_raw.contains("sess-init-001"));

    // 3. Simulate restarting codeduck: reload project overrides + sessions.toml and verify session loaded
    let mut cfg2 = AppConfig::default();
    cfg2.cache_ai_results = false;
    cfg2.apply_project_overrides_with_sessions(
        backend.repo_root(),
        backend.scope_dir(),
        Some(&sessions_file),
    );
    assert_eq!(
        cfg2.ai_sessions.get("mock-sess").map(String::as_str),
        Some("sess-init-001")
    );

    // 4. Followup reuses saved session and advances to sess-followup-002
    let ans = ai::ask_followup(&provider, &bundle, Some(&rendered[0]), "why?", &cfg2).unwrap();
    assert_eq!(ans.session_id.as_deref(), Some("sess-followup-002"));
    cfg2.ai_sessions
        .insert("mock-sess".to_string(), ans.session_id.clone().unwrap());

    // 5. Commit message reuses sess-followup-002 -> sess-commit-003
    let cmsg = ai::generate_commit_message(
        &provider,
        &bundle,
        &rendered,
        &["a.lua".to_string()],
        Some(&out1.review),
        &cfg2,
    )
    .unwrap();
    assert_eq!(cmsg.session_id.as_deref(), Some("sess-commit-003"));

    // 6. Stale session automatically falls back to fresh call (session_id=None)
    cfg2.ai_sessions
        .insert("mock-sess".to_string(), "stale-session-id".to_string());
    let fallback_ans =
        ai::ask_followup(&provider, &bundle, Some(&rendered[0]), "retry?", &cfg2).unwrap();
    assert_eq!(fallback_ans.session_id.as_deref(), Some("sess-fresh-fallback"));

    // 7. reuse_session = false passes (None, false)
    cfg2.reuse_session = false;
    let no_reuse =
        ai::ask_followup(&provider, &bundle, Some(&rendered[0]), "fresh?", &cfg2).unwrap();
    assert!(no_reuse.text.contains("None"));

    // 8. Clear session in unified sessions.toml
    codeduck::config::save_project_session_at(
        backend.repo_root(),
        backend.scope_dir(),
        provider.id(),
        None,
        Some(&sessions_file),
    )
    .unwrap();
    let mut cfg3 = AppConfig::default();
    cfg3.apply_project_overrides_with_sessions(
        backend.repo_root(),
        backend.scope_dir(),
        Some(&sessions_file),
    );
    assert!(!cfg3.ai_sessions.contains_key("mock-sess"));

    // 9. Verify AppConfig::sessions_path() resolves under ~/.codeduck/sessions.toml
    if std::env::var_os("CODEDUCK_HOME").is_none() {
        let home = AppConfig::codeduck_home_dir().unwrap();
        assert_eq!(AppConfig::sessions_path().unwrap(), home.join("sessions.toml"));
    }

    let recorded = calls.lock().unwrap();
    assert_eq!(recorded[0], (None, true, true));
    assert_eq!(recorded[1], (Some("sess-init-001".to_string()), true, false));
    assert_eq!(recorded[2], (Some("sess-followup-002".to_string()), true, false));
    assert_eq!(recorded[3], (Some("stale-session-id".to_string()), true, false));
    assert_eq!(recorded[4], (None, true, false));
    assert_eq!(recorded[5], (None, false, false));
    drop(recorded);

    // 10. Two-stage review runs Stage 1 (groups+summary only, no tools) then Stage 2 (findings+files+hunks)
    struct TwoStageMockProvider {
        stage_prompts: Arc<Mutex<Vec<String>>>,
    }
    impl ai::AiProvider for TwoStageMockProvider {
        fn id(&self) -> &str {
            "two-stage-mock"
        }
        fn display_name(&self) -> &str {
            "Two-Stage Mock"
        }
        fn run_prompt_with_session(
            &self,
            prompt: &str,
            _work_dir: &std::path::Path,
            _json_mode: bool,
            session_id: Option<&str>,
            _persist_session: bool,
        ) -> anyhow::Result<ai::AiPromptOutput> {
            self.stage_prompts.lock().unwrap().push(prompt.to_string());
            if prompt.contains("Stage 1 (Fast Grouping & Summary)") {
                Ok(ai::AiPromptOutput {
                    text: r#"{
                        "summary": "Stage 1 summary",
                        "overall_risk": "low",
                        "review_strategy": "Check g1 first",
                        "groups": [{"id":"g1","title":"Fast Group","category":"logic","risk_level":"low","review_order":1,"description":"desc","files":["a.lua"]}]
                    }"#
                    .to_string(),
                    session_id: Some("ts-sess-1".to_string()),
                })
            } else {
                assert_eq!(session_id, None);
                Ok(ai::AiPromptOutput {
                    text: r#"{
                        "overall_risk": "high",
                        "files": [{"path":"a.lua","category":"logic","risk_level":"high","summary":"file note","review_focus":["edge"]}],
                        "hunks": [{"file":"a.lua","chunk_index":1,"start_line":1,"end_line":1,"title":"Return change","analysis":"Verify return value","risk_level":"high"}],
                        "findings": [{"id":"f1","file":"a.lua","line":1,"severity":"warning","title":"Changed return value","description":"Check callers","suggestion":"Add test"}]
                    }"#
                    .to_string(),
                    session_id: Some("ts-sess-2".to_string()),
                })
            }
        }
    }
    let two_prompts = Arc::new(Mutex::new(Vec::new()));
    let two_prov = TwoStageMockProvider {
        stage_prompts: two_prompts.clone(),
    };
    let mut cfg_ts = AppConfig::default();
    cfg_ts.cache_ai_results = false;
    cfg_ts.reuse_session = true;
    cfg_ts.ai_fast_model = Some("gemini-4-hydrogen-low".to_string());
    cfg_ts.ai_fast_effort = None;
    cfg_ts
        .ai_sessions
        .insert("two-stage-mock".to_string(), "ts-stale".to_string());
    let fast_cfg = cfg_ts.fast_config();
    assert_eq!(fast_cfg.ai_model.as_deref(), Some("gemini-4-hydrogen-low"));
    assert_eq!(fast_cfg.ai_effort.as_deref(), Some("low"));
    assert!(!fast_cfg.reuse_session);
    assert!(fast_cfg.ai_sessions.is_empty());

    let mut early_groups: Option<Vec<String>> = None;
    let mut early_findings_empty = false;
    let ts_out = ai::analyze_diff_two_stage(&two_prov, &two_prov, &bundle, &rendered, &cfg_ts, true, |s1, sid| {
        early_groups = Some(s1.groups.iter().map(|g| g.title.clone()).collect());
        early_findings_empty = s1.findings.is_empty();
        assert_eq!(sid, None);
    })
    .unwrap();
    assert_eq!(early_groups, Some(vec!["Fast Group".to_string()]));
    assert!(early_findings_empty);
    assert_eq!(ts_out.review.summary, "Stage 1 summary");
    assert_eq!(ts_out.review.groups.len(), 1);
    assert_eq!(ts_out.review.overall_risk, codeduck::model::RiskLevel::High);
    assert_eq!(ts_out.review.findings.len(), 1);
    assert_eq!(ts_out.review.files.len(), 1);
    assert_eq!(ts_out.review.hunks.len(), 1);
    assert_eq!(ts_out.session_id.as_deref(), Some("ts-sess-2"));
}

#[test]
fn test_history_and_commit_diff_disk_cache() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wc_dir = tmp.path().join("wc");
    fs::create_dir_all(&repo_dir).unwrap();
    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());

    fs::write(wc_dir.join("a.lua"), "return 1\n").unwrap();
    run("svn", &["add", "a.lua"], &wc_dir);
    run("svn", &["commit", "-m", "r1"], &wc_dir);

    fs::write(wc_dir.join("a.lua"), "return 2\n").unwrap();
    run("svn", &["commit", "-m", "r2"], &wc_dir);

    fs::write(
        wc_dir.join(".codeduck"),
        "[ai]\nprovider = \"none\"\n[diff]\ncache_history = true\n[ignore]\npatterns = [\"*.meta\"]\n",
    )
    .unwrap();

    let backend = vcs::open_vcs(&wc_dir, Some(VcsKind::Svn)).unwrap();
    let commits1 = vcs::recent_commits_cached(&*backend, 10, true, false).unwrap();
    let revs1: Vec<_> = commits1.iter().map(|c| c.revision.as_str()).collect();
    assert_eq!(revs1, vec!["2", "1"]);

    // Disk cache is immediately readable without hitting SVN
    let disk_cached = vcs::load_history_disk_cache(&*backend, 10).unwrap();
    assert_eq!(disk_cached.len(), 2);

    // Add r3 with both a .lua file and an ignored .meta file
    fs::write(wc_dir.join("a.lua"), "return 3\n").unwrap();
    fs::write(wc_dir.join("skip.meta"), "ignored\n").unwrap();
    run("svn", &["add", "skip.meta"], &wc_dir);
    run("svn", &["commit", "-m", "r3"], &wc_dir);

    // Incremental cache update picks up r3 automatically
    let commits2 = vcs::recent_commits_cached(&*backend, 10, true, false).unwrap();
    let revs2: Vec<_> = commits2.iter().map(|c| c.revision.as_str()).collect();
    assert_eq!(revs2, vec!["3", "2", "1"]);

    // Immutable commit r3 diff cache respects ignore rules & invalidates if .codeduck ignore changes
    let matcher1 = codeduck::ignore::IgnoreMatcher::load(backend.repo_root(), backend.scope_dir());
    let b1 = vcs::collect_changes_cached(
        &*backend,
        &DiffTarget::Commit("3".to_string()),
        Some(&matcher1),
        true,
        false,
    )
    .unwrap();
    let p1: Vec<_> = b1.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(p1, vec!["a.lua"]);

    // Add *.lua to ignore -> fingerprint changes -> cache misses & filters out a.lua
    fs::write(
        wc_dir.join(".codeduck"),
        "[ai]\nprovider = \"none\"\n[diff]\ncache_history = true\n[ignore]\npatterns = [\"*.meta\", \"*.lua\"]\n",
    )
    .unwrap();
    let matcher2 = codeduck::ignore::IgnoreMatcher::load(backend.repo_root(), backend.scope_dir());
    let b2 = vcs::collect_changes_cached(
        &*backend,
        &DiffTarget::Commit("3".to_string()),
        Some(&matcher2),
        true,
        false,
    )
    .unwrap();
    assert!(b2.changes.is_empty());

    // Uncommitted diff is never stale-cached
    fs::write(wc_dir.join("b.txt"), "one\n").unwrap();
    run("svn", &["add", "b.txt"], &wc_dir);
    let u1 = vcs::collect_changes_cached(&*backend, &DiffTarget::Uncommitted, None, true, false)
        .unwrap();
    assert_eq!(u1.changes[0].new_content.as_deref(), Some(b"one\n".as_slice()));
    fs::write(wc_dir.join("b.txt"), "two\n").unwrap();
    let u2 = vcs::collect_changes_cached(&*backend, &DiffTarget::Uncommitted, None, true, false)
        .unwrap();
    assert_eq!(u2.changes[0].new_content.as_deref(), Some(b"two\n".as_slice()));
}

#[test]
fn test_audit_regressions_edge_cases_and_ui_rendering() {
    use codeduck::difft::{extract_hunk_anchors, parse_source_line_from_ansi};
    use codeduck::model::{ChangeKind, FileChange};
    use codeduck::ui::{self, text_input};
    use crossterm::event::KeyEventKind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // 1. TextInput cursor, UTF-8 / CJK, Ctrl shortcuts, and paste sanitization
    let mut buf = String::new();
    let mut cur = 0usize;
    text_input::insert_clean_str(&mut buf, &mut cur, "修复\n登录");
    assert_eq!(buf, "修复 登录");
    assert!(text_input::handle_key(
        &mut buf,
        &mut cur,
        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)
    ) == false);
    assert!(text_input::handle_key(
        &mut buf,
        &mut cur,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
    ));
    assert_eq!(buf, "修复 录");
    text_input::handle_key(
        &mut buf,
        &mut cur,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
    );
    assert_eq!(cur, 0);
    text_input::handle_key(
        &mut buf,
        &mut cur,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
    );
    assert_eq!(buf, "X修复 录");

    // 2. extract_json_block with nested code fences inside JSON strings
    let nested = r#"Here is the analysis:
```JSON
{"summary": "Example:\n```lua\nreturn 0\n```", "overall_risk": "low", "groups": [], "files": [], "hunks": [], "findings": []}
```"#;
    let extracted = ai::extract_json_block(nested).unwrap();
    let v: serde_json::Value = serde_json::from_str(extracted).unwrap();
    assert_eq!(v["overall_risk"], "low");

    // 3. ANSI hunk anchors (compound SGR) and line-number parser ignoring code literals
    let ansi = "\x1b[1;91m   12   return 0;\x1b[0m\n   13   x = 42\n";
    assert_eq!(extract_hunk_anchors(ansi), vec![0]);
    assert_eq!(parse_source_line_from_ansi("   12   return 0;"), Some(12));
    assert_eq!(parse_source_line_from_ansi("        return 0;"), None);

    // 4. DifftEngine unique snapshot isolation (case collision + file/dir replacement)
    let engine = DifftEngine::new("difft", false).unwrap();
    let changes = vec![
        FileChange {
            path: "Foo.rs".to_string(),
            old_path: None,
            kind: ChangeKind::Deleted,
            is_binary: false,
            old_content: Some(b"fn a() {}\n".to_vec()),
            new_content: None,
            old_size: Some(10),
            new_size: None,
            prop_changes: None,
        },
        FileChange {
            path: "foo.rs".to_string(),
            old_path: None,
            kind: ChangeKind::Added,
            is_binary: false,
            old_content: None,
            new_content: Some(b"fn b() {}\n".to_vec()),
            old_size: None,
            new_size: Some(10),
            prop_changes: None,
        },
        FileChange {
            path: "foo.rs/nested.rs".to_string(),
            old_path: None,
            kind: ChangeKind::Added,
            is_binary: false,
            old_content: None,
            new_content: Some(b"fn c() {}\n".to_vec()),
            old_size: None,
            new_size: Some(10),
            prop_changes: None,
        },
    ];
    let rendered = engine
        .render_changes_bounded(&changes, DifftDisplayMode::SideBySide, 100)
        .unwrap();
    assert_eq!(rendered.len(), 3);

    // 5. SVN `@` peg filenames, Chinese URL-encoded history paths, and partial external commit
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("svn_repo");
    let ext_repo = tmp.path().join("ext_repo");
    let wc_dir = tmp.path().join("wc");
    let ext_wc = tmp.path().join("ext_wc");
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(&ext_repo).unwrap();
    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    run("svnadmin", &["create", ext_repo.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    let ext_url = format!("file://{}", ext_repo.display());

    run("svn", &["checkout", &ext_url, ext_wc.to_str().unwrap()], tmp.path());
    fs::write(ext_wc.join("ext.lua"), "return 1\n").unwrap();
    run("svn", &["add", "ext.lua"], &ext_wc);
    run("svn", &["commit", "-m", "ext init"], &ext_wc);

    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());
    fs::create_dir_all(wc_dir.join("ExtDir")).unwrap();
    fs::write(wc_dir.join("btn@2x.png.txt"), "v1\n").unwrap();
    fs::write(wc_dir.join("中文 路径.lua"), "return 1\n").unwrap();
    run("svn", &["add", "ExtDir", "btn@2x.png.txt@", "中文 路径.lua"], &wc_dir);
    run(
        "svn",
        &["propset", "svn:externals", &format!("Sub {}\n", ext_url), "ExtDir"],
        &wc_dir,
    );
    run("svn", &["commit", "-m", "init special paths"], &wc_dir);
    run("svn", &["update"], &wc_dir);

    // Modify @ file + Chinese file (r2) to check history URL decoding
    fs::write(wc_dir.join("btn@2x.png.txt"), "v2\n").unwrap();
    fs::write(wc_dir.join("中文 路径.lua"), "return 2\n").unwrap();
    run("svn", &["commit", "-m", "update special paths"], &wc_dir);

    let svn_backend = vcs::open_vcs(&wc_dir, Some(VcsKind::Svn)).unwrap();
    let r2_bundle = svn_backend
        .collect_changes(&DiffTarget::Commit("2".to_string()))
        .unwrap();
    let r2_paths: Vec<_> = r2_bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(r2_paths, vec!["btn@2x.png.txt", "中文 路径.lua"]);

    // Uncommitted @ file edit works cleanly and commits via commit_files_detailed
    fs::write(wc_dir.join("btn@2x.png.txt"), "v3\n").unwrap();
    let outcome = svn_backend.commit_files_detailed(&["btn@2x.png.txt".to_string()], "更新图标");
    assert!(outcome.error.is_none(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.committed_paths, vec!["btn@2x.png.txt".to_string()]);

    // 6. Git `-uall` untracked directory & Chinese paths & Rename commit
    let git_dir = tmp.path().join("git_special");
    fs::create_dir_all(&git_dir).unwrap();
    run("git", &["init"], &git_dir);
    run("git", &["config", "user.email", "t@example.com"], &git_dir);
    run("git", &["config", "user.name", "Test"], &git_dir);
    fs::write(git_dir.join("old.lua"), "print('hi')\n").unwrap();
    run("git", &["add", "old.lua"], &git_dir);
    run("git", &["commit", "-m", "init"], &git_dir);

    fs::create_dir_all(git_dir.join("new_dir")).unwrap();
    fs::write(git_dir.join("new_dir/中文.lua"), "print('zh')\n").unwrap();
    run("git", &["mv", "old.lua", "renamed.lua"], &git_dir);

    let git_backend = vcs::open_vcs(&git_dir, Some(VcsKind::Git)).unwrap();
    let g_bundle = git_backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    let g_paths: Vec<_> = g_bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert!(g_paths.contains(&"new_dir/中文.lua"));
    assert!(g_paths.contains(&"renamed.lua"));
    git_backend
        .commit_files(&["renamed.lua".to_string(), "new_dir/中文.lua".to_string()], "重命名与新增目录")
        .unwrap();

    // 7. TUI KeyEventKind::Release guard + Search first-match + TestBackend rendering at normal & narrow sizes
    fs::write(git_dir.join("renamed.lua"), "print('first')\nprint('second')\n").unwrap();
    let mut cfg = AppConfig::default();
    cfg.ai_provider = "none".to_string();
    let mut app = App::new(git_backend, cfg, DiffTarget::Uncommitted, 120).unwrap();

    // Key Release must NOT trigger 'q' quit or double actions
    let mut rel_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    rel_q.kind = KeyEventKind::Release;
    app.handle_key(rel_q);
    assert!(!app.should_quit);

    // Search '/' jumps to Match 1 (not Match 2)
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    for ch in "print".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.status_message.starts_with("Match 1/"), "got {}", app.status_message);

    // Pressing 'R' refreshes uncommitted changes from disk
    fs::write(git_dir.join("added_by_refresh.lua"), "return 42\n").unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
    for _ in 0..40 {
        app.poll_background();
        if !app.diff_loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert!(app.path_to_index.contains_key("added_by_refresh.lua"));

    // TestBackend render passes cleanly at both 120x35 and narrow 60x20 across overlays (including live AI progress)
    app.ai_running = true;
    app.ai_started_at = Some(std::time::Instant::now());
    app.ai_progress_stage = "模型思考规划中...".to_string();
    app.ai_stream_logs = vec!["[0.2s] 构建提示词".to_string(), "[1.0s] 已连接 agy".to_string()];
    app.ai_stream_thinking = "分析核心改动边界条件".to_string();
    app.ai_stream_text = "{\"summary\":\"正在生成\"}".to_string();
    for (w, h) in [(120u16, 35u16), (60u16, 20u16)] {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| ui::draw(f, &mut app)).unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
        app.commit_msg_generating = true;
        app.commit_started_at = Some(std::time::Instant::now());
        app.commit_progress_stage = "模型正在生成英文日志...".to_string();
        app.commit_stream_logs = vec!["[0.3s] 已连接 agy".to_string()];
        app.commit_stream_thinking = "Summarizing selected files".to_string();
        app.commit_stream_text = "Initialize codeduck repository".to_string();
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        app.commit_msg_generating = false;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }
    app.ai_running = false;
}

#[test]
fn test_global_configs_toml_fallback_and_codeduck_home() {
    let tmp = TempDir::new().unwrap();
    let home_dir = tmp.path().join("docs_codeduck");
    fs::create_dir_all(&home_dir).unwrap();
    let global_cfg = home_dir.join("configs.toml");
    fs::write(
        &global_cfg,
        r#"
[ai]
provider = "codex"
model = "global-model"
effort = "high"
language = "en-US"
timeout_secs = 120

[diff]
display_mode = "inline"
cache_history = false

[ignore]
patterns = ["*.meta", "Library/"]
"#,
    )
    .unwrap();

    // Load global configs.toml directly, then merge partial project .codeduck
    let mut cfg = AppConfig::load(Some(&global_cfg)).unwrap();
    assert_eq!(cfg.ai_provider, "codex");
    assert_eq!(cfg.ai_model.as_deref(), Some("global-model"));
    assert_eq!(cfg.ai_effort.as_deref(), Some("high"));
    assert_eq!(cfg.review_language, "en-US");
    assert_eq!(cfg.ai_timeout_secs, 120);
    assert_eq!(cfg.display_mode, DifftDisplayMode::Inline);
    assert!(!cfg.cache_history);

    let proj_dir = tmp.path().join("proj");
    fs::create_dir_all(&proj_dir).unwrap();
    // Project only overrides model; everything else falls back to configs.toml!
    fs::write(
        proj_dir.join(".codeduck"),
        r#"
[ai]
model = "proj-model"

[ignore]
patterns = ["!keep.meta"]
"#,
    )
    .unwrap();

    cfg.apply_project_overrides(&proj_dir, &proj_dir);
    assert_eq!(cfg.ai_provider, "codex");
    assert_eq!(cfg.ai_model.as_deref(), Some("proj-model"));
    assert_eq!(cfg.ai_effort.as_deref(), Some("high"));
    assert_eq!(cfg.review_language, "en-US");
    assert_eq!(cfg.ai_timeout_secs, 120);
    assert_eq!(cfg.display_mode, DifftDisplayMode::Inline);
    assert!(!cfg.cache_history);

    // Ignore rules fall back to configs.toml, while project `.codeduck` `!keep.meta` overrides it
    let matcher = codeduck::ignore::IgnoreMatcher::load_with_global(
        &proj_dir,
        &proj_dir,
        Some(&global_cfg),
    );
    assert!(matcher.is_ignored(&proj_dir, "skip.meta"));
    assert!(matcher.is_ignored(&proj_dir, "Library/temp.dat"));
    assert!(!matcher.is_ignored(&proj_dir, "keep.meta"));
    assert!(!matcher.is_ignored(&proj_dir, "src/main.rs"));

    // Verify default codeduck_home_dir resolves to ~/.codeduck and cache_dir under it/cache
    if std::env::var_os("CODEDUCK_HOME").is_none() {
        let home = AppConfig::codeduck_home_dir().unwrap();
        let expected_home = dirs::home_dir().unwrap().join(".codeduck");
        assert_eq!(home, expected_home);
        assert_eq!(AppConfig::config_path().unwrap(), home.join("configs.toml"));
        assert_eq!(AppConfig::cache_dir().unwrap(), home.join("cache"));
    }
}

#[test]
fn test_security_and_state_machine_regressions() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let tmp = TempDir::new().unwrap();

    // 1. Security boundary: project `.codeduck` cannot override `difft_bin` or inject `custom_ai`,
    //    switching providers clears cross-provider model/fast_model, and empty strings clear overrides.
    let home_dir = tmp.path().join("home_cfg");
    fs::create_dir_all(&home_dir).unwrap();
    let global_cfg = home_dir.join("configs.toml");
    fs::write(
        &global_cfg,
        r#"
[ai]
provider = "agy"
model = "gemini-4-hydrogen-medium"
fast_model = "gemini-4-hydrogen-low"
effort = "medium"

[diff]
difft_bin = "difft"
"#,
    )
    .unwrap();

    let untrusted_proj = tmp.path().join("untrusted_proj");
    fs::create_dir_all(&untrusted_proj).unwrap();
    fs::write(
        untrusted_proj.join(".codeduck"),
        r#"
[ai]
provider = "codex"
fast_model = "none"

[diff]
difft_bin = "/tmp/evil_bin"

[[custom_ai]]
name = "evil"
command = "sh"
args = ["-c", "echo pwned"]
"#,
    )
    .unwrap();

    let mut sec_cfg = AppConfig::load(Some(&global_cfg)).unwrap();
    sec_cfg.apply_project_overrides(&untrusted_proj, &untrusted_proj);
    assert_eq!(sec_cfg.difft_bin, "difft");
    assert!(sec_cfg.custom_ai.is_empty());
    assert_eq!(sec_cfg.ai_provider, "codex");
    // Cross-provider model was cleared automatically on provider change
    assert_eq!(sec_cfg.ai_model, None);
    assert_eq!(sec_cfg.ai_fast_model, None);

    assert!(codeduck::config::validate_effort("xhigh").is_ok());
    assert!(codeduck::config::validate_effort("bogus").is_err());
    assert!(codeduck::config::validate_cli_value("model", "--evil-flag").is_err());

    // 2. parse_ai_review_json rejects empty `{}` and skips stray non-review JSON prefix
    assert!(ai::parse_ai_review_json("{}", "mock", &[]).is_err());
    let stray_plus_review = r#"Note: {"k": 1}

{
  "summary": "Valid review summary",
  "overall_risk": "low",
  "groups": [{"id":"g1","title":"Group1","files":["a.rs"]}]
}"#;
    let parsed_review = ai::parse_ai_review_json(stray_plus_review, "mock", &[]).unwrap();
    assert_eq!(parsed_review.summary, "Valid review summary");
    assert_eq!(parsed_review.groups.len(), 1);

    // 3. SVN PropertyOnly directory commit uses --depth empty and does not commit unchecked child files
    let svn_repo = tmp.path().join("svn_prop_repo");
    let svn_wc = tmp.path().join("svn_prop_wc");
    fs::create_dir_all(&svn_repo).unwrap();
    run("svnadmin", &["create", svn_repo.to_str().unwrap()], tmp.path());
    let svn_url = format!("file://{}", svn_repo.display());
    run("svn", &["checkout", &svn_url, svn_wc.to_str().unwrap()], tmp.path());
    fs::create_dir_all(svn_wc.join("dir")).unwrap();
    fs::write(svn_wc.join("dir/child.txt"), "v1\n").unwrap();
    run("svn", &["add", "dir"], &svn_wc);
    run("svn", &["commit", "-m", "r1"], &svn_wc);

    run("svn", &["propset", "custom:tag", "yes", "dir"], &svn_wc);
    fs::write(svn_wc.join("dir/child.txt"), "v2-unchecked\n").unwrap();
    let svn_backend = vcs::open_vcs(&svn_wc, Some(VcsKind::Svn)).unwrap();
    let svn_bundle = svn_backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert!(svn_bundle.changes.iter().any(|c| c.path == "dir" && c.kind == codeduck::model::ChangeKind::PropertyOnly));
    assert!(svn_bundle.changes.iter().any(|c| c.path == "dir/child.txt"));
    svn_backend
        .commit_files(&["dir".to_string()], "Update dir property only")
        .unwrap();
    // Unchecked child.txt MUST still be uncommitted!
    let after_bundle = svn_backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert_eq!(after_bundle.changes.len(), 1);
    assert_eq!(after_bundle.changes[0].path, "dir/child.txt");

    // 4. Git rejects `-` rev option injection and parses `-z` filenames with ` -> ` cleanly
    let git_dir = tmp.path().join("git_edge");
    fs::create_dir_all(&git_dir).unwrap();
    run("git", &["init"], &git_dir);
    run("git", &["config", "user.email", "t@example.com"], &git_dir);
    run("git", &["config", "user.name", "Tester"], &git_dir);
    fs::write(git_dir.join("init.txt"), "base\n").unwrap();
    run("git", &["add", "init.txt"], &git_dir);
    run("git", &["commit", "-m", "init"], &git_dir);
    fs::write(git_dir.join("foo -> bar.txt"), "special\n").unwrap();
    let git_backend = vcs::open_vcs(&git_dir, Some(VcsKind::Git)).unwrap();
    assert!(git_backend.collect_changes(&DiffTarget::Commit("--output=/tmp/x".to_string())).is_err());
    let git_uncomm = git_backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert_eq!(git_uncomm.changes[0].path, "foo -> bar.txt");

    // 5. TUI state machine fixes: same-line `]i`, `:tree` focus, `CommitDoubleConfirm` notice vs error, `P -> Off` clearing `ai_deep_running`
    fs::write(git_dir.join("init.txt"), "base\nupdated\n").unwrap();
    let mut tcfg = AppConfig::default();
    tcfg.ai_provider = "none".to_string();
    let mut app = App::new(git_backend, tcfg, DiffTarget::Uncommitted, 120).unwrap();

    // Focus stays in Diff; Tab/Ctrl-w/H are disabled, `:tree` toggles sidebar visibility while staying in Diff
    assert_eq!(app.focus, codeduck::ui::app::PaneFocus::Diff);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE));
    assert_eq!(app.focus, codeduck::ui::app::PaneFocus::Diff);
    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
    for ch in "tree".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.show_sidebar);
    assert_eq!(app.focus, codeduck::ui::app::PaneFocus::Diff);

    // Show sidebar again, verify `h` at scroll_x=0 keeps focus in Diff,
    // `l` scrolls right in Diff and `h` scrolls back to 0 without leaving Diff
    app.handle_key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::NONE));
    assert!(app.show_sidebar);
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert_eq!(app.focus, codeduck::ui::app::PaneFocus::Diff);
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    assert_eq!(app.diff_scroll_x, 4);
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert_eq!(app.diff_scroll_x, 0);
    assert_eq!(app.focus, codeduck::ui::app::PaneFocus::Diff);
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert_eq!(app.focus, codeduck::ui::app::PaneFocus::Diff);

    // Same-line findings cycle with `]i`
    app.ai_review = Some(codeduck::model::AiReviewResult {
        provider_name: "mock".to_string(),
        summary: "s".to_string(),
        overall_risk: codeduck::model::RiskLevel::Low,
        review_strategy: String::new(),
        groups: vec![codeduck::model::ReviewGroup {
            id: "g1".to_string(),
            title: "G1".to_string(),
            description: String::new(),
            category: "logic".to_string(),
            risk_level: codeduck::model::RiskLevel::Low,
            review_order: 1,
            files: vec!["init.txt".to_string()],
            key_points: vec![],
        }],
        files: vec![],
        hunks: vec![],
        findings: vec![
            codeduck::model::ReviewFinding {
                id: "f1".to_string(),
                file: "init.txt".to_string(),
                line: Some(1),
                severity: codeduck::model::Severity::Warning,
                title: "First finding".to_string(),
                description: String::new(),
                suggestion: None,
            },
            codeduck::model::ReviewFinding {
                id: "f2".to_string(),
                file: "init.txt".to_string(),
                line: Some(1),
                severity: codeduck::model::Severity::Info,
                title: "Second finding".to_string(),
                description: String::new(),
                suggestion: None,
            },
        ],
    });
    app.rebuild_tree_and_continuous();
    // Single-key `n` / `N` cycle issues and sync left tree cursor to FindingItem (`[` / `]` are unbound)
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.status_message.contains("First finding"));
    assert!(matches!(
        app.tree_rows.get(app.tree_cursor),
        Some(codeduck::ui::app::TreeRow::FindingItem { finding_id, .. }) if finding_id == "f1"
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.status_message.contains("Second finding"));
    assert!(matches!(
        app.tree_rows.get(app.tree_cursor),
        Some(codeduck::ui::app::TreeRow::FindingItem { finding_id, .. }) if finding_id == "f2"
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
    assert!(app.status_message.contains("First finding"));
    assert!(matches!(
        app.tree_rows.get(app.tree_cursor),
        Some(codeduck::ui::app::TreeRow::FindingItem { finding_id, .. }) if finding_id == "f1"
    ));

    // Pressing `y` while commit_msg_generating sets commit_hint_notice (NOT commit_error)
    app.open_commit_confirm();
    app.open_commit_double_confirm();
    app.commit_msg_generating = true;
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(app.commit_error.is_none());
    assert!(app.commit_hint_notice.is_some());
    app.commit_msg_generating = false;
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // Selecting `Off` in AiPicker clears `ai_deep_running` & `ai_started_at`
    app.ai_deep_running = true;
    app.ai_started_at = Some(std::time::Instant::now());
    app.handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::NONE));
    app.ai_picker_cursor = app.available_providers.len(); // Off option
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.ai_deep_running);
    assert!(app.ai_started_at.is_none());
}

#[test]
fn test_revert_group_and_file_with_double_confirm_svn_and_git() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    let tmp = TempDir::new().unwrap();

    // --- Part 1: SVN Group Revert with Double Confirmation ---
    let repo_dir = tmp.path().join("svn_rev_repo");
    let wc_dir = tmp.path().join("svn_rev_wc");
    fs::create_dir_all(&repo_dir).unwrap();
    run("svnadmin", &["create", repo_dir.to_str().unwrap()], tmp.path());
    let repo_url = format!("file://{}", repo_dir.display());
    run("svn", &["checkout", &repo_url, wc_dir.to_str().unwrap()], tmp.path());

    fs::write(wc_dir.join("g1_a.lua"), "return 1\n").unwrap();
    fs::write(wc_dir.join("g1_b.lua"), "return 2\n").unwrap();
    fs::write(wc_dir.join("g2_keep.lua"), "return 3\n").unwrap();
    run("svn", &["add", "g1_a.lua", "g1_b.lua", "g2_keep.lua"], &wc_dir);
    run("svn", &["commit", "-m", "r1"], &wc_dir);

    fs::write(wc_dir.join("g1_a.lua"), "return 100\n").unwrap();
    fs::write(wc_dir.join("g1_b.lua"), "return 200\n").unwrap();
    fs::write(wc_dir.join("g2_keep.lua"), "return 300\n").unwrap();

    let backend = vcs::open_vcs(&wc_dir, Some(VcsKind::Svn)).unwrap();
    let mut cfg = AppConfig::default();
    cfg.ai_provider = "none".to_string();
    let mut app = App::new(backend, cfg, DiffTarget::Uncommitted, 120).unwrap();
    app.ai_review = Some(codeduck::model::AiReviewResult {
        provider_name: "mock".to_string(),
        summary: "groups".to_string(),
        overall_risk: codeduck::model::RiskLevel::Low,
        review_strategy: String::new(),
        groups: vec![
            codeduck::model::ReviewGroup {
                id: "g1".to_string(),
                title: "GroupOne".to_string(),
                description: String::new(),
                category: "logic".to_string(),
                risk_level: codeduck::model::RiskLevel::Low,
                review_order: 1,
                files: vec!["g1_a.lua".to_string(), "g1_b.lua".to_string()],
                key_points: vec![],
            },
            codeduck::model::ReviewGroup {
                id: "g2".to_string(),
                title: "GroupTwo".to_string(),
                description: String::new(),
                category: "logic".to_string(),
                risk_level: codeduck::model::RiskLevel::Low,
                review_order: 2,
                files: vec!["g2_keep.lua".to_string()],
                key_points: vec![],
            },
        ],
        files: vec![],
        hunks: vec![],
        findings: vec![],
    });
    app.tree_mode = codeduck::ui::app::TreeMode::AiGrouped;
    app.rebuild_tree_and_continuous();
    app.tree_cursor = 0; // On GroupHeader "GroupOne"
    app.focus = codeduck::ui::app::PaneFocus::Tree;

    // Press `U` -> opens RevertConfirm (Step 1) with both files in GroupOne
    app.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::RevertConfirm);
    assert_eq!(app.revert_files.len(), 2);

    // Draw Step 1 and Step 2 to ensure zero layout panics
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();

    // Press `y` -> opens RevertDoubleConfirm (Step 2, does not revert yet)
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(
        app.overlay,
        codeduck::ui::app::OverlayKind::RevertDoubleConfirm
    );
    assert!(!app.revert_executing);
    assert_eq!(fs::read_to_string(wc_dir.join("g1_a.lua")).unwrap(), "return 100\n");
    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();

    // Press `n` -> goes back to Step 1 without reverting
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::RevertConfirm);

    // Confirm Step 1 -> Step 2 -> confirm Step 2
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    for _ in 0..40 {
        app.poll_background();
        if !app.revert_executing && !app.diff_loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::None);
    assert_eq!(fs::read_to_string(wc_dir.join("g1_a.lua")).unwrap(), "return 1\n");
    assert_eq!(fs::read_to_string(wc_dir.join("g1_b.lua")).unwrap(), "return 2\n");
    // GroupTwo file untouched!
    assert_eq!(fs::read_to_string(wc_dir.join("g2_keep.lua")).unwrap(), "return 300\n");
    assert_eq!(app.bundle.changes.len(), 1);
    assert_eq!(app.bundle.changes[0].path, "g2_keep.lua");

    // --- Part 2: Git Single File Revert (Modified + Untracked) ---
    let git_dir = tmp.path().join("git_revert_repo");
    fs::create_dir_all(&git_dir).unwrap();
    run("git", &["init"], &git_dir);
    run("git", &["config", "user.email", "t@example.com"], &git_dir);
    run("git", &["config", "user.name", "Tester"], &git_dir);
    fs::write(git_dir.join("tracked.rs"), "fn ok() -> i32 { 1 }\n").unwrap();
    run("git", &["add", "tracked.rs"], &git_dir);
    run("git", &["commit", "-m", "init"], &git_dir);

    fs::write(git_dir.join("tracked.rs"), "fn ok() -> i32 { 999 }\n").unwrap();
    fs::write(git_dir.join("untracked.rs"), "// temp\n").unwrap();

    let git_backend = vcs::open_vcs(&git_dir, Some(VcsKind::Git)).unwrap();
    let mut gcfg = AppConfig::default();
    gcfg.ai_provider = "none".to_string();
    let mut gapp = App::new(git_backend, gcfg, DiffTarget::Uncommitted, 120).unwrap();
    // Target tracked.rs specifically in Diff focus and revert via `U` -> `y` -> `y`
    let tracked_idx = gapp.path_to_index["tracked.rs"];
    gapp.selected_file_idx = Some(tracked_idx);
    gapp.focus = codeduck::ui::app::PaneFocus::Diff;
    let tracked_y = gapp
        .continuous
        .sections
        .iter()
        .find(|s| s.change_idx == tracked_idx)
        .unwrap()
        .start_y;
    gapp.diff_cursor_y = tracked_y;
    gapp.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::NONE));
    assert_eq!(gapp.revert_files.len(), 1);
    assert_eq!(gapp.revert_files[0].path, "tracked.rs");
    gapp.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    gapp.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    for _ in 0..40 {
        gapp.poll_background();
        if !gapp.revert_executing && !gapp.diff_loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(
        fs::read_to_string(git_dir.join("tracked.rs")).unwrap(),
        "fn ok() -> i32 { 1 }\n"
    );
    // Untracked file still present until reverted explicitly
    assert!(git_dir.join("untracked.rs").exists());
    assert_eq!(gapp.bundle.changes.len(), 1);
    assert_eq!(gapp.bundle.changes[0].path, "untracked.rs");

    // Now revert untracked.rs
    gapp.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::NONE));
    gapp.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    gapp.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    for _ in 0..40 {
        gapp.poll_background();
        if !gapp.revert_executing && !gapp.diff_loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(!git_dir.join("untracked.rs").exists());
    assert!(gapp.bundle.changes.is_empty());
}

#[test]
fn test_localization_config_and_settings_modal() {
    use codeduck::i18n;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::{BTreeMap, BTreeSet};

    // 1. Dynamic locale resolution from `locales/*.toml` ([meta].aliases)
    let avail = i18n::available_locales();
    assert!(avail.contains(&"en".to_string()));
    assert!(avail.contains(&"zh-CN".to_string()));
    assert_eq!(i18n::resolve_locale("en").as_deref(), Some("en"));
    assert_eq!(i18n::resolve_locale("en-US").as_deref(), Some("en"));
    assert_eq!(i18n::resolve_locale("zh-CN").as_deref(), Some("zh-CN"));
    assert_eq!(i18n::resolve_locale("zh_hans").as_deref(), Some("zh-CN"));
    assert_eq!(i18n::resolve_locale("简体中文").as_deref(), Some("zh-CN"));
    assert_eq!(i18n::resolve_locale("unknown-xx"), None);

    // 2. Automated parity check across all `locales/*.toml` files:
    //    every locale must define the exact same keys and `%{var}` placeholders as `en.toml`.
    fn collect_toml_leaves(
        val: &toml::Value,
        prefix: &str,
        out: &mut BTreeMap<String, String>,
    ) {
        if let Some(tbl) = val.as_table() {
            for (k, v) in tbl {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                collect_toml_leaves(v, &next, out);
            }
        } else if let Some(s) = val.as_str() {
            if prefix != "_version" {
                out.insert(prefix.to_string(), s.to_string());
            }
        }
    }
    fn extract_placeholders(s: &str) -> BTreeSet<String> {
        let mut vars = BTreeSet::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i + 2 < bytes.len() {
            if bytes[i] == b'%' && bytes[i + 1] == b'{' {
                if let Some(end) = s[i + 2..].find('}') {
                    vars.insert(s[i + 2..i + 2 + end].to_string());
                    i += 2 + end + 1;
                    continue;
                }
            }
            i += 1;
        }
        vars
    }

    let locales_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
    let en_raw = fs::read_to_string(locales_dir.join("en.toml")).unwrap();
    let en_val: toml::Value = toml::from_str(&en_raw).unwrap();
    let mut en_keys = BTreeMap::new();
    collect_toml_leaves(&en_val, "", &mut en_keys);
    assert!(!en_keys.is_empty());

    for entry in fs::read_dir(&locales_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let fname = path.file_name().unwrap().to_string_lossy().into_owned();
        if fname == "en.toml" {
            continue;
        }
        let raw = fs::read_to_string(&path).unwrap();
        let val: toml::Value = toml::from_str(&raw).unwrap();
        let mut keys = BTreeMap::new();
        collect_toml_leaves(&val, "", &mut keys);
        let missing: Vec<_> = en_keys
            .keys()
            .filter(|k| !keys.contains_key(*k))
            .cloned()
            .collect();
        let extra: Vec<_> = keys
            .keys()
            .filter(|k| !en_keys.contains_key(*k))
            .cloned()
            .collect();
        assert!(missing.is_empty(), "{} missing keys: {:?}", fname, missing);
        assert!(extra.is_empty(), "{} extra keys: {:?}", fname, extra);
        for (k, en_str) in &en_keys {
            if k == "meta.aliases" {
                continue;
            }
            let loc_str = &keys[k];
            assert_eq!(
                extract_placeholders(en_str),
                extract_placeholders(loc_str),
                "Placeholder mismatch in {} for key `{}`",
                fname,
                k
            );
        }
    }

    let tmp = TempDir::new().unwrap();
    let cfg_path = tmp.path().join("configs.toml");
    fs::write(
        &cfg_path,
        "# user custom comment\n[ui]\nlanguage = \"en\"\n\n[ai]\nlanguage = \"zh-CN\"\n\n[ignore]\npatterns = [\"*.meta\"]\n",
    )
    .unwrap();

    // Explicit [ui].language takes precedence over [ai].language
    let loaded = AppConfig::load(Some(&cfg_path)).unwrap();
    assert_eq!(loaded.ui_language, "en");
    assert!(loaded.ui_language_explicit);
    assert_eq!(loaded.review_language, "zh-CN");

    // When [ui].language is omitted, [ai].language fallback configures UI locale cleanly
    let fallback_cfg_path = tmp.path().join("fallback.toml");
    fs::write(&fallback_cfg_path, "[ai]\nlanguage = \"zh-CN\"\n").unwrap();
    let fb_loaded = AppConfig::load(Some(&fallback_cfg_path)).unwrap();
    assert_eq!(fb_loaded.ui_language, "zh-CN");
    assert!(!fb_loaded.ui_language_explicit);

    // Setup tiny git repo to test interactive Settings modal ('O') + live language switch + save ('s')
    let git_dir = tmp.path().join("git_loc_repo");
    fs::create_dir_all(&git_dir).unwrap();
    run("git", &["init"], &git_dir);
    run("git", &["config", "user.email", "t@example.com"], &git_dir);
    run("git", &["config", "user.name", "Tester"], &git_dir);
    fs::write(git_dir.join("main.rs"), "fn main() {}\n").unwrap();
    run("git", &["add", "main.rs"], &git_dir);
    run("git", &["commit", "-m", "init"], &git_dir);
    fs::write(git_dir.join("main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();

    let backend = vcs::open_vcs(&git_dir, Some(VcsKind::Git)).unwrap();
    let mut app_cfg = loaded;
    app_cfg.ai_provider = "none".to_string();
    let mut app = App::new(backend, app_cfg, DiffTarget::Uncommitted, 120).unwrap();
    app.config_path_override = Some(cfg_path.clone());

    let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();

    // Render English default first
    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();
    let en_buf: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(en_buf.contains("File List") || en_buf.contains("Files:"));

    // Press 'O' to open Settings modal -> row 0 is UI Language -> press Space to switch to zh-CN
    app.handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::Settings);
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert_eq!(app.config.ui_language, "zh-CN");
    let mut term_zh = Terminal::new(TestBackend::new(120, 34)).unwrap();
    term_zh.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();
    let zh_buf: String = term_zh
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .filter(|s| *s != " ")
        .collect();
    assert!(zh_buf.contains("配置与语言设置"), "zh_buf={}", zh_buf);
    assert!(zh_buf.contains("简体中文"), "zh_buf={}", zh_buf);
    assert!(
        zh_buf.contains("未提交改动"),
        "target_label={:?} zh_buf={}",
        app.bundle.target.localized_label("zh-CN"),
        zh_buf
    );
    assert!(zh_buf.contains("并排对照"), "zh_buf={}", zh_buf);
    assert!(zh_buf.contains("浏览"), "zh_buf={}", zh_buf);
    assert!(!zh_buf.contains("uncommitted"), "zh_buf={}", zh_buf);
    assert!(!zh_buf.contains("NORMAL"), "zh_buf={}", zh_buf);

    // Press 's' to save and verify existing comment & [ignore] patterns are preserved
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    let saved_raw = fs::read_to_string(&cfg_path).unwrap();
    assert!(saved_raw.contains("# user custom comment"));
    assert!(saved_raw.contains("*.meta"));
    let reloaded = AppConfig::load(Some(&cfg_path)).unwrap();
    assert_eq!(reloaded.ui_language, "zh-CN");

    // Close Settings and verify ex-command ':ui-lang en' switches back immediately
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::None);
    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
    for ch in "ui-lang en".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.config.ui_language, "en");
}

#[test]
fn git_submodule_uncommitted_commit_and_revert_flow() {
    let tmp = TempDir::new().unwrap();

    // 1. Create child repo to serve as submodule upstream
    let sub_origin = tmp.path().join("sub_origin");
    fs::create_dir_all(sub_origin.join("docs")).unwrap();
    run("git", &["init", "-b", "main"], &sub_origin);
    run("git", &["config", "user.email", "t@example.com"], &sub_origin);
    run("git", &["config", "user.name", "Tester"], &sub_origin);
    fs::write(sub_origin.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    fs::write(sub_origin.join("docs/guide.md"), "# Guide\n").unwrap();
    run("git", &["add", "."], &sub_origin);
    run("git", &["commit", "-m", "sub init"], &sub_origin);

    // 2. Create superproject and add submodule at `moss`
    let super_dir = tmp.path().join("super_repo");
    fs::create_dir_all(&super_dir).unwrap();
    run("git", &["init", "-b", "main"], &super_dir);
    run("git", &["config", "user.email", "t@example.com"], &super_dir);
    run("git", &["config", "user.name", "Tester"], &super_dir);
    fs::write(super_dir.join("README.md"), "# Super\n").unwrap();
    run("git", &["add", "README.md"], &super_dir);
    run("git", &["commit", "-m", "super init"], &super_dir);

    let sub_url = sub_origin.to_string_lossy().to_string();
    run(
        "git",
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &sub_url,
            "moss",
        ],
        &super_dir,
    );
    run("git", &["commit", "-m", "add moss submodule"], &super_dir);

    let sub_dir = super_dir.join("moss");
    run("git", &["config", "user.email", "t@example.com"], &sub_dir);
    run("git", &["config", "user.name", "Tester"], &sub_dir);

    // 3. Modify main repo + modify/untracked/rename inside submodule
    fs::write(super_dir.join("README.md"), "# Super\nupdated\n").unwrap();
    fs::write(sub_dir.join("lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    fs::write(sub_dir.join("notes.txt"), "scratch note\n").unwrap();
    run("git", &["mv", "docs/guide.md", "docs/trade.md"], &sub_dir);

    let backend = vcs::open_vcs(&super_dir, Some(VcsKind::Git)).unwrap();
    assert_eq!(backend.external_prefixes(), vec!["moss".to_string()]);

    let bundle = backend
        .collect_changes_filtered(&DiffTarget::Uncommitted, None)
        .unwrap();
    let paths: Vec<&str> = bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "README.md",
            "moss/docs/trade.md",
            "moss/lib.rs",
            "moss/notes.txt",
        ]
    );

    let renamed = bundle
        .changes
        .iter()
        .find(|c| c.path == "moss/docs/trade.md")
        .unwrap();
    assert_eq!(
        renamed.kind,
        codeduck::model::ChangeKind::Renamed {
            from: "moss/docs/guide.md".to_string()
        }
    );
    let lib_change = bundle
        .changes
        .iter()
        .find(|c| c.path == "moss/lib.rs")
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(lib_change.old_content.as_deref().unwrap()),
        "pub fn v() -> i32 { 1 }\n"
    );
    assert_eq!(
        String::from_utf8_lossy(lib_change.new_content.as_deref().unwrap()),
        "pub fn v() -> i32 { 2 }\n"
    );

    // 4. Verify App marks submodule files `is_external: true` and main repo `false`
    let mut cfg = AppConfig::default();
    cfg.ai_provider = "none".to_string();
    let mut app = App::new(
        std::sync::Arc::clone(&backend),
        cfg,
        DiffTarget::Uncommitted,
        120,
    )
    .unwrap();
    app.open_commit_confirm();
    assert!(
        !app.commit_files
            .iter()
            .find(|f| f.path == "README.md")
            .unwrap()
            .is_external
    );
    assert!(
        app.commit_files
            .iter()
            .find(|f| f.path == "moss/lib.rs")
            .unwrap()
            .is_external
    );

    // 5. Revert modified + untracked file inside submodule without touching README.md or rename
    let rev_outcome = backend.revert_files_detailed(&[
        "moss/lib.rs".to_string(),
        "moss/notes.txt".to_string(),
    ]);
    assert!(rev_outcome.error.is_none(), "err={:?}", rev_outcome.error);
    assert!(!sub_dir.join("notes.txt").exists());
    assert_eq!(
        fs::read_to_string(sub_dir.join("lib.rs")).unwrap(),
        "pub fn v() -> i32 { 1 }\n"
    );

    // 6. Commit remaining changes across main repo + submodule with separate messages:
    //    submodule commits `docs/trade.md` first, then superproject merges updated `moss` gitlink!
    let c_outcome = backend.commit_files_with_external_message(
        &[
            "README.md".to_string(),
            "moss/docs/trade.md".to_string(),
        ],
        "feat: update super docs and sync moss",
        Some("docs: rename guide.md to trade.md"),
    );
    assert!(c_outcome.error.is_none(), "err={:?}", c_outcome.error);
    assert_eq!(c_outcome.committed_paths.len(), 2);

    // 7. Because superproject commit automatically synced `moss` gitlink, working tree is now clean
    let after_bundle = backend
        .collect_changes_filtered(&DiffTarget::Uncommitted, None)
        .unwrap();
    assert!(
        after_bundle.changes.is_empty(),
        "expected clean tree after merged commit, got: {:?}",
        after_bundle
            .changes
            .iter()
            .map(|c| &c.path)
            .collect::<Vec<_>>()
    );

    // Verify both repos received their own independent commit log messages,
    // and superproject HEAD recorded both README.md and `moss` gitlink update!
    let head_bundle = backend
        .collect_changes_filtered(&DiffTarget::Commit("HEAD".to_string()), None)
        .unwrap();
    let head_paths: Vec<&str> = head_bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(head_paths, vec!["README.md", "moss"]);
    let sm_gitlink = head_bundle
        .changes
        .iter()
        .find(|c| c.path == "moss")
        .unwrap();
    assert!(String::from_utf8_lossy(sm_gitlink.old_content.as_deref().unwrap()).starts_with("Submodule commit "));
    assert!(String::from_utf8_lossy(sm_gitlink.new_content.as_deref().unwrap()).starts_with("Submodule commit "));

    let sub_log = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(&sub_dir)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&sub_log.stdout).trim(),
        "docs: rename guide.md to trade.md"
    );
    let super_log = backend.recent_commits(1).unwrap();
    assert_eq!(super_log[0].message, "feat: update super docs and sync moss");

    // 8. Also test submodule-only uncommitted edit through App Commit modal with mock AI
    //    -> verifies separate external log generation + single-action merged superproject gitlink commit!
    use codeduck::ui::app::CommitModalFocus;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fs::write(sub_dir.join("lib.rs"), "pub fn v() -> i32 { 42 }\n").unwrap();
    let app_backend = vcs::open_vcs(&super_dir, Some(VcsKind::Git)).unwrap();
    let mut cfg = AppConfig::default();
    cfg.ai_provider = "mock".to_string();
    let mut app = App::new(app_backend, cfg, DiffTarget::Uncommitted, 120).unwrap();

    // Open CommitConfirm ('S')
    app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::CommitConfirm);
    // In submodule-only selection, external log is separate (`ext_commit_message`)
    // and main message auto-derives submodule link log (`Update submodule moss: ...`).
    for _ in 0..40 {
        app.poll_background();
        if !app.any_commit_msg_generating() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(!app.ext_commit_message.trim().is_empty());
    assert!(
        app.commit_message.starts_with("Update submodule moss:"),
        "commit_message={:?}",
        app.commit_message
    );

    // Tab cycles Files -> Message -> ExternalMessage -> Files, and rendering both boxes panics nowhere
    let mut term = Terminal::new(TestBackend::new(110, 36)).unwrap();
    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.commit_modal_focus, CommitModalFocus::Message);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.commit_modal_focus, CommitModalFocus::ExternalMessage);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.commit_modal_focus, CommitModalFocus::Files);

    // Confirm -> Step 2 (renders both Main & External boxes cleanly) -> execute merged commit
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(
        app.overlay,
        codeduck::ui::app::OverlayKind::CommitDoubleConfirm
    );
    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    for _ in 0..40 {
        app.poll_background();
        if !app.commit_executing && !app.diff_loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert_eq!(app.overlay, codeduck::ui::app::OverlayKind::None);
    assert!(
        app.bundle.changes.is_empty(),
        "expected completely clean superproject & submodule after merged commit, remaining={:?}",
        app.bundle.changes
    );
}

#[test]
fn test_parse_stage2_review_json_lenient_variations() {
    let raw = r#"
I inspected the codebase first:
```json
{"summary": "preliminary note"}
```

Here is the final review:
```json
{
  "overall_risk": "medium",
  "files": [
    {
      "path": "src/lib.rs",
      "category": null,
      "risk_level": "medium",
      "summary": null,
      "review_focus": "Verify overflow guard",
    },
  ],
  "hunks": [
    {
      "file": "src/lib.rs",
      "chunk_index": "1",
      "start_line": "40",
      "end_line": "55",
      "title": "Check math",
      "analysis": "Handles saturating add",
      "risk_level": "low",
    },
  ],
  "findings": [
    {
      "id": null,
      "file": null,
      "line": "42-48",
      "severity": "warning",
      "title": "Global invariant",
      "description": "Missing check when input is zero",
      "suggestion": ["Add guard clause", "Add unit test"],
    },
  ],
}
```
"#;
    let parsed = ai::parse_ai_review_json(raw, "agy", &[]).unwrap();
    assert_eq!(parsed.overall_risk, codeduck::model::RiskLevel::Medium);
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].review_focus, vec!["Verify overflow guard"]);
    assert_eq!(parsed.hunks.len(), 1);
    assert_eq!(parsed.hunks[0].chunk_index, Some(1));
    assert_eq!(parsed.hunks[0].start_line, Some(40));
    assert_eq!(parsed.findings.len(), 1);
    assert_eq!(parsed.findings[0].line, Some(42));
    assert_eq!(
        parsed.findings[0].suggestion.as_deref(),
        Some("Add guard clause\nAdd unit test")
    );
}

#[test]
fn test_svn_same_repo_external_merges_commit_while_cross_repo_splits() {
    let tmp = TempDir::new().unwrap();
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    let seed_a = tmp.path().join("seed_a");
    let seed_b = tmp.path().join("seed_b");
    let wc_main = tmp.path().join("wc_main");

    run("svnadmin", &["create", repo_a.to_str().unwrap()], tmp.path());
    run("svnadmin", &["create", repo_b.to_str().unwrap()], tmp.path());
    let url_a = format!("file://{}", repo_a.display());
    let url_b = format!("file://{}", repo_b.display());

    // Populate repo_b (separate repository external)
    run("svn", &["checkout", &url_b, seed_b.to_str().unwrap()], tmp.path());
    fs::write(seed_b.join("cross.lua"), "return 1\n").unwrap();
    run("svn", &["add", "cross.lua"], &seed_b);
    run("svn", &["commit", "-m", "b r1"], &seed_b);

    // Populate repo_a with /trunk and /shared, plus svn:externals pointing to BOTH:
    //   - SameExt ^/shared (same SVN repo_a!)
    //   - CrossExt <url_b> (different SVN repo_b!)
    run("svn", &["checkout", &url_a, seed_a.to_str().unwrap()], tmp.path());
    fs::create_dir_all(seed_a.join("trunk/Assets")).unwrap();
    fs::create_dir_all(seed_a.join("shared")).unwrap();
    fs::write(seed_a.join("trunk/main.lua"), "return 1\n").unwrap();
    fs::write(seed_a.join("shared/same.lua"), "return 1\n").unwrap();
    run("svn", &["add", "trunk", "shared"], &seed_a);
    run(
        "svn",
        &[
            "propset",
            "svn:externals",
            &format!("^/shared SameExt\nCrossExt {}\n", url_b),
            "trunk/Assets",
        ],
        &seed_a,
    );
    run("svn", &["commit", "-m", "a r1"], &seed_a);

    // Checkout trunk
    let trunk_url = format!("{}/trunk", url_a);
    run("svn", &["checkout", &trunk_url, wc_main.to_str().unwrap()], tmp.path());

    // Modify main.lua, SameExt/same.lua (same repo), and CrossExt/cross.lua (different repo)
    fs::write(wc_main.join("main.lua"), "return 2\n").unwrap();
    fs::write(wc_main.join("Assets/SameExt/same.lua"), "return 2\n").unwrap();
    fs::write(wc_main.join("Assets/CrossExt/cross.lua"), "return 2\n").unwrap();

    let backend = vcs::open_vcs(&wc_main, Some(VcsKind::Svn)).unwrap();

    // Only cross-repo external is returned as a split external prefix
    let exts = backend.external_prefixes();
    assert_eq!(exts, vec!["Assets/CrossExt".to_string()]);

    // Uncommitted still collects all 3 changed files across main + both externals
    let bundle = backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    let paths: Vec<&str> = bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "Assets/CrossExt/cross.lua",
            "Assets/SameExt/same.lua",
            "main.lua",
        ]
    );

    // Committing main.lua + Assets/SameExt/same.lua merges into ONE single SVN revision (r2)!
    let head_before = backend.latest_revision().unwrap().unwrap();
    assert_eq!(head_before, "1");
    let merged_outcome = backend.commit_files_detailed(
        &[
            "main.lua".to_string(),
            "Assets/SameExt/same.lua".to_string(),
        ],
        "feat: single atomic commit for main and same-repo external",
    );
    assert!(merged_outcome.error.is_none(), "err={:?}", merged_outcome.error);
    assert_eq!(merged_outcome.committed_paths.len(), 2);

    // Verify repo_a advanced by strictly 1 revision (r1 -> r2)
    run("svn", &["update"], &wc_main);
    let head_after = backend.latest_revision().unwrap().unwrap();
    assert_eq!(head_after, "2");

    // CrossExt still remains uncommitted until committed independently with external message
    let remaining_bundle = backend.collect_changes(&DiffTarget::Uncommitted).unwrap();
    assert_eq!(remaining_bundle.changes.len(), 1);
    assert_eq!(remaining_bundle.changes[0].path, "Assets/CrossExt/cross.lua");

    let cross_outcome = backend.commit_files_with_external_message(
        &["Assets/CrossExt/cross.lua".to_string()],
        "",
        Some("ext: separate cross-repo update"),
    );
    assert!(cross_outcome.error.is_none(), "err={:?}", cross_outcome.error);
    assert!(
        backend
            .collect_changes(&DiffTarget::Uncommitted)
            .unwrap()
            .changes
            .is_empty()
    );
}

#[test]
fn test_partial_commit_and_revert_prune_in_place_without_full_reload() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;

    let tmp = TempDir::new().unwrap();
    let git_dir = tmp.path().join("partial_prune_repo");
    fs::create_dir_all(&git_dir).unwrap();
    run("git", &["init"], &git_dir);
    run("git", &["config", "user.email", "t@example.com"], &git_dir);
    run("git", &["config", "user.name", "Tester"], &git_dir);
    fs::write(git_dir.join("a.rs"), "fn a() { 1 }\n").unwrap();
    fs::write(git_dir.join("b.rs"), "fn b() { 1 }\n").unwrap();
    fs::write(git_dir.join("c.rs"), "fn c() { 1 }\n").unwrap();
    run("git", &["add", "."], &git_dir);
    run("git", &["commit", "-m", "init"], &git_dir);

    fs::write(git_dir.join("a.rs"), "fn a() { 2 }\n").unwrap();
    fs::write(git_dir.join("b.rs"), "fn b() { 2 }\n").unwrap();
    fs::write(git_dir.join("c.rs"), "fn c() { 2 }\n").unwrap();

    let backend = vcs::open_vcs(&git_dir, Some(VcsKind::Git)).unwrap();
    let mut cfg = AppConfig::default();
    cfg.ai_provider = "none".to_string();
    let mut app = App::new(Arc::clone(&backend), cfg, DiffTarget::Uncommitted, 120).unwrap();

    // Seed existing AI review covering all 3 files across 2 groups + findings
    app.ai_review = Some(codeduck::model::AiReviewResult {
        provider_name: "mock".to_string(),
        summary: "existing analysis".to_string(),
        overall_risk: codeduck::model::RiskLevel::Medium,
        review_strategy: String::new(),
        groups: vec![
            codeduck::model::ReviewGroup {
                id: "g1".to_string(),
                title: "Group A".to_string(),
                description: String::new(),
                category: "feature".to_string(),
                risk_level: codeduck::model::RiskLevel::Low,
                review_order: 1,
                files: vec!["a.rs".to_string()],
                key_points: vec![],
            },
            codeduck::model::ReviewGroup {
                id: "g2".to_string(),
                title: "Group BC".to_string(),
                description: String::new(),
                category: "bugfix".to_string(),
                risk_level: codeduck::model::RiskLevel::Medium,
                review_order: 2,
                files: vec!["b.rs".to_string(), "c.rs".to_string()],
                key_points: vec![],
            },
        ],
        files: vec![],
        hunks: vec![],
        findings: vec![
            codeduck::model::ReviewFinding {
                id: "f1".to_string(),
                file: "a.rs".to_string(),
                line: Some(1),
                severity: codeduck::model::Severity::Warning,
                title: "Finding A".to_string(),
                description: "desc a".to_string(),
                suggestion: None,
            },
            codeduck::model::ReviewFinding {
                id: "f2".to_string(),
                file: "b.rs".to_string(),
                line: Some(1),
                severity: codeduck::model::Severity::Warning,
                title: "Finding B".to_string(),
                description: "desc b".to_string(),
                suggestion: None,
            },
        ],
    });
    app.tree_mode = codeduck::ui::app::TreeMode::AiGrouped;
    app.rebuild_tree_and_continuous();
    let initial_diff_gen = app.diff_generation;

    // 1. Partial commit: commit only `a.rs` via CommitConfirm
    app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
    for f in &mut app.commit_files {
        f.included = f.path == "a.rs";
    }
    app.commit_message = "commit only a.rs".to_string();
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    for _ in 0..40 {
        app.poll_background();
        if !app.commit_executing {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Must prune in-place without triggering a full Diff reload or discarding remaining AI review!
    assert!(!app.diff_loading);
    assert_eq!(app.diff_generation, initial_diff_gen);
    let paths_after_commit: Vec<&str> = app.bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths_after_commit, vec!["b.rs", "c.rs"]);
    let rev = app.ai_review.as_ref().expect("AI review should be preserved");
    assert_eq!(rev.groups.len(), 1);
    assert_eq!(rev.groups[0].files, vec!["b.rs", "c.rs"]);
    assert_eq!(rev.findings.len(), 1);
    assert_eq!(rev.findings[0].file, "b.rs");

    // 2. Partial revert: select Group BC, revert only `c.rs` -> prunes `c.rs` in-place while keeping `b.rs` & its finding
    app.tree_cursor = 0;
    app.focus = codeduck::ui::app::PaneFocus::Tree;
    app.open_revert_confirm();
    for f in &mut app.revert_files {
        f.included = f.path == "c.rs";
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    for _ in 0..40 {
        app.poll_background();
        if !app.revert_executing {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!app.diff_loading);
    assert_eq!(app.diff_generation, initial_diff_gen);
    let paths_after_revert: Vec<&str> = app.bundle.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths_after_revert, vec!["b.rs"]);
    let rev2 = app.ai_review.as_ref().expect("AI review for b.rs should remain");
    assert_eq!(rev2.findings.len(), 1);
    assert_eq!(rev2.findings[0].file, "b.rs");
}

#[test]
fn test_header_project_name_scheme_c_diff_and_synced_footer_progress() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;

    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("my_cool_project");
    fs::create_dir_all(&repo_dir).unwrap();
    run("git", &["init"], &repo_dir);
    run("git", &["config", "user.email", "t@example.com"], &repo_dir);
    run("git", &["config", "user.name", "Tester"], &repo_dir);
    fs::write(repo_dir.join("main.rs"), "fn main() {}\n").unwrap();
    run("git", &["add", "main.rs"], &repo_dir);
    run("git", &["commit", "-m", "init"], &repo_dir);
    fs::write(repo_dir.join("main.rs"), "fn main() { let x = 1; }\n").unwrap();

    let backend = vcs::open_vcs(&repo_dir, Some(VcsKind::Git)).unwrap();
    let mut cfg = AppConfig::default();
    cfg.ai_provider = "none".to_string();
    let mut app = App::new(Arc::clone(&backend), cfg, DiffTarget::Uncommitted, 120).unwrap();

    // Default Scheme C: inspector hidden so Diff takes full workspace height
    assert!(!app.show_inspector);

    // Simulate active AI progress to verify synced footer spinner + project name in header
    app.ai_running = true;
    app.active_ai_provider_id = "agy".to_string();
    app.ai_started_at = Some(std::time::Instant::now());
    app.ai_progress_stage = "Testing stream stage...".to_string();

    let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();
    let rendered: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        rendered.contains("my_cool_project"),
        "header should show project name, got:\n{}",
        rendered
    );
    assert!(
        rendered.contains("Running (agy"),
        "footer should show live spinner progress, got:\n{}",
        rendered
    );
    assert!(rendered.contains("Testing stream stage..."));
    assert!(
        !rendered.contains("Group Details"),
        "inspector box should stay hidden by default under Scheme C"
    );

    // Findings and hunk analyses render inline beneath their code lines inside Diff,
    // leaving the right Diff pane at 100% full height unless the user explicitly presses `I`.
    app.ai_running = false;
    app.ai_review = Some(codeduck::model::AiReviewResult {
        provider_name: "mock".to_string(),
        summary: "ok".to_string(),
        overall_risk: codeduck::model::RiskLevel::Medium,
        review_strategy: String::new(),
        groups: vec![codeduck::model::ReviewGroup {
            id: "g1".to_string(),
            title: "Main".to_string(),
            description: String::new(),
            category: "feature".to_string(),
            risk_level: codeduck::model::RiskLevel::Medium,
            review_order: 1,
            files: vec!["main.rs".to_string()],
            key_points: vec![],
        }],
        files: vec![],
        hunks: vec![codeduck::model::HunkAnalysis {
            file: "main.rs".to_string(),
            chunk_index: Some(0),
            start_line: Some(1),
            end_line: Some(1),
            title: "Declare unused local".to_string(),
            analysis: "Adds variable x inside main.".to_string(),
            risk_level: codeduck::model::RiskLevel::Medium,
        }],
        findings: vec![codeduck::model::ReviewFinding {
            id: "f1".to_string(),
            file: "main.rs".to_string(),
            line: Some(1),
            severity: codeduck::model::Severity::Warning,
            title: "Unused local x".to_string(),
            description: "Variable x is unused".to_string(),
            suggestion: Some("Remove or use x".to_string()),
        }],
    });
    app.rebuild_tree_and_continuous();
    // Verify both ReviewFinding AND Medium/High/Critical HunkAnalysis appear in the left tree under main.rs
    assert!(app.tree_rows.iter().any(|r| matches!(
        r,
        codeduck::ui::app::TreeRow::FindingItem { finding_id, line: Some(1), .. } if finding_id == "f1"
    )));
    assert!(app.tree_rows.iter().any(|r| matches!(
        r,
        codeduck::ui::app::TreeRow::FindingItem {
            level: codeduck::ui::app::TreeIssueLevel::HunkRisk(codeduck::model::RiskLevel::Medium),
            title,
            ..
        } if title == "Declare unused local"
    )));
    // Selecting FindingItem via `n` cycles through FindingItem and Medium HunkAnalysis and highlights in tree
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(
        app.continuous.line_to_source_line[app.diff_cursor_y],
        Some(1)
    );
    assert!(matches!(
        app.tree_rows.get(app.tree_cursor),
        Some(codeduck::ui::app::TreeRow::FindingItem { finding_id, .. }) if finding_id == "f1"
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(matches!(
        app.tree_rows.get(app.tree_cursor),
        Some(codeduck::ui::app::TreeRow::FindingItem {
            level: codeduck::ui::app::TreeIssueLevel::HunkRisk(codeduck::model::RiskLevel::Medium),
            ..
        })
    ));
    assert_eq!(app.focus, codeduck::ui::app::PaneFocus::Diff);

    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();
    let with_inline_ai: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        with_inline_ai.contains("Unused local x"),
        "finding should render inline beneath its code line:\n{}",
        with_inline_ai
    );
    assert!(with_inline_ai.contains("Remove or use x"));
    assert!(with_inline_ai.contains("Declare unused local"));
    assert!(with_inline_ai.contains("┌─ "));
    assert!(with_inline_ai.contains("┐"));
    assert!(with_inline_ai.contains("┘"));
    assert!(
        !with_inline_ai.contains("AI Finding Preview"),
        "bottom popup box must stay closed so Diff keeps 100% height"
    );
    assert!(!with_inline_ai.contains("Code Inspector"));

    // Pressing `I` or `J` does NOT open an Inspector popup; Diff keeps 100% full height
    app.handle_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE));
    assert!(!app.show_inspector);
    term.draw(|f| codeduck::ui::draw(f, &mut app)).unwrap();
    let after_i_key: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(!after_i_key.contains("Code Inspector"));
}
