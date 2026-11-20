//! `warren connect <agent>`: register the Warren MCP server (`warren-mcp`,
//! found next to `warren`) with a coding agent, carrying `WARREN_API_URL` and
//! `WARREN_API_KEY` from the CLI config; and `warren skill install`, which
//! writes the Warren skill for Claude Code.
//!
//! Claude Code is configured through `claude mcp add`; every other agent has a
//! JSON (or, for Codex, TOML) file that is merged: other servers and settings
//! stay, the `warren` entry is replaced, and the previous file is kept as
//! `<file>.bak-<unix time>`. `--print` shows the result without writing.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};
use warren_cli::{
    EXIT_OK, Failure,
    cli::{ConnectArgs, ConnectTarget, SkillCmd},
    util::{find_in_path, shell_join},
};

use super::{Ctx, setup};

/// The MCP server's name in every client.
pub const SERVER_NAME: &str = "warren";
const SKILL_MD: &str = include_str!("../../../../skills/warren/SKILL.md");

/// The server definition every renderer starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// Absolute path of `warren-mcp` (or the bare name when it is not installed).
    pub command: String,
    /// `WARREN_API_URL` and `WARREN_API_KEY`.
    pub env: BTreeMap<String, String>,
}

impl McpServer {
    fn new(command: String, api_url: &str, api_key: &str) -> McpServer {
        let mut env = BTreeMap::new();
        env.insert("WARREN_API_URL".to_string(), api_url.to_string());
        env.insert("WARREN_API_KEY".to_string(), api_key.to_string());
        McpServer { command, env }
    }

    fn env_json(&self) -> Value {
        Value::Object(
            self.env
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect::<Map<_, _>>(),
        )
    }

    /// `{ "command", "args": [], "env" }`: Cursor, Windsurf, Caret, Gemini, Claude Desktop.
    pub fn standard_json(&self) -> Value {
        json!({ "command": self.command, "args": [], "env": self.env_json() })
    }

    /// OpenCode's `mcp.servers.<name>` entry (`type: local`, `command` as an array, `environment`).
    pub fn opencode_json(&self) -> Value {
        json!({ "type": "local", "command": [self.command], "environment": self.env_json() })
    }

    /// The `[mcp_servers.warren]` table for Codex's config.toml.
    pub fn codex_toml(&self) -> String {
        let env: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k} = {}", toml_string(v)))
            .collect();
        format!(
            "[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = []\nenv = {{ {} }}\n",
            toml_string(&self.command),
            env.join(", ")
        )
    }

    /// `claude mcp add warren --scope <scope> -e K=V ... -- <command>`.
    pub fn claude_command(&self, scope: &str) -> Vec<String> {
        let mut argv = vec![
            "claude".to_string(),
            "mcp".into(),
            "add".into(),
            SERVER_NAME.into(),
            "--scope".into(),
            scope.into(),
        ];
        for (k, v) in &self.env {
            argv.push("-e".into());
            argv.push(format!("{k}={v}"));
        }
        argv.push("--".into());
        argv.push(self.command.clone());
        argv
    }
}

/// A TOML basic string.
pub fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Where each agent keeps its MCP configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `claude mcp add` with this scope (`user` or `project`).
    ClaudeCli { scope: &'static str },
    /// A JSON file whose `<key>` object maps server names to `standard_json`.
    Json { path: PathBuf, key: &'static str },
    /// OpenCode's JSON (`mcp.servers`).
    OpenCode { path: PathBuf },
    /// Codex's TOML.
    Toml { path: PathBuf },
}

impl Target {
    /// The file this target writes, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Target::ClaudeCli { .. } => None,
            Target::Json { path, .. } | Target::OpenCode { path } | Target::Toml { path } => {
                Some(path)
            }
        }
    }
}

/// Pick the file (or command) for an agent, user- or project-level.
pub fn target_for(
    agent: ConnectTarget,
    project: bool,
    home: &Path,
    cwd: &Path,
) -> Result<Target, Failure> {
    let no_project = |what: &str| {
        Err(Failure::usage(format!(
            "{what} has no project-level MCP configuration; drop --project"
        )))
    };
    Ok(match (agent, project) {
        (ConnectTarget::Claude, false) => Target::ClaudeCli { scope: "user" },
        (ConnectTarget::Claude, true) => Target::ClaudeCli { scope: "project" },
        (ConnectTarget::Codex, false) => Target::Toml {
            path: home.join(".codex/config.toml"),
        },
        (ConnectTarget::Codex, true) => return no_project("Codex"),
        (ConnectTarget::Cursor, false) => Target::Json {
            path: home.join(".cursor/mcp.json"),
            key: "mcpServers",
        },
        (ConnectTarget::Cursor, true) => Target::Json {
            path: cwd.join(".cursor/mcp.json"),
            key: "mcpServers",
        },
        (ConnectTarget::Windsurf, false) => Target::Json {
            path: home.join(".codeium/windsurf/mcp_config.json"),
            key: "mcpServers",
        },
        (ConnectTarget::Windsurf, true) => return no_project("Windsurf"),
        (ConnectTarget::Caret, false) => Target::Json {
            path: home.join(".caret/mcp.json"),
            key: "mcpServers",
        },
        (ConnectTarget::Caret, true) => Target::Json {
            path: cwd.join(".caret/mcp.json"),
            key: "mcpServers",
        },
        (ConnectTarget::Opencode, false) => Target::OpenCode {
            path: std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("opencode/opencode.json"),
        },
        (ConnectTarget::Opencode, true) => Target::OpenCode {
            path: cwd.join("opencode.json"),
        },
        (ConnectTarget::Gemini, false) => Target::Json {
            path: home.join(".gemini/settings.json"),
            key: "mcpServers",
        },
        (ConnectTarget::Gemini, true) => Target::Json {
            path: cwd.join(".gemini/settings.json"),
            key: "mcpServers",
        },
    })
}

fn agent_name(agent: ConnectTarget) -> &'static str {
    match agent {
        ConnectTarget::Claude => "Claude Code",
        ConnectTarget::Codex => "Codex",
        ConnectTarget::Cursor => "Cursor",
        ConnectTarget::Windsurf => "Windsurf",
        ConnectTarget::Caret => "Caret",
        ConnectTarget::Opencode => "OpenCode",
        ConnectTarget::Gemini => "Gemini CLI",
    }
}

/// Parse a JSON config file's text; empty means `{}`.
fn parse_object(text: &str, path: &Path) -> Result<Map<String, Value>, Failure> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let v: Value = serde_json::from_str(text).map_err(|e| {
        Failure::local(format!(
            "{}: not valid JSON ({e}); fix it or add the server by hand (`--print` shows the entry)",
            path.display()
        ))
    })?;
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(Failure::local(format!(
            "{}: the top level must be a JSON object",
            path.display()
        ))),
    }
}

/// Set `<key>.warren` in a JSON document, keeping everything else.
pub fn merge_json(
    existing: &str,
    path: &Path,
    key: &str,
    server: Value,
) -> Result<String, Failure> {
    let mut root = parse_object(existing, path)?;
    let servers = root
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        return Err(Failure::local(format!(
            "{}: {key:?} is not an object",
            path.display()
        )));
    };
    servers.insert(SERVER_NAME.to_string(), server);
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|e| Failure::local(e.to_string()))?
    ))
}

/// Set `mcp.servers.warren` for OpenCode, removing a deprecated flat `mcp.warren` entry.
pub fn merge_opencode(existing: &str, path: &Path, server: Value) -> Result<String, Failure> {
    let mut root = parse_object(existing, path)?;
    if !root.contains_key("$schema") && root.is_empty() {
        root.insert(
            "$schema".to_string(),
            Value::String("https://opencode.ai/config.json".to_string()),
        );
    }
    let mcp = root
        .entry("mcp".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(mcp) = mcp else {
        return Err(Failure::local(format!(
            "{}: \"mcp\" is not an object",
            path.display()
        )));
    };
    mcp.remove(SERVER_NAME);
    let servers = mcp
        .entry("servers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        return Err(Failure::local(format!(
            "{}: \"mcp.servers\" is not an object",
            path.display()
        )));
    };
    servers.insert(SERVER_NAME.to_string(), server);
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|e| Failure::local(e.to_string()))?
    ))
}

/// Replace the `[mcp_servers.warren]` table (and its subtables) in a TOML
/// document with `block`, keeping every other line as it is.
pub fn merge_toml(existing: &str, block: &str) -> String {
    let mut kept = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            skipping = is_warren_table(t);
        }
        if !skipping {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    let mut out = kept.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    out
}

/// `[mcp_servers.warren]`, `[mcp_servers."warren"]`, `[mcp_servers.warren.env]`, ...
fn is_warren_table(header: &str) -> bool {
    let inner = header
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or("")
        .replace(['"', '\'', ' ', '\t'], "");
    inner == format!("mcp_servers.{SERVER_NAME}")
        || inner.starts_with(&format!("mcp_servers.{SERVER_NAME}."))
}

/// Where `warren-mcp` is: next to `warren`, on PATH, else the bare name.
fn mcp_command() -> (String, bool) {
    match setup::find_tool("warren-mcp") {
        Some(p) => (p.display().to_string(), true),
        None => ("warren-mcp".to_string(), false),
    }
}

/// Copy `path` to `<path>.bak-<unix time>` when it exists; returns the backup path.
fn back_up(path: &Path) -> Result<Option<PathBuf>, Failure> {
    if !path.exists() {
        return Ok(None);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".bak-{ts}"));
    let backup = path.with_file_name(name);
    fs::copy(path, &backup).map_err(|e| Failure::local(format!("{}: {e}", backup.display())))?;
    Ok(Some(backup))
}

/// Write a config file (mode 0600 when new), creating parent directories.
fn write_config(path: &Path, body: &str) -> Result<(), Failure> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| Failure::local(format!("{}: {e}", dir.display())))?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Failure::local(format!("{}: {e}", path.display())))?;
    f.write_all(body.as_bytes())
        .map_err(|e| Failure::local(format!("{}: {e}", path.display())))
}

/// `warren connect <agent> [--project] [--print]`
pub fn connect(ctx: &Ctx, args: ConnectArgs) -> Result<i32, Failure> {
    let Some(key) = ctx.settings.api_key.clone() else {
        return Err(Failure::usage(
            "no API key configured: run `warren up` (or `warren login --api-url URL --api-key KEY`) first",
        ));
    };
    let home = setup::home_dir()?;
    let cwd =
        std::env::current_dir().map_err(|e| Failure::local(format!("current directory: {e}")))?;
    let (command, found) = mcp_command();
    let server = McpServer::new(command, &ctx.settings.api_url, &key);
    let target = target_for(args.agent, args.project, &home, &cwd)?;
    let name = agent_name(args.agent);
    let mut notes: Vec<String> = Vec::new();
    if !found {
        notes.push(
            "warren-mcp was not found next to warren or on PATH; the entry uses the bare name, install it before starting the agent (docs/install.md)".to_string(),
        );
    }

    match &target {
        Target::ClaudeCli { scope } => {
            let argv = server.claude_command(scope);
            let shown = redact(&shell_join(&argv), &key);
            if *scope == "project" {
                notes.push("--scope project writes .mcp.json in this directory with the key in it; keep it out of version control or replace the key with ${WARREN_API_KEY}".to_string());
            }
            let claude = find_in_path("claude");
            if args.print || claude.is_none() {
                if ctx.json {
                    println!(
                        "{}",
                        warren_cli::output::to_json_pretty(&json!({
                            "agent": name, "command": argv, "ran": false, "notes": notes
                        }))
                    );
                    return Ok(EXIT_OK);
                }
                if claude.is_none() && !args.print {
                    notes
                        .push("`claude` is not on PATH; run this once it is installed".to_string());
                }
                println!("{shown}");
                for n in &notes {
                    eprintln!("note: {n}");
                }
                return Ok(EXIT_OK);
            }
            // Replace an existing entry: `add` refuses duplicates.
            let _ = Command::new("claude")
                .args(["mcp", "remove", SERVER_NAME, "--scope", scope])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let status = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .status()
                .map_err(|e| Failure::local(format!("claude: {e}")))?;
            if !status.success() {
                return Err(Failure::local(format!("`{shown}` failed with {status}")));
            }
            if ctx.json {
                println!(
                    "{}",
                    warren_cli::output::to_json_pretty(&json!({
                        "agent": name, "command": argv, "ran": true, "notes": notes
                    }))
                );
            } else {
                println!(
                    "Registered the {SERVER_NAME} MCP server with {name} (scope {scope}): {shown}"
                );
                println!("  server   {}", server.command);
                println!("  API      {}", ctx.settings.api_url);
                println!("Check with `claude mcp list`; new Claude Code sessions see the tools.");
                for n in &notes {
                    eprintln!("note: {n}");
                }
            }
            Ok(EXIT_OK)
        }
        Target::Json { .. } | Target::OpenCode { .. } | Target::Toml { .. } => {
            let path = target.path().expect("file targets have a path");
            let existing = match fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(Failure::local(format!("{}: {e}", path.display()))),
            };
            let body = match &target {
                Target::Json { key, .. } => {
                    merge_json(&existing, path, key, server.standard_json())?
                }
                Target::OpenCode { .. } => merge_opencode(&existing, path, server.opencode_json())?,
                Target::Toml { .. } => merge_toml(&existing, &server.codex_toml()),
                Target::ClaudeCli { .. } => unreachable!(),
            };
            if args.print {
                if ctx.json {
                    println!(
                        "{}",
                        warren_cli::output::to_json_pretty(&json!({
                            "agent": name, "path": path, "written": false, "content": body, "notes": notes
                        }))
                    );
                } else {
                    println!("# {} ({name})", path.display());
                    print!("{}", redact(&body, &key));
                    for n in &notes {
                        eprintln!("note: {n}");
                    }
                }
                return Ok(EXIT_OK);
            }
            let backup = if existing == body {
                None
            } else {
                back_up(path)?
            };
            if existing != body {
                write_config(path, &body)?;
            }
            if ctx.json {
                println!(
                    "{}",
                    warren_cli::output::to_json_pretty(&json!({
                        "agent": name, "path": path, "written": existing != body, "backup": backup, "notes": notes
                    }))
                );
            } else {
                if existing == body {
                    println!(
                        "{} already has the {SERVER_NAME} MCP server for {name}; nothing changed.",
                        path.display()
                    );
                } else {
                    println!(
                        "Registered the {SERVER_NAME} MCP server for {name} in {}{}",
                        path.display(),
                        backup
                            .as_ref()
                            .map(|b| format!(" (previous file kept as {})", b.display()))
                            .unwrap_or_default()
                    );
                }
                println!("  server   {}", server.command);
                println!("  API      {}", ctx.settings.api_url);
                println!("Restart {name} (or reload its MCP servers) to pick it up.");
                for n in &notes {
                    eprintln!("note: {n}");
                }
            }
            Ok(EXIT_OK)
        }
    }
}

/// Hide all but the prefix of the key in printed text.
fn redact(text: &str, key: &str) -> String {
    if key.len() <= 14 {
        return text.to_string();
    }
    text.replace(key, &format!("{}...", &key[..14]))
}

/// `warren skill install [--project]`
pub fn skill(ctx: &Ctx, cmd: SkillCmd) -> Result<i32, Failure> {
    match cmd {
        SkillCmd::Install { project } => {
            let base = if project {
                std::env::current_dir()
                    .map_err(|e| Failure::local(format!("current directory: {e}")))?
            } else {
                setup::home_dir()?
            };
            let path = skill_path(&base);
            let existing = fs::read_to_string(&path).ok();
            let changed = existing.as_deref() != Some(SKILL_MD);
            let backup = if changed && existing.is_some() {
                back_up(&path)?
            } else {
                None
            };
            if changed {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)
                        .map_err(|e| Failure::local(format!("{}: {e}", dir.display())))?;
                }
                fs::write(&path, SKILL_MD)
                    .map_err(|e| Failure::local(format!("{}: {e}", path.display())))?;
            }
            if ctx.json {
                println!(
                    "{}",
                    warren_cli::output::to_json_pretty(&json!({
                        "path": path, "written": changed, "backup": backup
                    }))
                );
            } else if changed {
                println!(
                    "Installed the Warren skill at {}{}",
                    path.display(),
                    backup
                        .map(|b| format!(" (previous file kept as {})", b.display()))
                        .unwrap_or_default()
                );
                println!(
                    "Claude Code loads it in new sessions; `/warren` or any sandbox-shaped task triggers it."
                );
            } else {
                println!("{} is already current.", path.display());
            }
            Ok(EXIT_OK)
        }
    }
}

/// `<base>/.claude/skills/warren/SKILL.md`
pub fn skill_path(base: &Path) -> PathBuf {
    base.join(".claude/skills/warren/SKILL.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> McpServer {
        McpServer::new(
            "/usr/local/bin/warren-mcp".into(),
            "http://api.warren.localhost:7337",
            "sk_live_secret",
        )
    }

    #[test]
    fn renderers() {
        let s = server();
        assert_eq!(
            s.standard_json(),
            json!({"command": "/usr/local/bin/warren-mcp", "args": [], "env": {"WARREN_API_KEY": "sk_live_secret", "WARREN_API_URL": "http://api.warren.localhost:7337"}})
        );
        assert_eq!(
            s.opencode_json(),
            json!({"type": "local", "command": ["/usr/local/bin/warren-mcp"], "environment": {"WARREN_API_KEY": "sk_live_secret", "WARREN_API_URL": "http://api.warren.localhost:7337"}})
        );
        assert_eq!(
            s.codex_toml(),
            "[mcp_servers.warren]\ncommand = \"/usr/local/bin/warren-mcp\"\nargs = []\nenv = { WARREN_API_KEY = \"sk_live_secret\", WARREN_API_URL = \"http://api.warren.localhost:7337\" }\n"
        );
        assert_eq!(
            s.claude_command("user"),
            vec![
                "claude",
                "mcp",
                "add",
                "warren",
                "--scope",
                "user",
                "-e",
                "WARREN_API_KEY=sk_live_secret",
                "-e",
                "WARREN_API_URL=http://api.warren.localhost:7337",
                "--",
                "/usr/local/bin/warren-mcp"
            ]
        );
        assert_eq!(toml_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(
            redact(
                "key sk_live_abcdefghijklmnop end",
                "sk_live_abcdefghijklmnop"
            ),
            "key sk_live_abcdef... end"
        );
    }

    #[test]
    fn json_merge_keeps_other_servers() {
        let p = Path::new("/x/mcp.json");
        let existing = r#"{"mcpServers": {"github": {"command": "gh-mcp"}}, "other": 1}"#;
        let out = merge_json(existing, p, "mcpServers", server().standard_json()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["other"], 1);
        assert_eq!(v["mcpServers"]["github"]["command"], "gh-mcp");
        assert_eq!(
            v["mcpServers"]["warren"]["command"],
            "/usr/local/bin/warren-mcp"
        );
        assert_eq!(
            v["mcpServers"]["warren"]["env"]["WARREN_API_KEY"],
            "sk_live_secret"
        );
        assert!(out.ends_with('\n'));
        // Empty file and replacement of an old entry.
        let out = merge_json("", p, "mcpServers", server().standard_json()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["warren"]["args"], json!([]));
        let old = r#"{"mcpServers": {"warren": {"command": "old", "env": {"X": "1"}}}}"#;
        let v: Value = serde_json::from_str(
            &merge_json(old, p, "mcpServers", server().standard_json()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["mcpServers"]["warren"]["command"],
            "/usr/local/bin/warren-mcp"
        );
        assert!(v["mcpServers"]["warren"]["env"].get("X").is_none());
        // Bad input.
        assert!(merge_json("not json", p, "mcpServers", json!({})).is_err());
        assert!(merge_json("[]", p, "mcpServers", json!({})).is_err());
        assert!(merge_json(r#"{"mcpServers": 5}"#, p, "mcpServers", json!({})).is_err());
    }

    #[test]
    fn opencode_merge_uses_mcp_servers_and_drops_the_flat_entry() {
        let p = Path::new("/x/opencode.json");
        let existing = r#"{"$schema": "https://opencode.ai/config.json", "model": "x", "mcp": {"warren": {"type": "local", "command": ["old"], "enabled": true}, "servers": {"gh": {"type": "remote", "url": "https://x"}}}}"#;
        let out = merge_opencode(existing, p, server().opencode_json()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["model"], "x");
        assert!(v["mcp"].get("warren").is_none());
        assert_eq!(v["mcp"]["servers"]["gh"]["url"], "https://x");
        assert_eq!(v["mcp"]["servers"]["warren"]["type"], "local");
        assert_eq!(
            v["mcp"]["servers"]["warren"]["command"],
            json!(["/usr/local/bin/warren-mcp"])
        );
        assert_eq!(
            v["mcp"]["servers"]["warren"]["environment"]["WARREN_API_URL"],
            "http://api.warren.localhost:7337"
        );
        let v: Value =
            serde_json::from_str(&merge_opencode("", p, server().opencode_json()).unwrap())
                .unwrap();
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["mcp"]["servers"]["warren"]["type"], "local");
    }

    #[test]
    fn toml_merge_replaces_only_the_warren_tables() {
        let existing = "model = \"o3\"\n\n[mcp_servers.github]\ncommand = \"gh\"\n\n[mcp_servers.warren]\ncommand = \"old\"\n\n[mcp_servers.warren.env]\nX = \"1\"\n\n[profiles.fast]\nmodel = \"mini\"\n";
        let out = merge_toml(existing, &server().codex_toml());
        assert!(out.starts_with("model = \"o3\"\n\n[mcp_servers.github]\ncommand = \"gh\"\n\n[profiles.fast]\nmodel = \"mini\"\n\n[mcp_servers.warren]\n"), "{out}");
        assert_eq!(out.matches("[mcp_servers.warren]").count(), 1);
        assert!(!out.contains("command = \"old\""));
        assert!(!out.contains("X = \"1\""));
        assert!(out.contains("env = { WARREN_API_KEY = \"sk_live_secret\""));
        // Quoted key form and an empty file.
        let out = merge_toml(
            "[mcp_servers.\"warren\"]\ncommand = \"old\"\n",
            &server().codex_toml(),
        );
        assert_eq!(out.matches("command =").count(), 1);
        assert!(out.contains("/usr/local/bin/warren-mcp"));
        assert_eq!(
            merge_toml("", &server().codex_toml()),
            server().codex_toml()
        );
        assert!(is_warren_table("[ mcp_servers.warren ]"));
        assert!(!is_warren_table("[mcp_servers.warrenx]"));
        assert!(!is_warren_table("[mcp_servers.github]"));
    }

    #[test]
    fn targets() {
        let home = Path::new("/home/u");
        let cwd = Path::new("/proj");
        assert_eq!(
            target_for(ConnectTarget::Claude, false, home, cwd).unwrap(),
            Target::ClaudeCli { scope: "user" }
        );
        assert_eq!(
            target_for(ConnectTarget::Claude, true, home, cwd).unwrap(),
            Target::ClaudeCli { scope: "project" }
        );
        assert_eq!(
            target_for(ConnectTarget::Codex, false, home, cwd)
                .unwrap()
                .path(),
            Some(Path::new("/home/u/.codex/config.toml"))
        );
        assert!(target_for(ConnectTarget::Codex, true, home, cwd).is_err());
        assert_eq!(
            target_for(ConnectTarget::Cursor, true, home, cwd)
                .unwrap()
                .path(),
            Some(Path::new("/proj/.cursor/mcp.json"))
        );
        assert_eq!(
            target_for(ConnectTarget::Windsurf, false, home, cwd)
                .unwrap()
                .path(),
            Some(Path::new("/home/u/.codeium/windsurf/mcp_config.json"))
        );
        assert_eq!(
            target_for(ConnectTarget::Caret, true, home, cwd)
                .unwrap()
                .path(),
            Some(Path::new("/proj/.caret/mcp.json"))
        );
        assert_eq!(
            target_for(ConnectTarget::Opencode, true, home, cwd)
                .unwrap()
                .path(),
            Some(Path::new("/proj/opencode.json"))
        );
        assert_eq!(
            target_for(ConnectTarget::Gemini, false, home, cwd)
                .unwrap()
                .path(),
            Some(Path::new("/home/u/.gemini/settings.json"))
        );
        assert_eq!(
            skill_path(Path::new("/home/u")),
            PathBuf::from("/home/u/.claude/skills/warren/SKILL.md")
        );
    }

    #[test]
    fn skill_file_has_frontmatter_and_the_essentials() {
        assert!(SKILL_MD.starts_with("---\nname: warren\ndescription: "));
        for needle in [
            "create_instance",
            "exec",
            "warren run",
            "warren shell",
            "warren-macos",
            "two macOS",
            "sleep",
            "fork",
            "signed_url",
        ] {
            assert!(SKILL_MD.contains(needle), "SKILL.md lacks {needle}");
        }
    }
}
