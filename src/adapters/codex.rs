//! Codex CLI adapter.
//!
//! Installs the skill at `<sandbox>/.agents/skills/<name>/SKILL.md` (the
//! documented repo skill root) and runs non-interactive `codex exec`.
//! The temp sandbox is not a git checkout, so the command passes
//! `--skip-git-repo-check`. `approval_policy=never` with
//! `--sandbox workspace-write` allows file writes without an approval prompt.
//! Stdin is ignored. Skips when the CLI or credentials are missing.

use crate::adapters::parse::{
    add_unique, canonical_tool_name, clip_chars, detect_skill_trigger, extract_json_events,
    file_path_from_input, looks_like_missing_credentials, parse_maybe_json, skill_ref_from_input,
};
use crate::model::AdapterResult;
use crate::process::{env_nonempty, timeout_from_env, CommandOutput, CommandRunner, CommandSpec};
use crate::skill_fs::{collect_output_files, install_codex_skill};
use crate::which_path::find_in_env;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const HARNESS: &str = "codex";
const SKIP_NO_BIN: &str = "codex CLI not found on PATH. Install Codex (https://developers.openai.com/codex/cli) or run with --harness mock.";
const SKIP_IDENTITY: &str =
    "`codex` is on PATH but `codex exec --help` does not describe the Codex CLI. Skipping.";
const SKIP_NO_AUTH: &str = "codex CLI is installed but no credentials are available (`codex login status` is not logged in, and CODEX_API_KEY / CODEX_ACCESS_TOKEN are unset). Skipping so CI stays green without secrets.";
const SKIP_RUN_UNAUTH: &str =
    "codex reported that credentials are unavailable. Skipping so this is not recorded as a pass.";

const HELP_TIMEOUT: Duration = Duration::from_secs(8);
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_QUERY_MS: u64 = 180_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    Match,
    Mismatch,
    TimedOut,
}

#[derive(Debug, Clone)]
pub enum BinChoice {
    Search,
    Missing,
    Path(PathBuf),
}

pub struct CodexRequest<'a> {
    pub sandbox_dir: &'a Path,
    pub skill_dir: PathBuf,
    pub prompt: &'a str,
    pub skill_name: Option<&'a str>,
    pub env: &'a HashMap<String, String>,
    pub bin: BinChoice,
    pub identity: Option<Identity>,
    /// `Some` skips `codex login status`. An API key in `env` still wins.
    pub logged_in: Option<bool>,
}

impl<'a> CodexRequest<'a> {
    pub fn new(
        sandbox_dir: &'a Path,
        skill_dir: &Path,
        prompt: &'a str,
        env: &'a HashMap<String, String>,
    ) -> Self {
        Self {
            sandbox_dir,
            skill_dir: skill_dir.to_path_buf(),
            prompt,
            skill_name: None,
            env,
            bin: BinChoice::Search,
            identity: None,
            logged_in: None,
        }
    }

    pub fn with_skill_name(mut self, skill_name: &'a str) -> Self {
        self.skill_name = Some(skill_name);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub ok: bool,
    pub skip_reason: Option<String>,
}

struct FoundCli {
    bin: Option<PathBuf>,
    identity: Identity,
}

struct AuthProbe {
    ok: bool,
    timed_out: bool,
}

#[derive(Debug, Clone)]
pub struct ParsedOutput {
    pub called_tools: Vec<String>,
    pub skill_triggered: bool,
    pub stdout: String,
    pub stderr: String,
    pub artifacts: Vec<String>,
    pub thread_id: Option<String>,
    pub is_error: bool,
    pub error: Option<String>,
}

pub fn build_codex_args(prompt: &str, workspace: &Path) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        "--json".into(),
        "--ephemeral".into(),
        "--skip-git-repo-check".into(),
        "--sandbox".into(),
        "workspace-write".into(),
        "-c".into(),
        "approval_policy=never".into(),
        "--cd".into(),
        workspace.display().to_string(),
    ];
    if prompt.starts_with('-') {
        args.push("--".into());
    }
    args.push(prompt.to_string());
    args
}

pub fn assess_codex_availability(
    bin: Option<&Path>,
    identity: Identity,
    credentials_ok: bool,
    credentials_timed_out: bool,
) -> Availability {
    if bin.is_none() {
        return skip(SKIP_NO_BIN);
    }
    match identity {
        Identity::TimedOut => {
            return skip(&format!(
                "codex exec --help timed out after {}ms. Skipping.",
                HELP_TIMEOUT.as_millis()
            ));
        }
        Identity::Mismatch => return skip(SKIP_IDENTITY),
        Identity::Match => {}
    }
    if credentials_timed_out {
        return skip(&format!(
            "codex login status timed out after {}ms, and CODEX_API_KEY / CODEX_ACCESS_TOKEN are unset. Skipping so CI stays green without secrets.",
            AUTH_TIMEOUT.as_millis()
        ));
    }
    if !credentials_ok {
        return skip(SKIP_NO_AUTH);
    }
    Availability {
        ok: true,
        skip_reason: None,
    }
}

pub fn parse_codex_output(stdout: &str, stderr: &str, skill_name: Option<&str>) -> ParsedOutput {
    let events = extract_json_events(&format!("{stdout}\n{stderr}"));
    let mut called_tools = Vec::new();
    let mut skill_refs = Vec::new();
    let mut read_paths = Vec::new();
    let mut artifacts = Vec::new();
    let mut assistant_texts = Vec::new();
    let mut thread_id = None;
    let mut is_error = false;
    let mut error = None;

    for event in &events {
        if thread_id.is_none() {
            thread_id = string_field(event, "thread_id");
        }
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "turn.failed" => {
                is_error = true;
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex turn failed");
                error = Some(message.to_string());
                assistant_texts.push(message.to_string());
            }
            "error" => {
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex error");
                if !message.to_ascii_lowercase().contains("reconnecting") {
                    is_error = true;
                    error = Some(message.to_string());
                    assistant_texts.push(message.to_string());
                }
            }
            "item.started" | "item.updated" | "item.completed" => {
                if let Some(item) = event.get("item") {
                    note_item(
                        item,
                        &mut called_tools,
                        &mut skill_refs,
                        &mut read_paths,
                        &mut artifacts,
                        &mut assistant_texts,
                    );
                }
            }
            _ => {}
        }
    }

    let skill_triggered =
        detect_skill_trigger(skill_name, &skill_refs, &read_paths, &assistant_texts);
    let joined = assistant_texts.join("\n");
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
        thread_id,
        is_error,
        error,
    }
}

pub fn run_task(request: CodexRequest<'_>, runner: &dyn CommandRunner) -> AdapterResult {
    let found = resolve_bin(&request, runner);
    let (credentials_ok, credentials_timed_out) = resolve_credentials(&request, &found, runner);
    let gate = assess_codex_availability(
        found.bin.as_deref(),
        found.identity,
        credentials_ok,
        credentials_timed_out,
    );
    if !gate.ok {
        return AdapterResult::skipped(
            HARNESS,
            gate.skip_reason.unwrap_or_else(|| "adapter skipped".into()),
        );
    }
    let bin = found.bin.expect("availability requires a binary");
    let installed = match install_codex_skill(&request.skill_dir, request.sandbox_dir) {
        Ok(installed) => installed,
        Err(err) => return AdapterResult::errored(HARNESS, err),
    };
    let skill_name = request
        .skill_name
        .filter(|name| !name.is_empty())
        .unwrap_or(installed.skill_name.as_str());
    let timeout = timeout_from_env(request.env, DEFAULT_QUERY_MS);
    let args = build_codex_args(request.prompt, request.sandbox_dir);
    let output = runner.run(&CommandSpec::new(
        &bin,
        args.clone(),
        Some(request.sandbox_dir.to_path_buf()),
        request.env.clone(),
        timeout,
        16 * 1024 * 1024,
    ));
    finish_result(
        &output,
        &args,
        request.sandbox_dir,
        skill_name,
        &installed.dest,
        timeout,
    )
}

fn finish_result(
    output: &CommandOutput,
    args: &[String],
    sandbox: &Path,
    skill_name: &str,
    skill_dir: &Path,
    timeout: Duration,
) -> AdapterResult {
    let mut parsed = parse_codex_output(&output.stdout, &output.stderr, Some(skill_name));
    let files = collect_output_files(sandbox, &parsed.artifacts);
    if files.contains("summary.md") && !parsed.called_tools.iter().any(|tool| tool == "Write") {
        parsed.called_tools.push("Write".into());
    }
    if !output.timed_out
        && parsed.called_tools.is_empty()
        && !files.contains("summary.md")
        && credentials_missing(output, &parsed)
    {
        return AdapterResult::skipped(HARNESS, SKIP_RUN_UNAUTH);
    }
    let error = task_error(output, &parsed, timeout);
    let meta = serde_json::to_value(CodexMeta {
        argv: args,
        exit_code: output.status,
        thread_id: parsed.thread_id.clone(),
        skill_dir: skill_dir.display().to_string(),
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

fn note_item(
    item: &Value,
    called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
    assistant_texts: &mut Vec<String>,
) {
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "command_execution" => {
            add_unique(called_tools, "Bash");
            let command = item.get("command").and_then(Value::as_str).unwrap_or("");
            let paths = command_file_tokens(command);
            if !paths.is_empty() {
                add_unique(called_tools, "Read");
                for path in paths {
                    push_unique(read_paths, &path);
                }
            }
        }
        "file_change" => {
            if item.get("status").and_then(Value::as_str) == Some("failed") {
                return;
            }
            let Some(changes) = item.get("changes").and_then(Value::as_array) else {
                return;
            };
            for change in changes {
                let path = change.get("path").and_then(Value::as_str).unwrap_or("");
                if path.is_empty() {
                    continue;
                }
                let kind = change.get("kind").and_then(Value::as_str).unwrap_or("");
                if !change_is_write(kind) {
                    continue;
                }
                add_unique(called_tools, "Write");
                push_unique(artifacts, path);
            }
        }
        "agent_message" => {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    assistant_texts.push(text.to_string());
                }
            }
        }
        "mcp_tool_call" => {
            if let Some(name) = item.get("tool").and_then(Value::as_str) {
                let args = item
                    .get("arguments")
                    .map(parse_maybe_json)
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                note_named_tool(name, &args, called_tools, skill_refs, read_paths, artifacts);
            }
        }
        "skill" => {
            add_unique(called_tools, "Skill");
            if let Some(name) = item
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| item.get("skill").and_then(Value::as_str))
            {
                if !name.is_empty() {
                    skill_refs.push(name.to_string());
                }
            }
        }
        _ => {}
    }
}

fn note_named_tool(
    name: &str,
    input: &Value,
    called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
) {
    add_unique(called_tools, name);
    let canon = canonical_tool_name(name);
    if canon == "Skill" {
        let skill = skill_ref_from_input(input);
        if !skill.is_empty() {
            skill_refs.push(skill);
        }
    }
    let path = file_path_from_input(input);
    if path.is_empty() {
        return;
    }
    if matches!(canon.as_str(), "Read" | "Glob" | "Grep") {
        push_unique(read_paths, &path);
    }
    if matches!(canon.as_str(), "Write" | "Edit") {
        push_unique(artifacts, &path);
    }
}

fn command_file_tokens(command: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for raw in command.split_whitespace() {
        let token = raw.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | ',' | ';' | '\\'));
        if token.is_empty() || token.starts_with('-') {
            continue;
        }
        if token.contains('/') || (token.contains('.') && !token.starts_with('.')) {
            push_unique(&mut paths, token);
        }
    }
    paths
}

fn change_is_write(kind: &str) -> bool {
    !matches!(
        kind.to_ascii_lowercase().as_str(),
        "delete" | "remove" | "deleted"
    )
}

fn resolve_bin(request: &CodexRequest<'_>, runner: &dyn CommandRunner) -> FoundCli {
    match &request.bin {
        BinChoice::Missing => FoundCli {
            bin: None,
            identity: Identity::Mismatch,
        },
        BinChoice::Path(path) => FoundCli {
            bin: Some(path.clone()),
            identity: request.identity.unwrap_or(Identity::Match),
        },
        BinChoice::Search => {
            let Some(bin) = find_in_env("codex", request.env) else {
                return FoundCli {
                    bin: None,
                    identity: Identity::Mismatch,
                };
            };
            let identity = identify_codex(&bin, request.env, runner);
            FoundCli {
                bin: Some(bin),
                identity,
            }
        }
    }
}

fn resolve_credentials(
    request: &CodexRequest<'_>,
    found: &FoundCli,
    runner: &dyn CommandRunner,
) -> (bool, bool) {
    if found.identity != Identity::Match {
        return (false, false);
    }
    if has_api_credential(request.env) {
        return (true, false);
    }
    if let Some(flag) = request.logged_in {
        return (flag, false);
    }
    let Some(bin) = found.bin.as_ref() else {
        return (false, false);
    };
    let probe = probe_login(bin, request.env, runner);
    (probe.ok, probe.timed_out)
}

fn has_api_credential(env: &HashMap<String, String>) -> bool {
    env_nonempty(env, "CODEX_API_KEY") || env_nonempty(env, "CODEX_ACCESS_TOKEN")
}

fn identify_codex(
    bin: &Path,
    env: &HashMap<String, String>,
    runner: &dyn CommandRunner,
) -> Identity {
    let help = output_of(
        runner,
        bin,
        &["exec", "--help"],
        env,
        HELP_TIMEOUT,
        2 * 1024 * 1024,
    );
    if help.timed_out {
        return Identity::TimedOut;
    }
    if help_is_codex(&help.text()) {
        Identity::Match
    } else {
        Identity::Mismatch
    }
}

fn help_is_codex(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("--json") && lower.contains("sandbox")
}

fn probe_login(bin: &Path, env: &HashMap<String, String>, runner: &dyn CommandRunner) -> AuthProbe {
    let output = output_of(
        runner,
        bin,
        &["login", "status"],
        env,
        AUTH_TIMEOUT,
        1024 * 1024,
    );
    if output.timed_out {
        return AuthProbe {
            ok: false,
            timed_out: true,
        };
    }
    if looks_like_missing_credentials(&output.text()) {
        return AuthProbe {
            ok: false,
            timed_out: false,
        };
    }
    AuthProbe {
        ok: output.status == Some(0),
        timed_out: false,
    }
}

fn credentials_missing(output: &CommandOutput, parsed: &ParsedOutput) -> bool {
    let error = parsed.error.clone().unwrap_or_default();
    looks_like_missing_credentials(&parsed.stdout)
        || looks_like_missing_credentials(&parsed.stderr)
        || looks_like_missing_credentials(&error)
        || looks_like_missing_credentials(&output.text())
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

fn task_error(output: &CommandOutput, parsed: &ParsedOutput, timeout: Duration) -> Option<String> {
    if output.timed_out {
        return Some(format!("codex timed out after {}ms", timeout.as_millis()));
    }
    if let Some(error) = &output.error {
        return Some(error.clone());
    }
    if parsed.is_error {
        return Some(
            parsed
                .error
                .clone()
                .unwrap_or_else(|| "codex result is_error".into()),
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
                format!("codex exited {status}")
            } else {
                format!("codex exited {status}: {}", clip_chars(detail, 500))
            });
        }
    }
    None
}

fn skip(reason: &str) -> Availability {
    Availability {
        ok: false,
        skip_reason: Some(reason.to_string()),
    }
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if value.is_empty() || list.iter().any(|existing| existing == value) {
        return;
    }
    list.push(value.to_string());
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[derive(Serialize)]
struct CodexMeta<'a> {
    argv: &'a [String],
    #[serde(rename = "exitCode")]
    exit_code: Option<i32>,
    thread_id: Option<String>,
    skill_dir: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{StaticRunner, StdinMode, SystemRunner};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    fn ndjson(lines: &[&str]) -> String {
        lines.join("\n")
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

    fn skill() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr")
    }

    fn write_exec(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn summarize_stream() -> String {
        ndjson(&[
            r#"{"type":"thread.started","thread_id":"thr-1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.started","item":{"id":"read-skill","type":"command_execution","command":"cat .agents/skills/summarize-pr/SKILL.md","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"read-skill","type":"command_execution","command":"cat .agents/skills/summarize-pr/SKILL.md","status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"read-diff","type":"command_execution","command":"cat pr.diff","status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"write","type":"file_change","changes":[{"path":"summary.md","kind":"add"}],"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"failed-write","type":"file_change","changes":[{"path":"nope.md","kind":"add"}],"status":"failed"}}"#,
            r#"{"type":"item.completed","item":{"id":"delete","type":"file_change","changes":[{"path":"old.md","kind":"delete"}],"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"msg","type":"agent_message","text":"Wrote summary.md"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":2}}"#,
        ])
    }

    #[test]
    fn parse_normalizes_shell_reads_and_file_changes() {
        let parsed = parse_codex_output(&summarize_stream(), "", Some("summarize-pr"));
        assert_eq!(
            parsed.called_tools,
            vec!["Bash".to_string(), "Read".to_string(), "Write".to_string()]
        );
        assert!(parsed.skill_triggered);
        assert_eq!(parsed.artifacts, vec!["summary.md".to_string()]);
        assert_eq!(parsed.stdout, "Wrote summary.md");
        assert_eq!(parsed.thread_id.as_deref(), Some("thr-1"));
        assert!(!parsed.is_error);
    }

    #[test]
    fn parse_ignores_prose_and_unrelated_skill_paths() {
        let prose = r#"{"type":"item.completed","item":{"id":"m","type":"agent_message","text":"I will Write a poem about the weather."}}"#;
        let parsed = parse_codex_output(prose, "", Some("summarize-pr"));
        assert!(parsed.called_tools.is_empty());
        assert!(!parsed.skill_triggered);

        let other = r#"{"type":"item.completed","item":{"id":"r","type":"command_execution","command":"cat .agents/skills/other-skill/SKILL.md","status":"completed"}}"#;
        let parsed = parse_codex_output(other, "", Some("summarize-pr"));
        assert!(!parsed.skill_triggered);
        assert_eq!(
            parsed.called_tools,
            vec!["Bash".to_string(), "Read".to_string()]
        );
    }

    #[test]
    fn plain_listing_is_not_a_read() {
        let stdout = r#"{"type":"item.completed","item":{"id":"ls","type":"command_execution","command":"ls","status":"completed"}}"#;
        let parsed = parse_codex_output(stdout, "", Some("summarize-pr"));
        assert_eq!(parsed.called_tools, vec!["Bash".to_string()]);
    }

    #[test]
    fn reconnect_notice_is_not_a_failure() {
        let stdout = "\
{\"type\":\"error\",\"message\":\"Reconnecting... 1/5\"}\n\
{\"type\":\"item.completed\",\"item\":{\"id\":\"m\",\"type\":\"agent_message\",\"text\":\"pr.diff is missing\"}}\n\
{\"type\":\"turn.completed\"}";
        let parsed = parse_codex_output(stdout, "", Some("summarize-pr"));
        assert!(!parsed.is_error);
        assert!(parsed.stdout.contains("missing"));
    }

    #[test]
    fn build_args_is_noninteractive_workspace_write() {
        let args = build_codex_args("Summarize the PR", Path::new("/tmp/sandbox"));
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--ephemeral".to_string(),
                "--skip-git-repo-check".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "-c".to_string(),
                "approval_policy=never".to_string(),
                "--cd".to_string(),
                "/tmp/sandbox".to_string(),
                "Summarize the PR".to_string(),
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--url" || arg == "--yolo"));
        assert!(!args
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
        let flagged = build_codex_args("--help", Path::new("/tmp/sandbox"));
        let dash = flagged.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(flagged[dash + 1], "--help");
    }

    #[test]
    fn availability_skips_without_cli_identity_or_credentials() {
        assert!(
            assess_codex_availability(None, Identity::Mismatch, false, false)
                .skip_reason
                .unwrap()
                .contains("not found")
        );
        assert!(assess_codex_availability(
            Some(Path::new("/usr/bin/codex")),
            Identity::Mismatch,
            true,
            false
        )
        .skip_reason
        .unwrap()
        .contains("does not describe"));
        assert!(assess_codex_availability(
            Some(Path::new("/usr/bin/codex")),
            Identity::TimedOut,
            true,
            false
        )
        .skip_reason
        .unwrap()
        .contains("timed out"));
        assert!(assess_codex_availability(
            Some(Path::new("/usr/bin/codex")),
            Identity::Match,
            false,
            false
        )
        .skip_reason
        .unwrap()
        .contains("no credentials"));
        assert!(
            assess_codex_availability(
                Some(Path::new("/usr/bin/codex")),
                Identity::Match,
                true,
                false
            )
            .ok
        );
    }

    #[test]
    fn run_skips_before_install_when_codex_is_absent() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(ok_output(""));
        let env = HashMap::new();
        let mut request = CodexRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Missing;
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("not found"));
        assert!(!sandbox.path().join(".agents").exists());
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn run_skips_when_login_status_is_not_logged_in() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(CommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "Not logged in".into(),
            timed_out: false,
            error: None,
        });
        let env = HashMap::new();
        let mut request = CodexRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/codex"));
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("no credentials"));
        assert!(!sandbox.path().join(".agents").exists());
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].args,
            vec!["login".to_string(), "status".to_string()]
        );
        assert_eq!(calls[0].timeout, AUTH_TIMEOUT);
    }

    #[test]
    fn api_key_skips_the_login_probe_and_parses_the_fixture() {
        let sandbox = tempfile::tempdir().unwrap();
        let sandbox_path = sandbox.path().to_path_buf();
        let mut runner = StaticRunner::new(ok_output(&summarize_stream()));
        runner.on_run = Some(Box::new(move || {
            fs::write(
                sandbox_path.join("summary.md"),
                "# Update src/hello.js\n\n- a\n- b\n- c\n",
            )
            .unwrap();
        }));
        let mut env = HashMap::new();
        env.insert("CODEX_API_KEY".into(), "test-key".into());
        env.insert("SKILLPARITY_TIMEOUT_MS".into(), "2500".into());
        let mut request = CodexRequest::new(
            sandbox.path(),
            &skill(),
            "Please summarize this PR using the local pr.diff file.",
            &env,
        )
        .with_skill_name("summarize-pr");
        request.bin = BinChoice::Path(PathBuf::from("/opt/codex"));
        let result = run_task(request, &runner);
        assert!(!result.skipped);
        assert!(result.error.is_none());
        assert!(result.skill_triggered);
        assert_eq!(
            result.called_tools,
            vec!["Bash".to_string(), "Read".to_string(), "Write".to_string()]
        );
        assert!(result
            .files
            .get("summary.md")
            .unwrap()
            .contains("# Update src/hello.js"));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, PathBuf::from("/opt/codex"));
        assert_eq!(calls[0].stdin, StdinMode::Ignore);
        assert_eq!(calls[0].cwd.as_deref(), Some(sandbox.path()));
        assert_eq!(calls[0].timeout, Duration::from_millis(2500));
        assert!(calls[0].args.iter().any(|arg| arg == "--json"));
        assert!(calls[0]
            .args
            .iter()
            .any(|arg| arg == "approval_policy=never"));
        assert!(calls[0]
            .args
            .iter()
            .any(|arg| arg == "--skip-git-repo-check"));
        assert!(!calls[0].args.iter().any(|arg| arg == "login"));
        assert!(sandbox
            .path()
            .join(".agents/skills/summarize-pr/SKILL.md")
            .is_file());
        assert!(!sandbox
            .path()
            .join(".agents/skills/summarize-pr/fixtures")
            .exists());
        assert!(!sandbox
            .path()
            .join(".agents/skills/summarize-pr/parity.yaml")
            .exists());
    }

    #[test]
    fn help_timeout_skips_without_a_query() {
        let dir = tempfile::tempdir().unwrap();
        write_exec(&dir.path().join("codex"), "#!/bin/sh\nexit 0\n");
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(CommandOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            error: Some("ETIMEDOUT".into()),
        });
        let mut env = HashMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        let request = CodexRequest::new(sandbox.path(), &skill(), "hi", &env);
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("timed out"));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].args,
            vec!["exec".to_string(), "--help".to_string()]
        );
        assert_eq!(calls[0].timeout, HELP_TIMEOUT);
        assert!(!sandbox.path().join(".agents").exists());
    }

    #[test]
    fn help_must_describe_codex_exec() {
        let dir = tempfile::tempdir().unwrap();
        write_exec(
            &dir.path().join("codex"),
            "#!/bin/sh\necho 'codex the book encoder'\n",
        );
        let sandbox = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        env.insert("CODEX_API_KEY".into(), "test-key".into());
        let request = CodexRequest::new(sandbox.path(), &skill(), "hi", &env);
        let result = run_task(request, &SystemRunner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("does not describe"));
        assert!(!sandbox.path().join(".agents").exists());
    }

    #[test]
    fn invalid_api_key_is_a_failure_not_a_skip() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(CommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "Invalid API key".into(),
            timed_out: false,
            error: None,
        });
        let env = HashMap::new();
        let mut request = CodexRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/codex"));
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(!result.skipped);
        let error = result.error.unwrap();
        assert!(error.contains("exited 1"), "{error}");
        assert!(error.contains("Invalid API key"), "{error}");
    }

    #[test]
    fn clear_auth_error_from_the_query_skips() {
        let sandbox = tempfile::tempdir().unwrap();
        let stdout = r#"{"type":"turn.failed","error":{"message":"Not logged in. Please login."}}"#;
        let runner = StaticRunner::new(ok_output(stdout));
        let env = HashMap::new();
        let mut request = CodexRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/codex"));
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result
            .skip_reason
            .unwrap()
            .contains("not recorded as a pass"));
    }

    #[test]
    fn query_process_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex");
        write_exec(&script, "#!/bin/sh\nexec /bin/sleep 30\n");
        let sandbox = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("SKILLPARITY_TIMEOUT_MS".into(), "400".into());
        let mut request = CodexRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(script);
        request.logged_in = Some(true);
        let started = Instant::now();
        let result = run_task(request, &SystemRunner);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "elapsed {:?}",
            started.elapsed()
        );
        assert!(!result.skipped);
        let error = result.error.unwrap();
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("400"), "{error}");
    }
}
