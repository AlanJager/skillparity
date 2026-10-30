//! Claude Code CLI adapter.
//!
//! Installs the skill at `<sandbox>/.claude/skills/<name>/SKILL.md` and runs
//! non-interactive print mode. `--bare` is not used. Stdin is ignored.
//! Skips when the CLI or credentials are missing.

use crate::adapters::parse::{
    add_unique, canonical_tool_name, detect_skill_trigger, extract_json_events,
    file_path_from_input, skill_ref_from_input, tools_from_plain_text,
};
use crate::model::AdapterResult;
use crate::process::{env_nonempty, timeout_from_env, CommandOutput, CommandRunner, CommandSpec};
use crate::skill_fs::{absolute_lexical, collect_output_files, install_claude_skill};
use crate::which_path::find_in_env;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const HARNESS: &str = "claude";
const SKIP_NO_BIN: &str = "claude CLI not found on PATH. Install Claude Code (https://code.claude.com/docs/en/cli-reference) or run with --harness mock.";
const SKIP_NO_AUTH: &str = "claude CLI is installed but no credentials are available (`claude auth status` is not logged in, and ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN are unset). Skipping so CI stays green without secrets.";

const READ_TOOLS: &[&str] = &["Read", "Glob", "Grep"];
const WRITE_TOOLS: &[&str] = &["Write", "Edit"];

#[derive(Debug, Clone)]
pub enum BinChoice {
    Search,
    Missing,
    Path(PathBuf),
}

pub struct ClaudeRequest<'a> {
    pub sandbox_dir: &'a Path,
    pub skill_dir: &'a Path,
    pub prompt: &'a str,
    pub env: &'a HashMap<String, String>,
    pub bin: BinChoice,
    pub home_dir: Option<PathBuf>,
    /// `Some` skips `claude auth status` (tests). A key in `env` still wins.
    pub logged_in: Option<bool>,
    pub permission_mode: Option<String>,
    pub max_turns: Option<String>,
}

impl<'a> ClaudeRequest<'a> {
    pub fn new(
        sandbox_dir: &'a Path,
        skill_dir: &'a Path,
        prompt: &'a str,
        env: &'a HashMap<String, String>,
    ) -> Self {
        Self {
            sandbox_dir,
            skill_dir,
            prompt,
            env,
            bin: BinChoice::Search,
            home_dir: None,
            logged_in: None,
            permission_mode: None,
            max_turns: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub ok: bool,
    pub skip_reason: Option<String>,
}

pub fn build_claude_args(
    prompt: &str,
    permission_mode: Option<&str>,
    max_turns: Option<&str>,
) -> Vec<String> {
    let permission_mode = permission_mode
        .filter(|mode| !mode.is_empty())
        .unwrap_or("acceptEdits");
    let max_turns = max_turns.filter(|turns| !turns.is_empty()).unwrap_or("8");
    vec![
        "-p".into(),
        prompt.to_string(),
        "--verbose".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--permission-mode".into(),
        permission_mode.to_string(),
        "--allowedTools".into(),
        "Read,Write,Edit,Glob,Grep,Skill".into(),
        "--max-turns".into(),
        max_turns.to_string(),
    ]
}

pub fn claude_project_slug(cwd: &Path) -> String {
    absolute_lexical(cwd)
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

pub fn claude_transcript_path(cwd: &Path, session_id: &str, home: &Path) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    Some(
        home.join(".claude")
            .join("projects")
            .join(claude_project_slug(cwd))
            .join(format!("{session_id}.jsonl")),
    )
}

pub fn assess_claude_availability(
    bin: Option<&Path>,
    logged_in: bool,
    env: &HashMap<String, String>,
) -> Availability {
    if bin.is_none() {
        return Availability {
            ok: false,
            skip_reason: Some(SKIP_NO_BIN.into()),
        };
    }
    let has_key =
        env_nonempty(env, "ANTHROPIC_API_KEY") || env_nonempty(env, "ANTHROPIC_AUTH_TOKEN");
    if has_key || logged_in {
        return Availability {
            ok: true,
            skip_reason: None,
        };
    }
    Availability {
        ok: false,
        skip_reason: Some(SKIP_NO_AUTH.into()),
    }
}

#[derive(Debug, Clone)]
pub struct ParsedOutput {
    pub called_tools: Vec<String>,
    pub skill_triggered: bool,
    pub stdout: String,
    pub stderr: String,
    pub artifacts: Vec<String>,
    pub session_id: Option<String>,
    pub is_error: bool,
    pub error: Option<String>,
}

pub fn parse_claude_output(stdout: &str, stderr: &str, skill_name: Option<&str>) -> ParsedOutput {
    let text = format!("{stdout}\n{stderr}");
    let events = extract_json_events(&text);
    let mut called_tools = Vec::new();
    let mut skill_refs = Vec::new();
    let mut read_paths = Vec::new();
    let mut artifacts = Vec::new();
    let mut assistant_texts = Vec::new();
    let mut result_text = String::new();
    let mut session_id = None;
    let mut is_error = false;
    let mut error = None;

    if events.is_empty() {
        for tool in tools_from_plain_text(&text) {
            add_unique(&mut called_tools, &tool);
        }
    } else {
        for event in &events {
            if session_id.is_none() {
                session_id =
                    string_field(event, "session_id").or_else(|| string_field(event, "sessionId"));
            }
            if event.get("type").and_then(Value::as_str) == Some("result") {
                if let Some(text) = event.get("result").and_then(Value::as_str) {
                    result_text = text.to_string();
                }
                if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                    is_error = true;
                    error = Some(
                        nonempty_string(event.get("error"))
                            .or_else(|| nonempty_string(event.get("result")))
                            .unwrap_or_else(|| "claude result is_error".into()),
                    );
                }
            }
            let in_assistant = event.get("type").and_then(Value::as_str) == Some("assistant")
                || event.get("role").and_then(Value::as_str) == Some("assistant");
            let mut ctx = VisitCtx {
                called_tools: &mut called_tools,
                skill_refs: &mut skill_refs,
                read_paths: &mut read_paths,
                artifacts: &mut artifacts,
                assistant_texts: &mut assistant_texts,
            };
            visit_claude(event, &mut ctx, in_assistant);
        }
    }

    let mut trigger_texts = assistant_texts.clone();
    if !result_text.is_empty() {
        trigger_texts.push(result_text.clone());
    }
    let skill_triggered =
        detect_skill_trigger(skill_name, &skill_refs, &read_paths, &trigger_texts);
    let mut visible: Vec<String> = assistant_texts
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect();
    if !result_text.is_empty() {
        visible.push(result_text);
    }
    let joined = visible.join("\n");
    ParsedOutput {
        called_tools,
        skill_triggered,
        stdout: if joined.is_empty() {
            stdout.to_string()
        } else {
            joined
        },
        stderr: stderr.to_string(),
        artifacts,
        session_id,
        is_error,
        error,
    }
}

struct VisitCtx<'a> {
    called_tools: &'a mut Vec<String>,
    skill_refs: &'a mut Vec<String>,
    read_paths: &'a mut Vec<String>,
    artifacts: &'a mut Vec<String>,
    assistant_texts: &'a mut Vec<String>,
}

fn visit_claude(node: &Value, ctx: &mut VisitCtx<'_>, in_assistant: bool) {
    match node {
        Value::Array(items) => {
            for item in items {
                visit_claude(item, ctx, in_assistant);
            }
        }
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(name) = map.get("name").and_then(Value::as_str) {
                    let input = map.get("input").cloned().unwrap_or(Value::Null);
                    record_claude_tool(ctx, name, &input);
                }
            }
            if map.get("type").and_then(Value::as_str) == Some("text") && in_assistant {
                if let Some(text) = map.get("text").and_then(Value::as_str) {
                    ctx.assistant_texts.push(text.to_string());
                }
            }
            let role = map.get("role").and_then(Value::as_str);
            let typ = map.get("type").and_then(Value::as_str);
            let child_assistant = if role == Some("assistant") || typ == Some("assistant") {
                true
            } else if role == Some("user") || typ == Some("user") {
                false
            } else {
                in_assistant
            };
            for value in map.values() {
                if value.is_object() || value.is_array() {
                    visit_claude(value, ctx, child_assistant);
                }
            }
        }
        _ => {}
    }
}

fn record_claude_tool(ctx: &mut VisitCtx<'_>, tool_name: &str, input: &Value) {
    add_unique(ctx.called_tools, tool_name);
    let canon = canonical_tool_name(tool_name);
    if canon == "Skill" {
        ctx.skill_refs.push(skill_ref_from_input(input));
    }
    let file_path = file_path_from_input(input);
    if file_path.is_empty() {
        return;
    }
    if READ_TOOLS.contains(&canon.as_str()) {
        ctx.read_paths.push(file_path.clone());
    }
    if WRITE_TOOLS.contains(&canon.as_str()) {
        ctx.artifacts.push(file_path);
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn nonempty_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

fn merge_parse(primary: ParsedOutput, extra: ParsedOutput) -> ParsedOutput {
    let mut called_tools = primary.called_tools;
    for tool in extra.called_tools {
        add_unique(&mut called_tools, &tool);
    }
    let mut artifacts = primary.artifacts;
    for artifact in extra.artifacts {
        if !artifacts.iter().any(|existing| existing == &artifact) {
            artifacts.push(artifact);
        }
    }
    ParsedOutput {
        called_tools,
        artifacts,
        skill_triggered: primary.skill_triggered || extra.skill_triggered,
        stdout: if !primary.stdout.is_empty() {
            primary.stdout
        } else {
            extra.stdout
        },
        stderr: primary.stderr,
        session_id: primary.session_id.or(extra.session_id),
        is_error: primary.is_error || extra.is_error,
        error: match primary.error {
            Some(error) if !error.is_empty() => Some(error),
            _ => extra.error,
        },
    }
}

fn resolve_bin(choice: &BinChoice, env: &HashMap<String, String>) -> Option<PathBuf> {
    match choice {
        BinChoice::Search => find_in_env("claude", env),
        BinChoice::Missing => None,
        BinChoice::Path(path) => Some(path.clone()),
    }
}

fn probe_auth(bin: &Path, env: &HashMap<String, String>, runner: &dyn CommandRunner) -> bool {
    let output = runner.run(&CommandSpec::new(
        bin,
        vec!["auth".into(), "status".into()],
        None,
        env.clone(),
        Duration::from_secs(20),
        16 * 1024 * 1024,
    ));
    if output.status != Some(0) {
        return false;
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(&output.stdout) {
        if parsed.get("authMethod").and_then(Value::as_str) == Some("none")
            || parsed.get("loggedIn") == Some(&Value::Bool(false))
        {
            return false;
        }
    }
    true
}

pub fn run_task(mut request: ClaudeRequest<'_>, runner: &dyn CommandRunner) -> AdapterResult {
    if request.permission_mode.is_none() {
        request.permission_mode = request
            .env
            .get("SKILLPARITY_CLAUDE_PERMISSION_MODE")
            .cloned();
    }
    if request.max_turns.is_none() {
        request.max_turns = request.env.get("SKILLPARITY_CLAUDE_MAX_TURNS").cloned();
    }
    if request.home_dir.is_none() {
        request.home_dir = Some(
            request
                .env
                .get("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp")),
        );
    }
    run_request(&request, runner)
}

fn run_request(request: &ClaudeRequest<'_>, runner: &dyn CommandRunner) -> AdapterResult {
    let bin = resolve_bin(&request.bin, request.env);
    let has_key = env_nonempty(request.env, "ANTHROPIC_API_KEY")
        || env_nonempty(request.env, "ANTHROPIC_AUTH_TOKEN");
    let logged_in = match &bin {
        None => false,
        Some(_) if has_key => true,
        Some(path) => match request.logged_in {
            Some(flag) => flag,
            None => probe_auth(path, request.env, runner),
        },
    };
    let gate = assess_claude_availability(bin.as_deref(), logged_in, request.env);
    if !gate.ok {
        return AdapterResult::skipped(
            HARNESS,
            gate.skip_reason.unwrap_or_else(|| "adapter skipped".into()),
        );
    }
    let bin = bin.expect("availability requires a binary");
    let installed = match install_claude_skill(request.skill_dir, request.sandbox_dir) {
        Ok(installed) => installed,
        Err(err) => return AdapterResult::errored(HARNESS, err),
    };
    let timeout = timeout_from_env(request.env, 180_000);
    let args = build_claude_args(
        request.prompt,
        request.permission_mode.as_deref(),
        request.max_turns.as_deref(),
    );
    let output = runner.run(&CommandSpec::new(
        &bin,
        args.clone(),
        Some(request.sandbox_dir.to_path_buf()),
        request.env.clone(),
        timeout,
        16 * 1024 * 1024,
    ));
    let mut parsed =
        parse_claude_output(&output.stdout, &output.stderr, Some(&installed.skill_name));
    if parsed.session_id.is_some() && parsed.called_tools.is_empty() {
        if let Some(session_id) = parsed.session_id.clone() {
            let home_dir = request
                .home_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            if let Some(transcript) =
                claude_transcript_path(request.sandbox_dir, &session_id, &home_dir)
            {
                if transcript.is_file() {
                    if let Ok(disk) = fs::read_to_string(&transcript) {
                        let extra = parse_claude_output(&disk, "", Some(&installed.skill_name));
                        parsed = merge_parse(parsed, extra);
                    }
                }
            }
        }
    }
    let files = collect_output_files(request.sandbox_dir, &parsed.artifacts);
    if files.contains("summary.md") && !parsed.called_tools.iter().any(|tool| tool == "Write") {
        parsed.called_tools.push("Write".into());
    }
    let error = task_error(&output, &parsed, timeout);
    let permission_mode = args
        .iter()
        .position(|arg| arg == "--permission-mode")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| "acceptEdits".into());
    let meta = serde_json::to_value(ClaudeMeta {
        argv: &args,
        exit_code: output.status,
        session_id: parsed.session_id.clone(),
        permission_mode,
        skill_dir: installed.dest.display().to_string(),
    })
    .ok();
    AdapterResult {
        error,
        skill_triggered: parsed.skill_triggered,
        called_tools: parsed.called_tools,
        files,
        stdout: parsed.stdout,
        harness: HARNESS.into(),
        skipped: false,
        skip_reason: None,
        meta,
    }
}

#[derive(Serialize)]
struct ClaudeMeta<'a> {
    argv: &'a [String],
    #[serde(rename = "exitCode")]
    exit_code: Option<i32>,
    session_id: Option<String>,
    permission_mode: String,
    skill_dir: String,
}

fn task_error(output: &CommandOutput, parsed: &ParsedOutput, timeout: Duration) -> Option<String> {
    if output.timed_out {
        return Some(format!("claude timed out after {}ms", timeout.as_millis()));
    }
    if let Some(error) = &output.error {
        return Some(error.clone());
    }
    if parsed.is_error {
        return Some(
            parsed
                .error
                .clone()
                .unwrap_or_else(|| "claude result is_error".into()),
        );
    }
    if let Some(status) = output.status {
        if status != 0 && parsed.stdout.trim().is_empty() && parsed.called_tools.is_empty() {
            let detail = if !output.stderr.trim().is_empty() {
                output.stderr.trim()
            } else {
                output.stdout.trim()
            };
            return Some(if detail.is_empty() {
                format!("claude exited {status}")
            } else {
                format!("claude exited {status}: {}", clip(detail, 500))
            });
        }
    }
    None
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CommandOutput, StaticRunner, StdinMode};
    use crate::skill_fs::install_claude_skill;
    use std::fs;

    fn stream() -> String {
        [
            r#"{"type":"system","subtype":"init","session_id":"sess-1","tools":["Read","Write","Skill"]}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Please summarize this PR using the local pr.diff file."}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll use the summarize-pr skill."},{"type":"tool_use","name":"Skill","input":{"skill":"summarize-pr"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"pr.diff"}}]}}"#,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"summary.md","content":"# Update src/hello.js\n"}}]}}"##,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Wrote summary.md","session_id":"sess-1"}"#,
        ]
        .join("\n")
    }

    fn env_empty() -> HashMap<String, String> {
        HashMap::new()
    }

    fn ok_output(stdout: &str) -> CommandOutput {
        CommandOutput {
            status: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            timed_out: false,
            error: None,
        }
    }

    #[test]
    fn parse_reads_stream_json_tool_use() {
        let parsed = parse_claude_output(&stream(), "", Some("summarize-pr"));
        assert_eq!(
            parsed.called_tools,
            vec!["Skill".to_string(), "Read".to_string(), "Write".to_string()]
        );
        assert!(parsed.skill_triggered);
        assert_eq!(parsed.session_id.as_deref(), Some("sess-1"));
        assert!(!parsed.is_error);
        assert!(parsed.artifacts.iter().any(|path| path == "summary.md"));
        assert!(parsed.stdout.contains("Wrote summary.md"));
        assert!(parsed.stdout.contains("summarize-pr skill"));
        assert!(!parsed.stdout.contains("Please summarize this PR"));
    }

    #[test]
    fn parse_ignores_prose_and_tool_catalog() {
        let prose = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I will Write a poem about the weather."}]}}"#;
        let parsed = parse_claude_output(prose, "Tool catalog: Read Write", Some("summarize-pr"));
        assert!(parsed.called_tools.is_empty());
        assert!(!parsed.skill_triggered);
    }

    #[test]
    fn plain_text_fallback_does_not_imply_a_skill() {
        let parsed =
            parse_claude_output("Using tool: Read\nTool: Write\n", "", Some("summarize-pr"));
        assert_eq!(
            parsed.called_tools,
            vec!["Read".to_string(), "Write".to_string()]
        );
        assert!(!parsed.skill_triggered);
    }

    #[test]
    fn read_of_skill_md_is_a_trigger() {
        let stdout = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":".claude/skills/summarize-pr/SKILL.md"}}]}}"#;
        let parsed = parse_claude_output(stdout, "", Some("summarize-pr"));
        assert!(parsed.skill_triggered);
        assert_eq!(parsed.called_tools, vec!["Read".to_string()]);
    }

    #[test]
    fn different_skill_name_does_not_trigger() {
        let stdout = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"other-skill"}}]}}"#;
        let parsed = parse_claude_output(stdout, "", Some("summarize-pr"));
        assert!(!parsed.skill_triggered);
        assert_eq!(parsed.called_tools, vec!["Skill".to_string()]);
    }

    #[test]
    fn single_json_result_object() {
        let stdout = r#"{"type":"result","result":"pr.diff is missing","is_error":false,"session_id":"abc"}"#;
        let parsed = parse_claude_output(stdout, "", None);
        assert_eq!(parsed.stdout, "pr.diff is missing");
        assert_eq!(parsed.session_id.as_deref(), Some("abc"));
        assert!(!parsed.skill_triggered);
    }

    #[test]
    fn build_args_is_print_mode_without_bare_or_bypass() {
        let args = build_claude_args("Summarize the PR", None, None);
        assert_eq!(
            &args[..2],
            ["-p".to_string(), "Summarize the PR".to_string()]
        );
        assert!(args.iter().any(|arg| arg == "--verbose"));
        let format_at = args
            .iter()
            .position(|arg| arg == "--output-format")
            .unwrap();
        assert_eq!(args[format_at + 1], "stream-json");
        let mode_at = args
            .iter()
            .position(|arg| arg == "--permission-mode")
            .unwrap();
        assert_eq!(args[mode_at + 1], "acceptEdits");
        let tools_at = args.iter().position(|arg| arg == "--allowedTools").unwrap();
        assert_eq!(args[tools_at + 1], "Read,Write,Edit,Glob,Grep,Skill");
        assert!(!args.iter().any(|arg| arg == "--bare"));
        assert!(!args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"));
        assert!(!args.iter().any(|arg| arg == "bypassPermissions"));
    }

    #[test]
    fn project_slug_and_transcript_path() {
        assert_eq!(
            claude_project_slug(Path::new("/tmp/skillparity-abc")),
            "-tmp-skillparity-abc"
        );
        let file = claude_transcript_path(
            Path::new("/tmp/skillparity-abc"),
            "sess-9",
            Path::new("/home/me"),
        )
        .unwrap();
        assert_eq!(
            file,
            PathBuf::from("/home/me/.claude/projects/-tmp-skillparity-abc/sess-9.jsonl")
        );
    }

    #[test]
    fn availability_skips_without_cli_or_credentials() {
        let missing = assess_claude_availability(None, false, &env_empty());
        assert!(!missing.ok);
        assert!(missing.skip_reason.unwrap().contains("not found on PATH"));

        let no_auth =
            assess_claude_availability(Some(Path::new("/usr/bin/claude")), false, &env_empty());
        assert!(no_auth.skip_reason.unwrap().contains("no credentials"));

        let mut env = env_empty();
        env.insert("ANTHROPIC_API_KEY".into(), "test-key".into());
        let key = assess_claude_availability(Some(Path::new("/usr/bin/claude")), false, &env);
        assert!(key.ok);
    }

    #[test]
    fn run_skips_before_install_when_claude_is_absent() {
        let sandbox = tempfile::tempdir().unwrap();
        let skill = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr");
        let runner = StaticRunner::new(ok_output(""));
        let env = env_empty();
        let mut request = ClaudeRequest::new(sandbox.path(), &skill, "hi", &env);
        request.bin = BinChoice::Missing;
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("not found on PATH"));
        assert!(!sandbox.path().join(".claude").exists());
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn run_installs_skill_and_parses_injected_process() {
        let sandbox = tempfile::tempdir().unwrap();
        fs::write(sandbox.path().join("pr.diff"), "diff\n").unwrap();
        let home = tempfile::tempdir().unwrap();
        let skill = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr");
        let sandbox_path = sandbox.path().to_path_buf();
        let mut runner = StaticRunner::new(ok_output(&stream()));
        runner.on_run = Some(Box::new(move || {
            fs::write(
                sandbox_path.join("summary.md"),
                "# Update src/hello.js\n\n- a\n- b\n- c\n",
            )
            .unwrap();
        }));
        let env = env_empty();
        let mut request = ClaudeRequest::new(
            sandbox.path(),
            &skill,
            "Please summarize this PR using the local pr.diff file.",
            &env,
        );
        request.bin = BinChoice::Path(PathBuf::from("/opt/claude"));
        request.home_dir = Some(home.path().to_path_buf());
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(!result.skipped);
        assert!(result.error.is_none());
        assert!(result.skill_triggered);
        assert_eq!(
            result.called_tools,
            vec!["Skill".to_string(), "Read".to_string(), "Write".to_string()]
        );
        assert!(result
            .files
            .get("summary.md")
            .unwrap()
            .contains("# Update src/hello.js"));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, PathBuf::from("/opt/claude"));
        assert_eq!(calls[0].stdin, StdinMode::Ignore);
        assert_eq!(calls[0].cwd.as_deref(), Some(sandbox.path()));
        let mode_at = calls[0]
            .args
            .iter()
            .position(|arg| arg == "--permission-mode")
            .unwrap();
        assert_eq!(calls[0].args[mode_at + 1], "acceptEdits");
        assert!(sandbox
            .path()
            .join(".claude/skills/summarize-pr/SKILL.md")
            .exists());
        assert!(!sandbox
            .path()
            .join(".claude/skills/summarize-pr/fixtures")
            .exists());
        assert!(!sandbox
            .path()
            .join(".claude/skills/summarize-pr/parity.yaml")
            .exists());
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(sandbox.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "acceptEdits");
    }

    #[test]
    fn run_reads_session_transcript_when_stdout_has_no_tools() {
        let sandbox = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session_id = "11111111-1111-4111-8111-111111111111";
        let transcript = claude_transcript_path(sandbox.path(), session_id, home.path()).unwrap();
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{\"file_path\":\"pr.diff\"}}]}}\n",
        )
        .unwrap();
        let stdout = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"result":"pr.diff is missing or empty","session_id":"{session_id}"}}"#
        );
        let mut env = env_empty();
        env.insert("ANTHROPIC_API_KEY".into(), "test-key".into());
        let runner = StaticRunner::new(ok_output(&stdout));
        let skill = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr");
        let mut request = ClaudeRequest::new(
            sandbox.path(),
            &skill,
            "Summarize the pull request from pr.diff.",
            &env,
        );
        request.bin = BinChoice::Path(PathBuf::from("/opt/claude"));
        request.home_dir = Some(home.path().to_path_buf());
        let result = run_task(request, &runner);
        assert!(!result.skill_triggered);
        assert_eq!(result.called_tools, vec!["Read".to_string()]);
        assert!(result.stdout.contains("missing or empty"));
        assert!(result.error.is_none());
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn install_is_covered_by_skill_fs() {
        let skill = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr");
        let sandbox = tempfile::tempdir().unwrap();
        let installed = install_claude_skill(&skill, sandbox.path()).unwrap();
        assert_eq!(installed.skill_name, "summarize-pr");
    }

    #[test]
    fn run_surfaces_a_hard_failure() {
        let sandbox = tempfile::tempdir().unwrap();
        let skill = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr");
        let runner = StaticRunner::new(CommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "Invalid API key".into(),
            timed_out: false,
            error: None,
        });
        let env = env_empty();
        let mut request = ClaudeRequest::new(sandbox.path(), &skill, "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/claude"));
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        let error = result.error.unwrap();
        assert!(error.contains("exited 1"), "{error}");
        assert!(error.contains("Invalid API key"), "{error}");
    }
}
