//! Recognizes AI tools and other programs from a command line, and maps folders to projects.

use crate::config::Tool;
use crate::util;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const INTERPRETERS: [&str; 12] = [
    "node", "bun", "deno", "python", "python3", "uv", "uvx", "npx", "pnpm", "ruby", "bunx", "tsx",
];

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Path segments and basenames that describe what a command line runs.
fn tokens(cmd: &[String]) -> Vec<String> {
    let mut out = vec![];
    let Some(argv0) = cmd.first() else { return out };
    let b0 = basename(argv0).to_string();
    out.push(b0.clone());
    let interp = INTERPRETERS
        .iter()
        .any(|i| b0 == *i || b0.starts_with(&format!("{i}.")));
    if interp && let Some(script) = cmd.iter().skip(1).find(|a| !a.starts_with('-')) {
        out.push(
            basename(script)
                .trim_end_matches(".js")
                .trim_end_matches(".mjs")
                .trim_end_matches(".py")
                .to_string(),
        );
        out.extend(
            script
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    if !interp {
        out.extend(
            argv0
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    out
}

pub fn match_tool<'a>(cmd: &[String], tools: &'a [Tool]) -> Option<&'a Tool> {
    let toks = tokens(cmd);
    tools.iter().find(|t| {
        t.matches
            .iter()
            .any(|m| toks.iter().any(|x| x.eq_ignore_ascii_case(m)))
    })
}

/// The program in a process title: its first word, basename, without a trailing ':'.
/// Programs that rewrite their title put arguments there ("npm run deploy -- --key=...",
/// "tmux: server"), so everything after the first whitespace is dropped.
fn title_program(s: &str) -> &str {
    let word = s.split_whitespace().next().unwrap_or("");
    basename(word).trim_end_matches(':')
}

/// Program name to store: the tool id, else the executable's basename. Never arguments.
pub fn program_name(cmd: &[String], tools: &[Tool]) -> String {
    program_name_ex(cmd, None, None, tools)
}

/// Like program_name, with the process's comm and /proc/<pid>/exe target, used instead of
/// argv[0] when argv[0] holds a rewritten process title (it contains whitespace).
pub fn program_name_ex(
    cmd: &[String],
    comm: Option<&str>,
    exe: Option<&str>,
    tools: &[Tool],
) -> String {
    if let Some(t) = match_tool(cmd, tools) {
        return t.id.clone();
    }
    let Some(argv0) = cmd.first() else {
        return String::new();
    };
    if argv0.chars().any(char::is_whitespace) {
        for s in [comm, exe].into_iter().flatten() {
            let p = title_program(s);
            if !p.is_empty() {
                return p.to_string();
            }
        }
    }
    title_program(argv0).to_string()
}

#[derive(Default)]
pub struct Projects {
    cache: HashMap<PathBuf, String>,
}

impl Projects {
    /// Git root of dir (home abbreviated), else dir itself.
    pub fn of(&mut self, dir: &Path) -> String {
        if let Some(p) = self.cache.get(dir) {
            return p.clone();
        }
        let mut cur = Some(dir);
        let mut root = None;
        while let Some(d) = cur {
            if d.join(".git").exists() {
                root = Some(d.to_path_buf());
                break;
            }
            cur = d.parent();
        }
        let p = util::abbreviate_home(root.as_deref().unwrap_or(dir));
        if self.cache.len() > 2000 {
            self.cache.clear();
        }
        self.cache.insert(dir.to_path_buf(), p.clone());
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Agents;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn recognizes_tools() {
        let tools = Agents::default().tools;
        assert_eq!(
            program_name(&v(&["claude", "--dangerously-skip-permissions"]), &tools),
            "claude"
        );
        assert_eq!(
            program_name(
                &v(&[
                    "node",
                    "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
                ]),
                &tools
            ),
            "claude"
        );
        assert_eq!(
            program_name(&v(&["/home/user/.opencode/bin/opencode"]), &tools),
            "opencode"
        );
        assert_eq!(
            program_name(
                &v(&["codex", "--dangerously-bypass-approvals-and-sandbox"]),
                &tools
            ),
            "codex"
        );
        assert_eq!(
            program_name(&v(&["/usr/bin/nvim", "secret.txt"]), &tools),
            "nvim"
        );
        assert_eq!(
            program_name(&v(&["python3", "-m", "http.server"]), &tools),
            "python3"
        );
        // Rewritten process titles carry arguments: only the program is kept.
        assert_eq!(
            program_name(&v(&["npm run deploy -- --key=abc123"]), &tools),
            "npm"
        );
        assert_eq!(program_name(&v(&["tmux: server"]), &tools), "tmux");
        assert_eq!(
            program_name(&v(&["/usr/local/bin/worker --token=abc"]), &tools),
            "worker"
        );
        assert_eq!(
            program_name_ex(
                &v(&["/opt/Some App/bin/app --flag=1"]),
                Some("app"),
                Some("/opt/Some App/bin/app"),
                &tools
            ),
            "app"
        );
        assert_eq!(
            program_name_ex(
                &v(&["npm run deploy -- --key=abc123"]),
                Some("npm run deploy "),
                Some("/usr/bin/node"),
                &tools
            ),
            "npm"
        );
        assert_eq!(
            program_name_ex(&v(&["x y"]), Some(""), Some("/usr/bin/real"), &tools),
            "real"
        );
        // An MCP helper launched by an agent is not the agent.
        assert_eq!(
            program_name(
                &v(&[
                    "node",
                    "/home/user/.npm/_npx/a17/node_modules/.bin/rea",
                    "mcp"
                ]),
                &tools
            ),
            "node"
        );
    }
}
