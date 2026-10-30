use crate::run::{execute, RunOptions};
use clap::Parser;
use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "\
skillparity — multi-harness skill behavioral parity

Usage:
  skillparity run --skill <dir> --parity <yaml> [--harness mock|claude|cursor] [--ci] [--skip-ok]
  skillparity help

Options:
  --skill     Directory containing SKILL.md
  --parity    parity.yaml task file
  --harness   mock | claude | cursor (default: mock)
  --ci        exit 1 on any fail (skip does not fail unless all skipped)
  --skip-ok   with --ci, exit 0 when every task was skipped
  --out       directory for parity-report.json (default: cwd)

Example:
  skillparity run \\
    --skill examples/summarize-pr \\
    --parity examples/summarize-pr/parity.yaml \\
    --harness mock --ci
";

pub enum Parsed {
    Help,
    UnknownCommand(String),
    Error(String),
    Run(RunOptions),
}

#[derive(Parser, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
struct RunArgs {
    #[arg(long)]
    skill: Option<String>,
    #[arg(long)]
    parity: Option<String>,
    #[arg(long, default_value = "mock")]
    harness: String,
    #[arg(long)]
    ci: bool,
    #[arg(long = "skip-ok")]
    skip_ok: bool,
    #[arg(long)]
    out: Option<String>,
}

pub fn parse_args<I, T>(args: I) -> Parsed
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args.is_empty() {
        return Parsed::Help;
    }
    let first = match args[0].to_str() {
        Some(text) => text.to_string(),
        None => return Parsed::Error("argument is not valid UTF-8".into()),
    };
    if first == "help" || first == "--help" || first == "-h" || args_have_help(&args) {
        return Parsed::Help;
    }
    if first != "run" {
        return Parsed::UnknownCommand(first);
    }
    if let Some(unknown) = unknown_flag(&args[1..]) {
        return Parsed::Error(format!("Unknown argument: {unknown}"));
    }
    let parsed = match RunArgs::try_parse_from(
        std::iter::once(OsString::from("skillparity")).chain(args.into_iter().skip(1)),
    ) {
        Ok(parsed) => parsed,
        Err(err) => return Parsed::Error(err.to_string()),
    };
    let (Some(skill), Some(parity)) = (parsed.skill, parsed.parity) else {
        return Parsed::Error("--skill and --parity are required".into());
    };
    let out = parsed
        .out
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    Parsed::Run(RunOptions {
        skill: PathBuf::from(skill),
        parity: PathBuf::from(parity),
        harness: parsed.harness,
        ci: parsed.ci,
        skip_ok: parsed.skip_ok,
        out,
    })
}

fn args_have_help(args: &[OsString]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.to_str(), Some("--help") | Some("-h")))
}

fn unknown_flag(args: &[OsString]) -> Option<String> {
    const KNOWN: &[&str] = &[
        "--skill",
        "--parity",
        "--harness",
        "--out",
        "--ci",
        "--skip-ok",
    ];
    const VALUES: &[&str] = &["--skill", "--parity", "--harness", "--out"];
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].to_str()?.to_string();
        if arg.starts_with('-') && !KNOWN.contains(&arg.as_str()) {
            return Some(arg);
        }
        if VALUES.contains(&arg.as_str()) {
            index += 1;
            if index >= args.len() {
                return None;
            }
        }
        index += 1;
    }
    None
}

pub fn real_main() {
    let parsed = parse_args(std::env::args().skip(1));
    let code = dispatch(parsed);
    if code != 0 {
        std::process::exit(code);
    }
}

fn dispatch(parsed: Parsed) -> i32 {
    match parsed {
        Parsed::Help => {
            println!("{USAGE}");
            0
        }
        Parsed::UnknownCommand(command) => {
            eprintln!("Unknown command: {command}\n");
            println!("{USAGE}");
            2
        }
        Parsed::Error(message) => {
            eprintln!("Error: {message}");
            2
        }
        Parsed::Run(opts) => match execute(opts) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("Error: {message}");
                2
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_and_unknown_command() {
        assert!(matches!(parse_args(Vec::<String>::new()), Parsed::Help));
        assert!(matches!(parse_args(["help"]), Parsed::Help));
        assert!(matches!(parse_args(["run", "--help"]), Parsed::Help));
        match parse_args(["nope"]) {
            Parsed::UnknownCommand(command) => assert_eq!(command, "nope"),
            other => panic!("unexpected {other:?}"),
        }
        match parse_args(["run", "--bogus"]) {
            Parsed::Error(message) => {
                assert!(message.contains("Unknown argument: --bogus"), "{message}")
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse_args(["run"]) {
            Parsed::Error(message) => {
                assert!(message.contains("--skill and --parity are required"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    impl std::fmt::Debug for Parsed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Parsed::Help => write!(f, "Help"),
                Parsed::UnknownCommand(command) => write!(f, "UnknownCommand({command})"),
                Parsed::Error(message) => write!(f, "Error({message})"),
                Parsed::Run(_) => write!(f, "Run"),
            }
        }
    }
}
