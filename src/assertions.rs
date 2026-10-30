use crate::model::AdapterResult;
use crate::yaml::Expect;
use regex::RegexBuilder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    pub pass: bool,
    pub skipped: bool,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
}

pub fn evaluate_expect(expect: &Expect, result: &AdapterResult) -> Result<Assertion, String> {
    let notes = Vec::new();
    if result.skipped {
        return Ok(Assertion {
            pass: false,
            skipped: true,
            failures: vec![format!(
                "skipped: {}",
                result.skip_reason.as_deref().unwrap_or("adapter skipped")
            )],
            notes,
        });
    }

    let mut failures = Vec::new();
    if let Some(error) = &result.error {
        if expect.skill_triggered != Some(false) {
            failures.push(format!("adapter error: {error}"));
        }
    }

    if let Some(expected) = expect.skill_triggered {
        let actual = result.skill_triggered;
        if actual != expected {
            failures.push(format!(
                "skill_triggered: expected {expected}, got {actual}"
            ));
        }
    }

    let called: Vec<String> = result
        .called_tools
        .iter()
        .map(|tool| tool.to_lowercase())
        .collect();

    if let Some(tools) = &expect.called_tools {
        for tool in tools {
            if !called.iter().any(|got| got == &tool.to_lowercase()) {
                failures.push(format!("called_tools: missing \"{tool}\""));
            }
        }
    }

    if let Some(tools) = &expect.not_called_tools {
        for tool in tools {
            if called.iter().any(|got| got == &tool.to_lowercase()) {
                failures.push(format!("not_called_tools: unexpectedly called \"{tool}\""));
            }
        }
    }

    if let Some(files) = &expect.files_exist {
        for rel in files {
            let present = result.files.contains(rel)
                || result.files.keys().iter().any(|key| key.ends_with(rel));
            if !present {
                failures.push(format!("files_exist: missing \"{rel}\""));
            }
        }
        if files.is_empty() && result.files.contains("summary.md") {
            failures.push("files_exist: unexpected \"summary.md\" present".into());
        }
    }

    if let Some(pattern) = &expect.stdout_matches {
        let matched = stdout_matches(pattern, &result.stdout)?;
        if !matched {
            failures.push(format!("stdout_matches: /{pattern}/ did not match stdout"));
        }
    }

    Ok(Assertion {
        pass: failures.is_empty(),
        skipped: false,
        failures,
        notes,
    })
}

fn stdout_matches(pattern: &str, stdout: &str) -> Result<bool, String> {
    let mut source = pattern.to_string();
    let mut case_insensitive = false;
    if let Some(rest) = source.strip_prefix("(?i)") {
        case_insensitive = true;
        source = rest.to_string();
    } else if let Some(rest) = source.strip_prefix("(?im)") {
        case_insensitive = true;
        source = rest.to_string();
    } else if let Some(rest) = source.strip_prefix("(?mi)") {
        case_insensitive = true;
        source = rest.to_string();
    }
    let re = RegexBuilder::new(&source)
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .build()
        .map_err(|err| format!("invalid stdout_matches /{pattern}/: {err}"))?;
    Ok(re.is_match(stdout))
}

#[cfg(test)]
mod tests {
    use super::evaluate_expect;
    use crate::model::AdapterResult;
    use crate::yaml::Expect;

    fn result() -> AdapterResult {
        AdapterResult::new("mock")
    }

    #[test]
    fn skill_trigger_and_tools() {
        let mut got = result();
        got.skill_triggered = true;
        got.called_tools = vec!["read".into(), "Write".into()];
        got.files.insert("summary.md", "# t\n");
        let expect = Expect {
            skill_triggered: Some(true),
            called_tools: Some(vec!["Read".into(), "Write".into()]),
            files_exist: Some(vec!["summary.md".into()]),
            ..Expect::default()
        };
        let assertion = evaluate_expect(&expect, &got).unwrap();
        assert!(assertion.pass);
    }

    #[test]
    fn missing_tool_and_unexpected_summary() {
        let mut got = result();
        got.skill_triggered = false;
        got.called_tools = vec!["Write".into()];
        got.files.insert("summary.md", "x");
        let expect = Expect {
            skill_triggered: Some(false),
            not_called_tools: Some(vec!["Write".into()]),
            files_exist: Some(vec![]),
            ..Expect::default()
        };
        let assertion = evaluate_expect(&expect, &got).unwrap();
        assert!(!assertion.pass);
        assert!(assertion
            .failures
            .iter()
            .any(|f| f.contains("unexpectedly called")));
        assert!(assertion.failures.iter().any(|f| f.contains("unexpected")));
    }

    #[test]
    fn stdout_inline_case_flag() {
        let mut got = result();
        got.stdout = "Error: pr.diff is Missing\n".into();
        let expect = Expect {
            stdout_matches: Some("(?i)(missing|empty|not found|no such)".into()),
            ..Expect::default()
        };
        assert!(evaluate_expect(&expect, &got).unwrap().pass);

        got.stdout = "all good".into();
        let failed = evaluate_expect(&expect, &got).unwrap();
        assert!(failed.failures[0].contains("stdout_matches"));
    }

    #[test]
    fn skip_short_circuits_and_error_respects_no_trigger() {
        let skipped = AdapterResult::skipped("claude", "claude CLI not found on PATH");
        let assertion = evaluate_expect(&Expect::default(), &skipped).unwrap();
        assert!(assertion.skipped);
        assert!(!assertion.pass);
        assert!(assertion.failures[0].starts_with("skipped:"));

        let mut errored = AdapterResult::errored("claude", "boom");
        errored.skill_triggered = false;
        let expect = Expect {
            skill_triggered: Some(false),
            ..Expect::default()
        };
        assert!(evaluate_expect(&expect, &errored).unwrap().pass);

        let expect_trigger = Expect {
            skill_triggered: Some(true),
            ..Expect::default()
        };
        let failed = evaluate_expect(&expect_trigger, &errored).unwrap();
        assert!(failed
            .failures
            .iter()
            .any(|f| f.starts_with("adapter error:")));
    }
}
