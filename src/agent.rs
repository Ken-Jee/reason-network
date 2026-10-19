//! Which agent are we running under, what is its session, and how do we reach it.

use crate::{git::GitInfo, paths, util};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const CLAUDE_CODE: &str = "claude-code";

/// Best guess from the environment the agent gives its children. `REASON_NETWORK_AGENT` (set by `reason network setup` in the
/// MCP config) wins.
pub fn detect_env() -> Option<&'static str> {
    let has = |k: &str| std::env::var_os(k).is_some();
    if let Ok(a) = crate::paths::env_var("AGENT") {
        return from_client_name(&a);
    }
    if has("CLAUDECODE") || has("CLAUDE_CODE_SESSION_ID") {
        Some(CLAUDE_CODE)
    } else if has("CODEX_THREAD_ID") || has("CODEX_SESSION_ID") {
        Some("codex")
    } else if has("GROK_SESSION_ID") || has("GROK_WORKSPACE_ROOT") {
        Some("grok")
    } else if has("GEMINI_CLI") {
        Some("gemini")
    } else if has("CURSOR_AGENT") {
        Some("cursor")
    } else if has("OPENCODE") {
        Some("opencode")
    } else if has("COPILOT_CLI") || has("COPILOT_AGENT_SESSION_ID") {
        Some("copilot")
    } else {
        None
    }
}

/// From the MCP client's `clientInfo.name`, which beats the environment (agents get launched from each
/// other's terminals and inherit their variables).
pub fn from_client_name(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    let has = |s: &str| n.contains(s);
    Some(if has("claude-code") || has("claude code") {
        CLAUDE_CODE
    } else if has("codex") {
        "codex"
    } else if has("cursor") {
        "cursor"
    } else if has("gemini") {
        "gemini"
    } else if has("grok") {
        "grok"
    } else if has("opencode") {
        "opencode"
    } else if has("copilot") {
        "copilot"
    } else if n == "pi" || n.starts_with("pi-") || n.starts_with("pi ") {
        "pi"
    } else if n == "dsh" || n.starts_with("dsh-") {
        // DeepSeek Harness: its MCP client says `dsh-mcp-client`.
        "dsh"
    } else if has("reason") {
        "reason"
    } else if n == "amp" || n.starts_with("amp-") || n.starts_with("amp ") {
        "amp"
    } else if has("zed") {
        "zed"
    } else if has("devin") || has("windsurf") {
        "devin"
    } else if has("claude") {
        "claude-desktop"
    } else {
        return None;
    })
}

/// Agents that run one MCP server process for many sessions, so the session must come from each call.
pub fn multiplexed(agent: &str) -> bool {
    matches!(agent, "codex" | "opencode" | "cursor")
}

/// The session id an agent exports to its MCP servers, if any. `REASON_NETWORK_SESSION` is set by our own pi extension.
pub fn env_session_id(agent: &str) -> Option<String> {
    if let Some(id) = crate::paths::env_var("SESSION").ok().filter(|s| !s.is_empty()) {
        return Some(id);
    }
    let var = match agent {
        CLAUDE_CODE => "CLAUDE_CODE_SESSION_ID",
        "copilot" => "COPILOT_AGENT_SESSION_ID",
        "gemini" => "GEMINI_SESSION_ID",
        "codex" => "CODEX_THREAD_ID",
        "grok" => "GROK_SESSION_ID",
        "opencode" => "OPENCODE_SESSION_ID",
        _ => return None,
    };
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

/// The session id carried in a tool call's `_meta`. Codex sends `sessionId` (the root thread, the same id its
/// hooks see) and `threadId` (which differs inside subagents); opencode V2 sends `ai.opencode/sessionID`.
pub fn meta_session_id(meta: &serde_json::Map<String, Value>) -> Option<String> {
    ["sessionId", "ai.opencode/sessionID", "threadId"]
        .iter()
        .find_map(|k| meta.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Finds an executable on `PATH` or in the usual install places (a launchd/systemd daemon has a bare `PATH`). On
/// Windows that is `<name>.exe`, or the `<name>.cmd` npm installs.
pub fn find_bin(name: &str) -> Option<PathBuf> {
    if let Some(p) = crate::paths::env_var_os(&format!("{}_BIN", name.to_uppercase())) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let home = paths::user_home();
    #[cfg(unix)]
    let extra = [
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".npm-global/bin"),
        home.join(".cargo/bin"),
        home.join(".opencode/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ];
    #[cfg(windows)]
    let extra = [home.join(r".local\bin"), home.join(r"AppData\Roaming\npm"), home.join(r".bun\bin")];
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd"] } else { &[""] };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(extra)
        .flat_map(|d| exts.iter().map(move |ext| d.join(format!("{name}{ext}"))))
        .find(|p| executable(p))
}

#[cfg(unix)]
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn executable(p: &Path) -> bool {
    p.is_file()
}

/// The agent process behind a hook: our parent, skipping the shell some agents run hooks through.
pub fn hook_agent_pid() -> Option<u32> {
    let mut pid = util::parent_pid();
    for _ in 0..3 {
        let (ppid, name) = util::process(pid)?;
        if !matches!(name.as_str(), "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "cmd" | "powershell" | "pwsh") {
            return Some(pid);
        }
        pid = ppid;
    }
    Some(pid)
}

pub fn short(agent: &str) -> &str {
    match agent {
        CLAUDE_CODE | "claude-desktop" => "claude",
        "" => "agent",
        a => a,
    }
}

/// How people write the agent's name, for notes to senders.
pub fn label(agent: &str) -> &str {
    match agent {
        "codex" => "Codex",
        "grok" => "Grok",
        "reason" => "Reason",
        a => a,
    }
}

/// `<repo-or-dir>-<agent>`, e.g. `api-claude`.
pub fn default_name(agent: &str, cwd: &str, git: &GitInfo) -> String {
    let dir = git.root.as_deref().unwrap_or(cwd);
    let base = util::slug(Path::new(dir).file_name().and_then(|n| n.to_str()).unwrap_or("home"));
    let base = if base.is_empty() { "home".into() } else { base };
    util::joined(&base, short(agent))
}

pub fn cwd() -> String {
    std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "/".into())
}

/// The agent process: `CLAUDE_PID` when Claude Code sets it, else our parent.
pub fn agent_pid() -> u32 {
    std::env::var("CLAUDE_PID").ok().and_then(|s| s.parse().ok()).unwrap_or_else(util::parent_pid)
}

/// The inbox socket of the Claude Code session that spawned us. Posting a `user` frame there puts the text in
/// the conversation between tool calls, or starts a turn if the session is idle.
#[derive(Debug, Clone)]
pub struct ClaudeSocket {
    pub path: PathBuf,
    pub token: Option<String>,
}

pub fn claude_socket() -> Option<ClaudeSocket> {
    let token = std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").ok().filter(|s| !s.is_empty());
    if let Some(path) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET") {
        return Some(ClaudeSocket { path: path.into(), token });
    }
    // Not exported to this process: fall back to the session's registry entry.
    let pid = agent_pid();
    let file = paths::claude_sessions().join(format!("{pid}.json"));
    let s: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let path = s["messagingSocketPath"].as_str()?;
    Some(ClaudeSocket { path: path.into(), token: token.or_else(|| crate::claude::peer_token(pid)) })
}

pub fn post_to_claude(sock: &ClaudeSocket, text: &str) -> std::io::Result<()> {
    let mut c = open_inbox(&sock.path)?;
    if let Some(t) = &sock.token {
        writeln!(c, "{}", json!({ "type": "auth", "token": t }))?;
    }
    writeln!(c, "{}", json!({ "type": "user", "message": { "role": "user", "content": text } }))?;
    c.flush()
}

#[cfg(unix)]
fn open_inbox(path: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    let c = std::os::unix::net::UnixStream::connect(path)?;
    c.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    Ok(c)
}

/// On Windows the session's inbox is a named pipe, which opens like a file.
#[cfg(windows)]
fn open_inbox(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_names() {
        assert_eq!(from_client_name("claude-code"), Some(CLAUDE_CODE));
        assert_eq!(from_client_name("codex-mcp-client"), Some("codex"));
        assert_eq!(from_client_name("cursor-vscode"), Some("cursor"));
        assert_eq!(from_client_name("gemini-cli-mcp-client"), Some("gemini"));
        // grok names its client after the server, so this one also contains "reason".
        assert_eq!(from_client_name("grok-shell-reason-network"), Some("grok"));
        assert_eq!(from_client_name("dsh-mcp-client"), Some("dsh"));
        assert_eq!(from_client_name("ashdsh"), None);
        assert_eq!(from_client_name("claude-ai"), Some("claude-desktop"));
        assert_eq!(from_client_name("example-client"), None);
    }

    #[test]
    fn names() {
        let git = GitInfo { root: Some("/Users/alex/src/API".into()), ..Default::default() };
        assert_eq!(default_name(CLAUDE_CODE, "/Users/alex/src/API/web", &git), "api-claude");
        assert_eq!(default_name("codex", "/tmp/My Thing", &GitInfo::default()), "my-thing-codex");
    }
}
