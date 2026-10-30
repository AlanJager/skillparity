use crate::model::FileMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

const SKIP_NAMES: &[&str] = &[
    "fixtures",
    "parity.yaml",
    "parity-report.json",
    "node_modules",
    ".git",
];

pub fn read_skill_name(skill_dir: &Path) -> Result<String, String> {
    let md_path = skill_dir.join("SKILL.md");
    let md = fs::read_to_string(&md_path)
        .map_err(|err| format!("failed to read {}: {err}", md_path.display()))?;
    Ok(skill_name_from_markdown(&md).unwrap_or_else(|| {
        skill_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "skill".into())
    }))
}

pub fn skill_name_from_markdown(md: &str) -> Option<String> {
    let re = regex::RegexBuilder::new(r#"^name:\s*["']?([^\s"']+)"#)
        .multi_line(true)
        .build()
        .expect("skill name regex");
    re.captures(md).map(|cap| cap[1].to_string())
}

pub fn copy_skill_tree(skill_dir: &Path, dest_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dest_dir)?;
    copy_filtered(skill_dir, dest_dir)
}

fn copy_filtered(src: &Path, dest: &Path) -> io::Result<()> {
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let name = ent.file_name();
        let name_str = name.to_string_lossy();
        if SKIP_NAMES.iter().any(|skip| *skip == name_str.as_ref()) {
            continue;
        }
        let ft = ent.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let to = dest.join(&name);
        if ft.is_dir() {
            fs::create_dir_all(&to)?;
            copy_filtered(&ent.path(), &to)?;
        } else if ft.is_file() {
            fs::copy(ent.path(), &to)?;
        }
    }
    Ok(())
}

pub fn install_claude_skill(skill_dir: &Path, sandbox_dir: &Path) -> Result<ClaudeInstall, String> {
    let skill_name = read_skill_name(skill_dir)?;
    let dest = sandbox_dir.join(".claude").join("skills").join(&skill_name);
    copy_skill_tree(skill_dir, &dest)
        .map_err(|err| format!("failed to install Claude skill: {err}"))?;
    let settings_path = sandbox_dir.join(".claude").join("settings.json");
    let settings = serde_json::json!({
        "permissions": {
            "defaultMode": "acceptEdits",
            "allow": ["Read", "Write", "Edit", "Glob", "Grep", "Skill"],
            "deny": []
        }
    });
    write_pretty(&settings_path, &settings)?;
    Ok(ClaudeInstall {
        skill_name,
        dest,
        settings_path,
    })
}

pub fn install_cursor_skill(skill_dir: &Path, sandbox_dir: &Path) -> Result<CursorInstall, String> {
    let skill_name = read_skill_name(skill_dir)?;
    let dirs = vec![
        sandbox_dir.join(".cursor").join("skills").join(&skill_name),
        sandbox_dir.join(".agents").join("skills").join(&skill_name),
    ];
    for dest in &dirs {
        copy_skill_tree(skill_dir, dest)
            .map_err(|err| format!("failed to install Cursor skill: {err}"))?;
    }
    let cli_json_path = sandbox_dir.join(".cursor").join("cli.json");
    let cli_json = serde_json::json!({
        "permissions": {
            "allow": ["Read(**)", "Write(**)"],
            "deny": ["Shell(rm)", "Read(.env*)", "Write(**/*.key)", "Write(**/.env*)"]
        }
    });
    write_pretty(&cli_json_path, &cli_json)?;
    Ok(CursorInstall {
        skill_name,
        dirs,
        cli_json_path,
    })
}

fn write_pretty(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|err| format!("failed to encode {}: {err}", path.display()))?;
    fs::write(path, format!("{text}\n"))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

#[derive(Debug)]
pub struct ClaudeInstall {
    pub skill_name: String,
    pub dest: PathBuf,
    pub settings_path: PathBuf,
}

#[derive(Debug)]
pub struct CursorInstall {
    pub skill_name: String,
    pub dirs: Vec<PathBuf>,
    pub cli_json_path: PathBuf,
}

pub fn absolute_lexical(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    normalize_lexical(&abs)
}

pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn sandbox_relative(root: &Path, file_path: &str) -> Option<String> {
    if file_path.is_empty() {
        return None;
    }
    let root = absolute_lexical(root);
    let joined = if Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        root.join(file_path)
    };
    let abs = normalize_lexical(&joined);
    let rel = abs.strip_prefix(&root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str == ".." || rel_str.starts_with("../") {
        return None;
    }
    Some(rel_str)
}

pub fn read_lossy(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Record artifact paths that landed inside the sandbox, plus `summary.md` when present.
pub fn collect_output_files(sandbox: &Path, artifacts: &[String]) -> FileMap {
    let mut files = FileMap::default();
    for path in artifacts {
        let Some(rel) = sandbox_relative(sandbox, path) else {
            continue;
        };
        if files.contains(&rel) {
            continue;
        }
        let abs = sandbox.join(&rel);
        if abs.is_file() {
            if let Some(text) = read_lossy(&abs) {
                files.insert(rel, text);
            }
        }
    }
    let summary = sandbox.join("summary.md");
    if summary.is_file() && !files.contains("summary.md") {
        if let Some(text) = read_lossy(&summary) {
            files.insert("summary.md", text);
        }
    }
    files
}

pub fn copy_fixture_tree(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let ft = ent.file_type()?;
        let to = dest.join(ent.file_name());
        if ft.is_symlink() {
            let from = ent.path();
            if from.is_file() {
                fs::copy(from, to)?;
            }
            continue;
        }
        if ft.is_dir() {
            copy_fixture_tree(&ent.path(), &to)?;
        } else if ft.is_file() {
            fs::copy(ent.path(), to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn skill_name_from_frontmatter() {
        let md = "---\nname: \"summarize-pr\"\n---\n\nname: other\n";
        assert_eq!(
            skill_name_from_markdown(md).as_deref(),
            Some("summarize-pr")
        );
        assert_eq!(
            skill_name_from_markdown("# no frontmatter").as_deref(),
            None
        );
    }

    #[test]
    fn install_claude_skill_skips_fixtures_parity_and_symlinks() {
        let skill = tempfile::tempdir().unwrap();
        let sandbox = tempfile::tempdir().unwrap();
        fs::create_dir(skill.path().join("references")).unwrap();
        fs::write(
            skill.path().join("SKILL.md"),
            "---\nname: nested-skill\n---\n\n# nested\n",
        )
        .unwrap();
        fs::write(skill.path().join("references").join("guide.md"), "guide\n").unwrap();
        fs::create_dir(skill.path().join("fixtures")).unwrap();
        fs::write(skill.path().join("fixtures").join("secret.txt"), "nope\n").unwrap();
        fs::write(skill.path().join("parity.yaml"), "tasks: []\n").unwrap();
        symlink(
            skill.path().join("references").join("guide.md"),
            skill.path().join("leak.md"),
        )
        .unwrap();

        let installed = install_claude_skill(skill.path(), sandbox.path()).unwrap();
        assert_eq!(installed.skill_name, "nested-skill");
        let root = sandbox
            .path()
            .join(".claude")
            .join("skills")
            .join("nested-skill");
        assert_eq!(
            fs::read_to_string(root.join("references").join("guide.md")).unwrap(),
            "guide\n"
        );
        assert!(!root.join("fixtures").exists());
        assert!(!root.join("parity.yaml").exists());
        assert!(!root.join("leak.md").exists());
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "acceptEdits");
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert!(allow.iter().any(|v| v == "Read"));
        assert!(allow.iter().any(|v| v == "Write"));
        assert!(allow.iter().any(|v| v == "Skill"));
    }

    #[test]
    fn sandbox_relative_stays_inside() {
        let root = Path::new("/tmp/skillparity-abc");
        assert_eq!(
            sandbox_relative(root, "summary.md").as_deref(),
            Some("summary.md")
        );
        assert_eq!(
            sandbox_relative(root, "/tmp/skillparity-abc/summary.md").as_deref(),
            Some("summary.md")
        );
        assert_eq!(sandbox_relative(root, "/etc/passwd"), None);
        assert_eq!(sandbox_relative(root, "../etc/passwd"), None);
    }
}
