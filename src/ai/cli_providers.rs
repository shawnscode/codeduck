use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

use crate::ai::{AiProgressUpdate, AiPromptOutput, AiProvider, emit_progress};

pub fn binary_exists(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

pub fn has_auth_hint(provider_id: &str) -> bool {
    let Some(home) = dirs::home_dir() else {
        return true;
    };
    match provider_id {
        "agy" => {
            home.join(".gemini/antigravity-cli").is_dir()
                || home.join(".gemini/antigravity").is_dir()
        }
        "codex" => home.join(".codex/auth.json").is_file(),
        "claude" => {
            home.join(".claude/.credentials.json").is_file()
                || home.join(".claude.json").is_file()
        }
        "gemini" => home.join(".gemini/oauth_creds.json").is_file(),
        _ => true,
    }
}

enum CliStreamEvent {
    StdoutLine(String),
    StderrLine(String),
}

fn run_with_stdin(
    bin: &str,
    args: &[&str],
    stdin_data: &str,
    work_dir: &Path,
    timeout_secs: u64,
) -> Result<String> {
    run_with_stdin_streaming(bin, args, stdin_data, work_dir, timeout_secs, |_| {})
}

fn run_with_stdin_streaming<F>(
    bin: &str,
    args: &[&str],
    stdin_data: &str,
    work_dir: &Path,
    timeout_secs: u64,
    mut on_stdout_line: F,
) -> Result<String>
where
    F: FnMut(&str),
{
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn local AI CLI `{}`", bin))?;

    if let Some(mut stdin) = child.stdin.take() {
        let data = stdin_data.as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (tx, rx) = mpsc::channel::<CliStreamEvent>();

    let tx_out = tx.clone();
    let out_handle = std::thread::spawn(move || {
        let mut all_out = String::new();
        if let Some(r) = stdout_pipe {
            let reader = BufReader::new(r);
            for line_res in reader.lines() {
                match line_res {
                    Ok(line) => {
                        all_out.push_str(&line);
                        all_out.push('\n');
                        let _ = tx_out.send(CliStreamEvent::StdoutLine(line));
                    }
                    Err(_) => break,
                }
            }
        }
        all_out
    });

    let tx_err = tx;
    let err_handle = std::thread::spawn(move || {
        let mut all_err = String::new();
        if let Some(r) = stderr_pipe {
            let reader = BufReader::new(r);
            for line_res in reader.lines() {
                match line_res {
                    Ok(line) => {
                        all_err.push_str(&line);
                        all_err.push('\n');
                        let _ = tx_err.send(CliStreamEvent::StderrLine(line));
                    }
                    Err(_) => break,
                }
            }
        }
        all_err
    });

    let mut drain_events = |stderr_hook: bool| {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                CliStreamEvent::StdoutLine(line) => on_stdout_line(&line),
                CliStreamEvent::StderrLine(line) => {
                    if stderr_hook {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            let clipped: String = trimmed.chars().take(160).collect();
                            emit_progress(AiProgressUpdate::LogLine(clipped));
                        }
                    }
                }
            }
        }
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(5));
    let status = loop {
        drain_events(true);
        if crate::ai::is_current_task_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{}` cancelled", bin);
        }
        match child.try_wait()? {
            Some(st) => break st,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("`{}` timed out after {}s", bin, timeout_secs);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let stdout_text = out_handle.join().unwrap_or_default();
    let stderr_text = err_handle.join().unwrap_or_default();
    drain_events(false);

    if !status.success() {
        let stderr = stderr_text.trim();
        let stdout = stdout_text.trim();
        bail!(
            "`{}` exited with status {}: {}{}",
            bin,
            status,
            stderr,
            if stderr.is_empty() { stdout } else { "" }
        );
    }

    Ok(stdout_text)
}

pub struct ClaudeCliProvider {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout_secs: u64,
}

impl AiProvider for ClaudeCliProvider {
    fn id(&self) -> &str {
        "claude"
    }

    fn display_name(&self) -> &str {
        "Claude Code (Local Subscription)"
    }

    fn run_prompt_with_session(
        &self,
        prompt: &str,
        work_dir: &Path,
        json_mode: bool,
        session_id: Option<&str>,
        persist_session: bool,
    ) -> Result<AiPromptOutput> {
        let want_json_envelope =
            json_mode || persist_session || session_id.is_some() || crate::ai::has_progress_hook();
        let allow_tools = crate::ai::current_allow_tools();
        let mut args = vec!["-p"];
        if allow_tools {
            args.extend(["--tools", "Read,Grep,Glob", "--permission-mode", "plan"]);
        } else {
            args.extend(["--tools", ""]);
        }
        args.extend([
            "--output-format",
            if want_json_envelope { "json" } else { "text" },
        ]);
        if !persist_session && session_id.is_none() {
            args.push("--no-session-persistence");
        }
        if let Some(sid) = session_id {
            args.push("--resume");
            args.push(sid);
        }
        if let Some(m) = &self.model {
            args.push("--model");
            args.push(m);
        }
        if let Some(e) = &self.effort {
            args.push("--effort");
            args.push(e);
        }

        let raw = run_with_stdin("claude", &args, prompt, work_dir, self.timeout_secs)?;
        if want_json_envelope {
            if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
                if v.get("is_error").and_then(|e| e.as_bool()) == Some(true) {
                    let err_msg = v
                        .get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("Claude CLI returned is_error=true");
                    anyhow::bail!("claude error: {}", err_msg);
                }
                let sid = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
                if let Some(inner) = v.get("result").and_then(|r| r.as_str()) {
                    return Ok(AiPromptOutput {
                        text: inner.to_string(),
                        session_id: sid,
                    });
                }
            }
        }
        Ok(AiPromptOutput {
            text: raw,
            session_id: session_id.map(|s| s.to_string()),
        })
    }
}

pub struct CodexCliProvider {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout_secs: u64,
}

impl AiProvider for CodexCliProvider {
    fn id(&self) -> &str {
        "codex"
    }

    fn display_name(&self) -> &str {
        "Codex CLI (Local Subscription)"
    }

    fn run_prompt_with_session(
        &self,
        prompt: &str,
        work_dir: &Path,
        _json_mode: bool,
        session_id: Option<&str>,
        persist_session: bool,
    ) -> Result<AiPromptOutput> {
        let out_file = NamedTempFile::new().context("Failed to create temp file for codex output")?;
        let out_path = out_file.path().to_string_lossy().to_string();

        let effort_cfg = self.effort.as_deref().map(|e| {
            let mapped = if e.eq_ignore_ascii_case("max") {
                "xhigh"
            } else {
                e
            };
            format!("model_reasoning_effort=\"{}\"", mapped)
        });

        let mut args: Vec<&str> = vec!["exec"];
        if let Some(sid) = session_id {
            args.extend([
                "resume",
                "--skip-git-repo-check",
                "-c",
                "mcp_servers={}",
                "-c",
                "sandbox_mode=\"read-only\"",
                "--json",
                "-o",
                &out_path,
            ]);
            if let Some(ref cfg_val) = effort_cfg {
                args.push("-c");
                args.push(cfg_val);
            }
            if let Some(m) = &self.model {
                args.push("-m");
                args.push(m);
            }
            args.push(sid);
            args.push("-");
        } else {
            args.extend([
                "--skip-git-repo-check",
                "-s",
                "read-only",
                "-c",
                "mcp_servers={}",
                "-o",
                &out_path,
            ]);
            if persist_session {
                args.push("--json");
            } else {
                args.push("--ephemeral");
            }
            if let Some(ref cfg_val) = effort_cfg {
                args.push("-c");
                args.push(cfg_val);
            }
            if let Some(m) = &self.model {
                args.push("-m");
                args.push(m);
            }
            args.push("-");
        }

        let stdout = run_with_stdin_streaming(
            "codex",
            &args,
            prompt,
            work_dir,
            self.timeout_secs,
            |line| {
                let trimmed = line.trim();
                if !trimmed.starts_with('{') {
                    return;
                }
                if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                    match v.get("type").and_then(|t| t.as_str()) {
                        Some("thread.started") => {
                            let tid = v
                                .get("thread_id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("");
                            let short_tid: String = tid.chars().take(8).collect();
                            emit_progress(AiProgressUpdate::Stage(format!(
                                "Codex 会话已连接 ({})",
                                short_tid
                            )));
                        }
                        Some("item.started") | Some("item.completed") => {
                            if let Some(item) = v.get("item") {
                                let kind = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    if !text.trim().is_empty() {
                                        if kind == "reasoning" {
                                            emit_progress(AiProgressUpdate::ThinkingDelta(format!(
                                                "{}\n",
                                                text
                                            )));
                                        } else {
                                            emit_progress(AiProgressUpdate::TextDelta(format!(
                                                "{}\n",
                                                text
                                            )));
                                        }
                                    }
                                } else if !kind.is_empty() {
                                    emit_progress(AiProgressUpdate::Stage(format!(
                                        "Codex 步骤: {}",
                                        kind
                                    )));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            },
        )?;
        let from_file = std::fs::read_to_string(out_file.path()).unwrap_or_default();
        let captured_sid = extract_codex_thread_id(&stdout)
            .or_else(|| session_id.map(|s| s.to_string()));
        let text = if !from_file.trim().is_empty() {
            from_file
        } else {
            extract_codex_last_message(&stdout).unwrap_or(stdout)
        };
        Ok(AiPromptOutput {
            text,
            session_id: captured_sid,
        })
    }
}

fn extract_codex_last_message(jsonl: &str) -> Option<String> {
    let mut last_msg: Option<String> = None;
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if v.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
                if let Some(item) = v.get("item") {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        if !text.trim().is_empty() {
                            last_msg = Some(text.to_string());
                        }
                    }
                }
            }
        }
    }
    last_msg
}

fn extract_codex_thread_id(jsonl: &str) -> Option<String> {
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if v.get("type").and_then(|t| t.as_str()) == Some("thread.started") {
                if let Some(tid) = v.get("thread_id").and_then(|s| s.as_str()) {
                    return Some(tid.to_string());
                }
            }
        }
    }
    None
}

pub struct AgyCliProvider {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout_secs: u64,
}

impl AiProvider for AgyCliProvider {
    fn id(&self) -> &str {
        "agy"
    }

    fn display_name(&self) -> &str {
        "Antigravity (agy Local Sub)"
    }

    fn run_prompt_with_session(
        &self,
        prompt: &str,
        work_dir: &Path,
        json_mode: bool,
        session_id: Option<&str>,
        persist_session: bool,
    ) -> Result<AiPromptOutput> {
        let want_json_envelope =
            json_mode || persist_session || session_id.is_some() || crate::ai::has_progress_hook();
        // Sanitize NUL bytes and enforce a safe byte cap to stay below OS ARG_MAX limits.
        let clean_prompt: String = prompt.chars().filter(|&c| c != '\0').collect();
        let bounded_prompt = crate::ai::truncate_utf8_bytes(&clean_prompt, 96_000);
        let prompt_arg = format!("-p={}", bounded_prompt);
        let mut args = vec![
            "--mode",
            "plan",
            "--disable-slash-commands",
            "--output-format",
            if want_json_envelope { "stream-json" } else { "text" },
        ];
        if let Some(sid) = session_id {
            args.push("--resume");
            args.push(sid);
        }
        if let Some(m) = &self.model {
            args.push("--model");
            args.push(m);
        }
        if let Some(e) = &self.effort {
            args.push("--effort");
            args.push(e);
        }
        args.push(&prompt_arg);

        let mut stream_sid: Option<String> = None;
        let mut stream_result: Option<String> = None;
        let mut stream_text_buf = String::new();
        let mut last_step_key = String::new();

        let raw = run_with_stdin_streaming("agy", &args, "", work_dir, self.timeout_secs, |line| {
            if !want_json_envelope {
                return;
            }
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                return;
            };
            match v.get("event").and_then(|e| e.as_str()) {
                Some("init") => {
                    if let Some(cid) = v.get("conversation_id").and_then(|s| s.as_str()) {
                        stream_sid = Some(cid.to_string());
                    }
                    let model = v
                        .get("init")
                        .and_then(|i| i.get("model"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("agy");
                    let short_cid: String = stream_sid
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(8)
                        .collect();
                    emit_progress(AiProgressUpdate::Stage(format!(
                        "连接 {} ({})",
                        model, short_cid
                    )));
                }
                Some("step_update") => {
                    if let Some(su) = v.get("step_update") {
                        if let Some(cid) = su.get("conversation_id").and_then(|s| s.as_str()) {
                            if !cid.trim().is_empty() {
                                stream_sid = Some(cid.to_string());
                            }
                        }
                        if let Some(stype) = su.get("step_type").and_then(|s| s.as_str()) {
                            let state = su.get("state").and_then(|s| s.as_str()).unwrap_or("");
                            let sidx = su.get("step_index").and_then(|n| n.as_u64()).unwrap_or(0);
                            let step_key = if stype == "tool" {
                                format!("{}:{}:{}", sidx, stype, state)
                            } else {
                                format!("{}:{}", sidx, stype)
                            };
                            if step_key != last_step_key {
                                last_step_key = step_key;
                                match stype {
                                    "user_input" => {
                                        emit_progress(AiProgressUpdate::Stage(
                                            "等待响应...".to_string(),
                                        ));
                                    }
                                    "planner_response" | "thought" | "thinking" => {
                                        emit_progress(AiProgressUpdate::Stage(
                                            "思考中...".to_string(),
                                        ));
                                    }
                                    "agent_response" => {
                                        emit_progress(AiProgressUpdate::Stage(
                                            "输出中...".to_string(),
                                        ));
                                    }
                                    "tool" if state == "ACTIVE" => {
                                        let tool_name = su
                                            .get("tool_name")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("tool");
                                        let params = su
                                            .get("tool_info")
                                            .and_then(|ti| ti.get("parameters"));
                                        let target = params
                                            .and_then(|p| {
                                                ["Path", "TargetFile", "AbsolutePath", "Query", "Pattern", "DirectoryPath", "SearchDirectory"]
                                                    .iter()
                                                    .find_map(|k| p.get(*k).and_then(|v| v.as_str()))
                                            })
                                            .unwrap_or("");
                                        let label = if target.is_empty() {
                                            format!("调用 {}", tool_name)
                                        } else {
                                            let short_target: String = target
                                                .chars()
                                                .rev()
                                                .take(48)
                                                .collect::<Vec<_>>()
                                                .into_iter()
                                                .rev()
                                                .collect();
                                            format!("{}: {}", tool_name, short_target)
                                        };
                                        emit_progress(AiProgressUpdate::Stage(label));
                                    }
                                    "tool" => {}
                                    other => {
                                        emit_progress(AiProgressUpdate::Stage(format!(
                                            "执行步骤: {} ({})",
                                            other, state
                                        )));
                                    }
                                }
                            }
                        }
                        for key in ["thinking_delta", "thought", "reasoning_delta"] {
                            if let Some(td) = su.get(key).and_then(|s| s.as_str()) {
                                if !td.is_empty() {
                                    emit_progress(AiProgressUpdate::ThinkingDelta(td.to_string()));
                                }
                            }
                        }
                        if let Some(delta) = su.get("text_delta").and_then(|s| s.as_str()) {
                            if !delta.is_empty() {
                                stream_text_buf.push_str(delta);
                                emit_progress(AiProgressUpdate::TextDelta(delta.to_string()));
                            }
                        }
                    }
                }
                Some("result") => {
                    if let Some(res) = v.get("result") {
                        if let Some(cid) = res.get("conversation_id").and_then(|s| s.as_str()) {
                            if !cid.trim().is_empty() {
                                stream_sid = Some(cid.to_string());
                            }
                        }
                        if let Some(resp) = res.get("response").and_then(|r| r.as_str()) {
                            if !resp.trim().is_empty() {
                                stream_result = Some(resp.to_string());
                            }
                        }
                        let dur = res.get("duration_seconds").and_then(|d| d.as_f64());
                        let in_tok = res
                            .get("usage")
                            .and_then(|u| u.get("input_tokens"))
                            .and_then(|n| n.as_u64());
                        let out_tok = res
                            .get("usage")
                            .and_then(|u| u.get("output_tokens"))
                            .and_then(|n| n.as_u64());
                        if let (Some(d), Some(it), Some(ot)) = (dur, in_tok, out_tok) {
                            emit_progress(AiProgressUpdate::LogLine(format!(
                                "完成 {:.1}s ({} in / {} out)",
                                d, it, ot
                            )));
                        }
                    }
                }
                _ => {}
            }
        })?;

        if want_json_envelope {
            if let Some(resp) = stream_result {
                return Ok(AiPromptOutput {
                    text: resp,
                    session_id: stream_sid.or_else(|| session_id.map(|s| s.to_string())),
                });
            }
            if !stream_text_buf.trim().is_empty() {
                return Ok(AiPromptOutput {
                    text: stream_text_buf,
                    session_id: stream_sid.or_else(|| session_id.map(|s| s.to_string())),
                });
            }
            // Backwards-compatible fallback for single-JSON or mock CLI outputs
            let trimmed = raw.trim();
            let parsed_val = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
                crate::ai::extract_json_block(trimmed)
                    .and_then(|b| serde_json::from_str::<Value>(b).ok())
            });
            if let Some(v) = parsed_val {
                let sid = v
                    .get("conversation_id")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| stream_sid.clone())
                    .or_else(|| session_id.map(|s| s.to_string()));
                if let Some(inner) = v.get("response").and_then(|r| r.as_str()) {
                    if !inner.trim().is_empty() {
                        return Ok(AiPromptOutput {
                            text: inner.to_string(),
                            session_id: sid,
                        });
                    }
                }
            }
        }
        Ok(AiPromptOutput {
            text: raw,
            session_id: stream_sid.or_else(|| session_id.map(|s| s.to_string())),
        })
    }
}

pub struct GeminiCliProvider {
    pub model: Option<String>,
    pub timeout_secs: u64,
}

impl AiProvider for GeminiCliProvider {
    fn id(&self) -> &str {
        "gemini"
    }

    fn display_name(&self) -> &str {
        "Gemini CLI (Local Subscription)"
    }

    fn run_prompt_with_session(
        &self,
        prompt: &str,
        work_dir: &Path,
        json_mode: bool,
        session_id: Option<&str>,
        persist_session: bool,
    ) -> Result<AiPromptOutput> {
        let want_json_envelope =
            json_mode || persist_session || session_id.is_some() || crate::ai::has_progress_hook();
        let mut args = vec![
            "-p",
            "Process the full review prompt provided on stdin.",
            "--approval-mode",
            "plan",
            "-o",
            if want_json_envelope { "json" } else { "text" },
        ];
        if let Some(sid) = session_id {
            args.push("--resume");
            args.push(sid);
        }
        if let Some(m) = &self.model {
            args.push("-m");
            args.push(m);
        }

        let raw = run_with_stdin("gemini", &args, prompt, work_dir, self.timeout_secs)?;
        if raw.contains("Please visit the following URL to authorize")
            || raw.contains("invalid_grant")
            || raw.contains("Failed to authenticate")
        {
            bail!(
                "Gemini CLI OAuth 授权已过期或未登录 (invalid_grant)，请先在终端直接运行 `gemini` 完成授权登录，或改用 `--ai agy` / `--ai codex`"
            );
        }
        if want_json_envelope {
            let trimmed = raw.trim();
            let parsed_val = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
                crate::ai::extract_json_block(trimmed)
                    .and_then(|b| serde_json::from_str::<Value>(b).ok())
            });
            if let Some(v) = parsed_val {
                let sid = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| session_id.map(|s| s.to_string()));
                if let Some(inner) = v.get("response").and_then(|r| r.as_str()) {
                    return Ok(AiPromptOutput {
                        text: inner.to_string(),
                        session_id: sid,
                    });
                }
            }
        }
        Ok(AiPromptOutput {
            text: raw,
            session_id: session_id.map(|s| s.to_string()),
        })
    }
}

pub struct CustomCliProvider {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_secs: u64,
}

impl AiProvider for CustomCliProvider {
    fn id(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn run_prompt_with_session(
        &self,
        prompt: &str,
        work_dir: &Path,
        _json_mode: bool,
        _session_id: Option<&str>,
        _persist_session: bool,
    ) -> Result<AiPromptOutput> {
        let arg_refs: Vec<&str> = self.args.iter().map(|s| s.as_str()).collect();
        let text = run_with_stdin(&self.command, &arg_refs, prompt, work_dir, self.timeout_secs)?;
        Ok(AiPromptOutput {
            text,
            session_id: None,
        })
    }
}
