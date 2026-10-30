use std::collections::HashMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinMode {
    /// Print mode must not block on an open stdin pipe.
    Ignore,
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub max_buffer: usize,
    pub stdin: StdinMode,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<PathBuf>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: HashMap<String, String>,
        timeout: Duration,
        max_buffer: usize,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            cwd,
            env,
            timeout,
            max_buffer,
            stdin: StdinMode::Ignore,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub error: Option<String>,
}

impl CommandOutput {
    pub fn text(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

pub trait CommandRunner {
    fn run(&self, spec: &CommandSpec) -> CommandOutput;
}

pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec) -> CommandOutput {
        run_system(spec)
    }
}

struct ReadResult {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn read_capped(mut reader: impl Read, max: usize) -> ReadResult {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let mut exceeded = false;
    loop {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < max {
                    let room = max - buf.len();
                    if n > room {
                        buf.extend_from_slice(&tmp[..room]);
                        exceeded = true;
                    } else {
                        buf.extend_from_slice(&tmp[..n]);
                    }
                } else {
                    exceeded = true;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    ReadResult {
        bytes: buf,
        exceeded,
    }
}

fn finish(
    mut child: Child,
    stdout_thread: JoinHandle<ReadResult>,
    stderr_thread: JoinHandle<ReadResult>,
    timed_out: bool,
) -> CommandOutput {
    if timed_out {
        let _ = child.kill();
    }
    let status = child.wait().ok().and_then(|status| status.code());
    let out = stdout_thread.join().unwrap_or(ReadResult {
        bytes: Vec::new(),
        exceeded: false,
    });
    let err = stderr_thread.join().unwrap_or(ReadResult {
        bytes: Vec::new(),
        exceeded: false,
    });
    let exceeded = out.exceeded || err.exceeded;
    CommandOutput {
        status,
        stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&err.bytes).into_owned(),
        timed_out,
        error: if timed_out {
            Some("ETIMEDOUT".into())
        } else if exceeded {
            Some("maxBuffer exceeded".into())
        } else {
            None
        },
    }
}

fn run_system(spec: &CommandSpec) -> CommandOutput {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return CommandOutput {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                error: Some(err.to_string()),
            };
        }
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let max = spec.max_buffer;
    let stdout_thread = thread::spawn(move || read_capped(stdout, max));
    let stderr_thread = thread::spawn(move || read_capped(stderr, max));
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return finish(child, stdout_thread, stderr_thread, false);
            }
            Ok(None) if start.elapsed() > spec.timeout => {
                return finish(child, stdout_thread, stderr_thread, true);
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return CommandOutput {
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: false,
                    error: Some(err.to_string()),
                };
            }
        }
    }
}

pub fn timeout_from_env(env: &HashMap<String, String>, fallback_ms: u64) -> Duration {
    let Some(raw) = env.get("SKILLPARITY_TIMEOUT_MS") else {
        return Duration::from_millis(fallback_ms);
    };
    if raw.is_empty() {
        return Duration::from_millis(fallback_ms);
    }
    match raw.parse::<f64>() {
        Ok(ms) if ms.is_finite() && ms >= 0.0 => Duration::from_millis(ms as u64),
        _ => Duration::from_millis(fallback_ms),
    }
}

pub fn env_nonempty(env: &HashMap<String, String>, key: &str) -> bool {
    env.get(key).map(|value| !value.is_empty()).unwrap_or(false)
}

pub fn snapshot_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
#[derive(Debug)]
pub struct RecordedCall {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub stdin: StdinMode,
    pub timeout: Duration,
}

#[cfg(test)]
pub struct StaticRunner {
    pub output: CommandOutput,
    pub calls: std::sync::Mutex<Vec<RecordedCall>>,
    pub on_run: Option<Box<dyn Fn() + Send + Sync>>,
}

#[cfg(test)]
impl StaticRunner {
    pub fn new(output: CommandOutput) -> Self {
        Self {
            output,
            calls: std::sync::Mutex::new(Vec::new()),
            on_run: None,
        }
    }
}

#[cfg(test)]
impl CommandRunner for StaticRunner {
    fn run(&self, spec: &CommandSpec) -> CommandOutput {
        self.calls.lock().expect("call lock").push(RecordedCall {
            program: spec.program.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            stdin: spec.stdin,
            timeout: spec.timeout,
        });
        if let Some(hook) = &self.on_run {
            hook();
        }
        self.output.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn system_runner_captures_stdout_and_ignores_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("echo-tool");
        fs_write_exec(
            &bin,
            "#!/bin/sh\nif [ -t 0 ]; then echo tty; else echo notty; fi\necho args:\"$1\"\n",
        );
        let spec = CommandSpec::new(
            &bin,
            vec!["hello".into()],
            None,
            HashMap::new(),
            Duration::from_secs(5),
            1024 * 1024,
        );
        let output = SystemRunner.run(&spec);
        assert_eq!(output.status, Some(0), "{}", output.stderr);
        assert!(output.stdout.contains("notty"), "{}", output.stdout);
        assert!(output.stdout.contains("args:hello"), "{}", output.stdout);
        assert!(!output.timed_out);
    }

    fn fs_write_exec(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
