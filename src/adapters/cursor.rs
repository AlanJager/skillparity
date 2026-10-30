//! Cursor Agent CLI adapter.
//!
//! Installs the skill at `.cursor/skills/<name>/` and `.agents/skills/<name>/`.
//! Invokes `agent -p ... --force --trust --output-format stream-json`.
//! `readToolCall` / `writeToolCall` are normalized to Read / Write.
//! Skips when the CLI or credentials are missing.

use crate::adapters::parse::{
    add_unique, canonical_tool_name, detect_skill_trigger, extract_json_events,
    file_path_from_input, parse_maybe_json, skill_ref_from_input, tools_from_plain_text,
};
use crate::model::AdapterResult;
use crate::process::{env_nonempty, timeout_from_env, CommandOutput, CommandRunner, CommandSpec};
use crate::skill_fs::{collect_output_files, install_cursor_skill, read_skill_name};
use crate::which_path::find_in_env;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const HARNESS: &str = "cursor";
const SKIP_NO_BIN: &str = "Cursor Agent CLI not found (`cursor-agent` or `agent`). Install from https://cursor.com/docs/cli/overview or run with --harness mock.";
const SKIP_IDENTITY: &str = "`agent` is on PATH but does not identify as the Cursor Agent CLI, and `cursor-agent` was not found. Skipping.";
const SKIP_NO_AUTH: &str = "Cursor Agent CLI is installed but not authenticated (no CURSOR_API_KEY, and whoami/status did not show a login). Skipping so CI stays green without secrets.";

const READ_TOOLS: &[&str] = &["Read", "Glob", "Grep"];
const WRITE_TOOLS: &[&str] = &["Write", "Edit"];

#[derive(Debug, Clone)]
pub enum BinChoice {
    Search,
    Missing,
    Path(PathBuf),
}

pub struct CursorRequest<'a> {
    pub sandbox_dir: &'a Path,
    pub skill_dir: &'a Path,
    pub prompt: &'a str,
    pub skill_name: Option<&'a str>,
    pub env: &'a HashMap<String, String>,
    pub bin: BinChoice,
    pub identity_ok: Option<bool>,
    pub logged_in: Option<bool>,
}

impl<'a> CursorRequest<'a> {
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
            skill_name: None,
            env,
            bin: BinChoice::Search,
            identity_ok: None,
            logged_in: None,
        }
    }

    pub fn with_skill_name(mut self, skill_name: &'a str) -> Self {
        self.skill_name = Some(skill_name);
        self
    }
}

#[derive(Debug, Clone)]
pub struct FoundCli {
    pub bin: Option<PathBuf>,
    pub identity_ok: bool,
    pub via: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub ok: bool,
    pub skip_reason: Option<String>,
}

pub fn build_cursor_args(prompt: &str, workspace: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        prompt.to_string(),
        "--force".into(),
        "--trust".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--workspace".into(),
        workspace.display().to_string(),
    ]
}

pub fn assess_cursor_availability(
    bin: Option<&Path>,
    identity_ok: bool,
    logged_in: bool,
    env: &HashMap<String, String>,
) -> Availability {
    if bin.is_none() {
        return Availability {
            ok: false,
            skip_reason: Some(SKIP_NO_BIN.into()),
        };
    }
    if !identity_ok {
        return Availability {
            ok: false,
            skip_reason: Some(SKIP_IDENTITY.into()),
        };
    }
    if env_nonempty(env, "CURSOR_API_KEY") || logged_in {
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

pub fn parse_cursor_output(stdout: &str, stderr: &str, skill_name: Option<&str>) -> ParsedOutput {
    let text = format!("{stdout}\n{stderr}");
    let events = extract_json_events(&text);
    let mut called_tools = Vec::new();
    let mut skill_refs = Vec::new();
    let mut read_paths = Vec::new();
    let mut artifacts = Vec::new();
    let mut assistant_texts = Vec::new();
    let mut seen_calls = HashSet::new();
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
                            .unwrap_or_else(|| "cursor result is_error".into()),
                    );
                }
            }
            if event.get("type").and_then(Value::as_str) == Some("assistant") {
                collect_assistant_text(event, &mut assistant_texts);
            }
            if event.get("type").and_then(Value::as_str) == Some("tool_call") {
                let Some(tool_call) = event.get("tool_call").and_then(Value::as_object) else {
                    continue;
                };
                if let Some(call_id) = call_id_of(event) {
                    if !seen_calls.insert(call_id) {
                        continue;
                    }
                }
                record_cursor_tool_call(
                    tool_call,
                    &mut called_tools,
                    &mut skill_refs,
                    &mut read_paths,
                    &mut artifacts,
                );
            }
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

fn collect_assistant_text(event: &Value, texts: &mut Vec<String>) {
    let Some(content) = event.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                texts.push(text.to_string());
            }
        }
    }
}

fn record_cursor_tool_call(
    tool_call: &Map<String, Value>,
    called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
) {
    for (key, value) in tool_call {
        if key == "function" {
            if let Some(obj) = value.as_object() {
                if let Some(name) = obj.get("name").and_then(Value::as_str) {
                    add_unique(called_tools, name);
                    let args = obj
                        .get("arguments")
                        .map(parse_maybe_json)
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    note_input(
                        called_tools,
                        skill_refs,
                        read_paths,
                        artifacts,
                        &canonical_tool_name(name),
                        &args,
                    );
                }
            }
            continue;
        }
        let canon = canonical_tool_name(key);
        add_unique(called_tools, key);
        let args = value
            .as_object()
            .and_then(|obj| obj.get("args").or_else(|| obj.get("input")))
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        note_input(
            called_tools,
            skill_refs,
            read_paths,
            artifacts,
            &canon,
            &args,
        );
    }
}

fn note_input(
    _called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
    canon: &str,
    input: &Value,
) {
    if canon == "Skill" {
        skill_refs.push(skill_ref_from_input(input));
    }
    let file_path = file_path_from_input(input);
    if file_path.is_empty() {
        return;
    }
    if READ_TOOLS.contains(&canon) {
        read_paths.push(file_path.clone());
    }
    if WRITE_TOOLS.contains(&canon) {
        artifacts.push(file_path);
    }
}

fn call_id_of(event: &Value) -> Option<String> {
    match event.get("call_id")? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
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

fn cursor_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new("cursor")
            .case_insensitive(true)
            .build()
            .expect("cursor name")
    })
}

fn cursor_help_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(r"cursor agent|cursor\.com/docs/cli|Cursor CLI")
            .case_insensitive(true)
            .build()
            .expect("cursor help")
    })
}

fn logged_out_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(
            r"not logged in|not authenticated|login required|no credentials|no api key",
        )
        .case_insensitive(true)
        .build()
        .expect("logged out")
    })
}

fn output_of(
    runner: &dyn CommandRunner,
    bin: &Path,
    args: &[&str],
    env: &HashMap<String, String>,
    timeout: Duration,
    max_buffer: usize,
) -> CommandOutput {
    runner.run(&CommandSpec::new(
        bin,
        args.iter().map(|arg| (*arg).to_string()).collect(),
        None,
        env.clone(),
        timeout,
        max_buffer,
    ))
}

pub fn cursor_identifies(
    bin: &Path,
    env: &HashMap<String, String>,
    runner: &dyn CommandRunner,
) -> bool {
    let version = output_of(
        runner,
        bin,
        &["--version"],
        env,
        Duration::from_secs(8),
        2 * 1024 * 1024,
    );
    if cursor_name_re().is_match(&version.text()) {
        return true;
    }
    let about = output_of(
        runner,
        bin,
        &["about"],
        env,
        Duration::from_secs(8),
        2 * 1024 * 1024,
    );
    if cursor_name_re().is_match(&about.text()) {
        return true;
    }
    let help = output_of(
        runner,
        bin,
        &["--help"],
        env,
        Duration::from_secs(8),
        2 * 1024 * 1024,
    );
    cursor_help_re().is_match(&help.text())
}

pub fn find_cursor_cli(env: &HashMap<String, String>, runner: &dyn CommandRunner) -> FoundCli {
    if let Some(cursor_agent) = find_in_env("cursor-agent", env) {
        return FoundCli {
            bin: Some(cursor_agent),
            identity_ok: true,
            via: Some("cursor-agent".into()),
        };
    }
    let Some(agent) = find_in_env("agent", env) else {
        return FoundCli {
            bin: None,
            identity_ok: false,
            via: None,
        };
    };
    let identity_ok = cursor_identifies(&agent, env, runner);
    FoundCli {
        bin: Some(agent),
        identity_ok,
        via: Some("agent".into()),
    }
}

fn probe_auth(bin: &Path, env: &HashMap<String, String>, runner: &dyn CommandRunner) -> bool {
    let attempts: &[&[&str]] = &[
        &["whoami", "--format", "json"],
        &["status", "--format", "json"],
        &["whoami"],
        &["status"],
    ];
    for args in attempts {
        let output = output_of(
            runner,
            bin,
            args,
            env,
            Duration::from_secs(15),
            2 * 1024 * 1024,
        );
        if output.error.is_some() && output.status.is_none() {
            continue;
        }
        let text = output.text();
        if output.status == Some(0) && !text.trim().is_empty() && !logged_out_re().is_match(&text) {
            return true;
        }
    }
    false
}

pub fn run_task(request: CursorRequest<'_>, runner: &dyn CommandRunner) -> AdapterResult {
    let found = match request.bin {
        BinChoice::Search => find_cursor_cli(request.env, runner),
        BinChoice::Missing => FoundCli {
            bin: None,
            identity_ok: false,
            via: None,
        },
        BinChoice::Path(path) => FoundCli {
            bin: Some(path),
            identity_ok: request.identity_ok != Some(false),
            via: Some("injected".into()),
        },
    };
    let logged_in = if env_nonempty(request.env, "CURSOR_API_KEY") {
        true
    } else {
        match (&found.bin, found.identity_ok, request.logged_in) {
            (Some(bin), true, None) => probe_auth(bin, request.env, runner),
            (Some(_), true, Some(flag)) => flag,
            _ => false,
        }
    };
    let gate = assess_cursor_availability(
        found.bin.as_deref(),
        found.identity_ok,
        logged_in,
        request.env,
    );
    if !gate.ok {
        return AdapterResult::skipped(
            HARNESS,
            gate.skip_reason.unwrap_or_else(|| "adapter skipped".into()),
        );
    }
    let bin = found.bin.expect("availability requires a binary");
    let installed = match install_cursor_skill(request.skill_dir, request.sandbox_dir) {
        Ok(installed) => installed,
        Err(err) => return AdapterResult::errored(HARNESS, err),
    };
    let resolved_name = request
        .skill_name
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or(installed.skill_name.clone());
    let resolved_name = if resolved_name.is_empty() {
        read_skill_name(request.skill_dir).unwrap_or(installed.skill_name.clone())
    } else {
        resolved_name
    };
    let timeout = timeout_from_env(request.env, 180_000);
    let args = build_cursor_args(request.prompt, request.sandbox_dir);
    let output = runner.run(&CommandSpec::new(
        &bin,
        args.clone(),
        Some(request.sandbox_dir.to_path_buf()),
        request.env.clone(),
        timeout,
        16 * 1024 * 1024,
    ));
    let mut parsed = parse_cursor_output(&output.stdout, &output.stderr, Some(&resolved_name));
    let files = collect_output_files(request.sandbox_dir, &parsed.artifacts);
    if files.contains("summary.md") && !parsed.called_tools.iter().any(|tool| tool == "Write") {
        parsed.called_tools.push("Write".into());
    }
    let error = task_error(&output, &parsed, timeout);
    let meta = serde_json::to_value(CursorMeta {
        argv: &args,
        exit_code: output.status,
        session_id: parsed.session_id.clone(),
        via: found.via.unwrap_or_else(|| "agent".into()),
        skill_dirs: installed
            .dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect(),
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
struct CursorMeta<'a> {
    argv: &'a [String],
    #[serde(rename = "exitCode")]
    exit_code: Option<i32>,
    session_id: Option<String>,
    via: String,
    skill_dirs: Vec<String>,
}

fn task_error(output: &CommandOutput, parsed: &ParsedOutput, timeout: Duration) -> Option<String> {
    if output.timed_out {
        return Some(format!(
            "cursor agent timed out after {}ms",
            timeout.as_millis()
        ));
    }
    if let Some(error) = &output.error {
        return Some(error.clone());
    }
    if parsed.is_error {
        return Some(
            parsed
                .error
                .clone()
                .unwrap_or_else(|| "cursor result is_error".into()),
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
                format!("cursor agent exited {status}")
            } else {
                format!(
                    "cursor agent exited {status}: {}",
                    detail.chars().take(500).collect::<String>()
                )
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CommandOutput, StaticRunner, StdinMode, SystemRunner};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn ndjson(events: &[&str]) -> String {
        events.join("\n")
    }

    fn env_with_path(path: &Path) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("PATH".into(), path.display().to_string());
        env
    }

    fn write_exec(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn parse_normalizes_tool_call_names_and_ignores_duplicate_completion() {
        let stdout = ndjson(&[
            r#"{"type":"system","subtype":"init","session_id":"c1"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll read the diff."}]},"session_id":"c1"}"#,
            r#"{"type":"tool_call","subtype":"started","call_id":"call-read","tool_call":{"readToolCall":{"args":{"path":"pr.diff"}}}}"#,
            r#"{"type":"tool_call","subtype":"completed","call_id":"call-read","tool_call":{"readToolCall":{"args":{"path":"pr.diff"},"result":{"success":{}}}}}"#,
            r##"{"type":"tool_call","subtype":"completed","call_id":"call-write","tool_call":{"writeToolCall":{"args":{"path":"summary.md","fileText":"# Update src/hello.js\n"},"result":{"success":{"path":"/tmp/summary.md"}}}}}"##,
            r#"{"type":"tool_call","subtype":"started","call_id":"call-fn","tool_call":{"function":{"name":"Grep","arguments":"{\"path\":\"src\"}"}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done","session_id":"c1"}"#,
        ]);
        let parsed = parse_cursor_output(&stdout, "", Some("summarize-pr"));
        assert_eq!(
            parsed.called_tools,
            vec!["Read".to_string(), "Write".to_string(), "Grep".to_string()]
        );
        assert!(!parsed.skill_triggered);
        assert!(parsed.artifacts.iter().any(|path| path == "summary.md"));
        assert!(parsed.stdout.contains("I'll read the diff"));
        assert!(parsed.stdout.contains("Done"));
    }

    #[test]
    fn skill_counts_when_skill_md_is_read() {
        let stdout = r#"{"type":"tool_call","subtype":"started","call_id":"s","tool_call":{"readToolCall":{"args":{"path":".cursor/skills/summarize-pr/SKILL.md"}}}}"#;
        let parsed = parse_cursor_output(stdout, "", Some("summarize-pr"));
        assert!(parsed.skill_triggered);
        assert_eq!(parsed.called_tools, vec!["Read".to_string()]);
    }

    #[test]
    fn build_args_matches_documented_print_mode() {
        let args = build_cursor_args("Summarize the PR", Path::new("/tmp/sandbox"));
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "Summarize the PR".to_string(),
                "--force".to_string(),
                "--trust".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--workspace".to_string(),
                "/tmp/sandbox".to_string(),
            ]
        );
    }

    #[test]
    fn availability_skips_without_binary_identity_or_credentials() {
        assert!(
            assess_cursor_availability(None, false, false, &HashMap::new())
                .skip_reason
                .unwrap()
                .contains("not found")
        );
        assert!(assess_cursor_availability(
            Some(Path::new("/usr/bin/agent")),
            false,
            false,
            &HashMap::new()
        )
        .skip_reason
        .unwrap()
        .contains("does not identify"));
        assert!(assess_cursor_availability(
            Some(Path::new("/usr/bin/agent")),
            true,
            false,
            &HashMap::new()
        )
        .skip_reason
        .unwrap()
        .contains("not authenticated"));
        let mut env = HashMap::new();
        env.insert("CURSOR_API_KEY".into(), "test".into());
        assert!(
            assess_cursor_availability(Some(Path::new("/usr/bin/agent")), true, false, &env).ok
        );
    }

    #[test]
    fn find_cli_prefers_cursor_agent_and_rejects_a_generic_agent() {
        let dir = tempfile::tempdir().unwrap();
        let cursor_agent = dir.path().join("cursor-agent");
        write_exec(&cursor_agent, "#!/bin/sh\necho cursor-agent\n");
        let preferred = find_cursor_cli(&env_with_path(dir.path()), &SystemRunner);
        assert_eq!(preferred.bin, Some(cursor_agent));
        assert!(preferred.identity_ok);
        assert_eq!(preferred.via.as_deref(), Some("cursor-agent"));

        let other = tempfile::tempdir().unwrap();
        write_exec(&other.path().join("agent"), "#!/bin/sh\necho vault agent\n");
        let generic = find_cursor_cli(&env_with_path(other.path()), &SystemRunner);
        assert!(!generic.identity_ok);

        let real = tempfile::tempdir().unwrap();
        write_exec(
            &real.path().join("agent"),
            "#!/bin/sh\necho 'Cursor Agent CLI 2026.10'\n",
        );
        let identified = find_cursor_cli(&env_with_path(real.path()), &SystemRunner);
        assert!(identified.identity_ok);
        assert_eq!(identified.via.as_deref(), Some("agent"));
    }

    #[test]
    fn run_installs_skill_roots_and_parses_injected_stream() {
        let sandbox = tempfile::tempdir().unwrap();
        let stdout = ndjson(&[
            r#"{"type":"tool_call","subtype":"started","call_id":"skill","tool_call":{"skillToolCall":{"args":{"skill":"summarize-pr"}}}}"#,
            r#"{"type":"tool_call","subtype":"started","call_id":"read","tool_call":{"readToolCall":{"args":{"path":"pr.diff"}}}}"#,
            r##"{"type":"tool_call","subtype":"started","call_id":"write","tool_call":{"writeToolCall":{"args":{"path":"summary.md","fileText":"# T\n"}}}}"##,
            r#"{"type":"result","is_error":false,"result":"Wrote summary.md"}"#,
        ]);
        let sandbox_path = sandbox.path().to_path_buf();
        let mut runner = StaticRunner::new(CommandOutput {
            status: Some(0),
            stdout,
            stderr: String::new(),
            timed_out: false,
            error: None,
        });
        runner.on_run = Some(Box::new(move || {
            fs::write(
                sandbox_path.join("summary.md"),
                "# Update src/hello.js\n\n- a\n- b\n- c\n",
            )
            .unwrap();
        }));
        let skill = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr");
        let env = HashMap::new();
        let mut request = CursorRequest::new(
            sandbox.path(),
            &skill,
            "Please summarize this PR using the local pr.diff file.",
            &env,
        )
        .with_skill_name("summarize-pr");
        request.bin = BinChoice::Path(PathBuf::from("/opt/agent"));
        request.identity_ok = Some(true);
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(result.skill_triggered);
        assert_eq!(
            result.called_tools,
            vec!["Skill".to_string(), "Read".to_string(), "Write".to_string()]
        );
        assert!(result.files.get("summary.md").unwrap().contains("hello.js"));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].stdin, StdinMode::Ignore);
        assert!(calls[0].args.iter().any(|arg| arg == "--force"));
        assert!(calls[0].args.iter().any(|arg| arg == "--trust"));
        let workspace = calls[0]
            .args
            .iter()
            .position(|arg| arg == "--workspace")
            .unwrap();
        assert_eq!(
            calls[0].args[workspace + 1],
            sandbox.path().display().to_string()
        );
        assert!(sandbox
            .path()
            .join(".cursor/skills/summarize-pr/SKILL.md")
            .exists());
        assert!(sandbox
            .path()
            .join(".agents/skills/summarize-pr/SKILL.md")
            .exists());
        assert!(!sandbox
            .path()
            .join(".cursor/skills/summarize-pr/fixtures")
            .exists());
        let cli: Value = serde_json::from_str(
            &fs::read_to_string(sandbox.path().join(".cursor/cli.json")).unwrap(),
        )
        .unwrap();
        let allow = cli["permissions"]["allow"].as_array().unwrap();
        assert!(allow.iter().any(|value| value == "Read(**)"));
        assert!(allow.iter().any(|value| value == "Write(**)"));
        assert!(cli.get("version").is_none());
    }

    #[test]
    fn run_skips_a_missing_cli_before_writing_the_sandbox() {
        let sandbox = tempfile::tempdir().unwrap();
        let skill = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr");
        let runner = StaticRunner::new(CommandOutput {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            error: None,
        });
        let env = HashMap::new();
        let mut request = CursorRequest::new(sandbox.path(), &skill, "hi", &env);
        request.bin = BinChoice::Missing;
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("not found"));
        assert!(!sandbox.path().join(".cursor").exists());
        assert!(runner.calls.lock().unwrap().is_empty());
    }
}
