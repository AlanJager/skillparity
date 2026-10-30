pub mod claude;
pub mod cursor;
pub mod mock;
pub mod parse;

use crate::model::AdapterResult;
use crate::process::CommandRunner;
use crate::yaml::Task;
use std::collections::HashMap;
use std::path::Path;

pub fn run_harness(
    harness: &str,
    sandbox_dir: &Path,
    skill_dir: &Path,
    task: &Task,
    skill_name: &str,
    env: &HashMap<String, String>,
    runner: &dyn CommandRunner,
) -> Result<AdapterResult, String> {
    match harness {
        "mock" => Ok(mock::run_task(sandbox_dir, skill_dir, task, skill_name)),
        "claude" => {
            let prompt = task.prompt_text();
            Ok(claude::run_task(
                claude::ClaudeRequest::new(sandbox_dir, skill_dir, &prompt, env),
                runner,
            ))
        }
        "cursor" => {
            let prompt = task.prompt_text();
            Ok(cursor::run_task(
                cursor::CursorRequest::new(sandbox_dir, skill_dir, &prompt, env)
                    .with_skill_name(skill_name),
                runner,
            ))
        }
        other => Err(format!(
            "Unknown harness \"{other}\". Available: mock, claude, cursor"
        )),
    }
}
