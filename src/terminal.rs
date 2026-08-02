use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

pub fn render_argv(template: &[String], cwd: &str, thread_id: &str) -> Result<Vec<String>> {
    if template.is_empty() || template[0].trim().is_empty() {
        bail!("terminal argv template is empty");
    }
    let mut saw_cwd = false;
    let mut saw_thread = false;
    let rendered = template
        .iter()
        .map(|argument| {
            saw_cwd |= argument.contains("{cwd}");
            saw_thread |= argument.contains("{thread_id}");
            argument
                .replace("{cwd}", cwd)
                .replace("{thread_id}", thread_id)
        })
        .collect::<Vec<_>>();
    if !saw_cwd {
        bail!("terminal argv template omitted {{cwd}}");
    }
    if !saw_thread {
        bail!("terminal argv template omitted {{thread_id}}");
    }
    Ok(rendered)
}

pub fn launch(template: &[String], cwd: &str, thread_id: &str) -> Result<u32> {
    let argv = render_argv(template, cwd, thread_id)?;
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch terminal executable {:?}", argv[0]))?;
    Ok(child.id())
}

pub fn find_executable(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if program.contains('/') {
        return path.is_file().then(|| path.to_path_buf());
    }
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_terminal_contract_renders_expected_resume_argv() {
        let template = vec![
            "x-terminal-emulator".into(),
            "-e".into(),
            "codex".into(),
            "-C".into(),
            "{cwd}".into(),
            "resume".into(),
            "{thread_id}".into(),
        ];
        assert_eq!(
            render_argv(&template, "/work/repo", "thr_123").unwrap(),
            vec![
                "x-terminal-emulator",
                "-e",
                "codex",
                "-C",
                "/work/repo",
                "resume",
                "thr_123"
            ]
        );
    }

    #[test]
    fn values_remain_single_arguments_without_shell_interpolation() {
        let template = vec!["terminal".into(), "{cwd}".into(), "{thread_id}".into()];
        let argv = render_argv(&template, "/repo; touch owned", "id$(whoami)").unwrap();
        assert_eq!(argv[1], "/repo; touch owned");
        assert_eq!(argv[2], "id$(whoami)");
        assert_eq!(argv.len(), 3);
    }
}
