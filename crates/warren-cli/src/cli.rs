//! The command-line surface of `warren`, declared with clap's derive API.
//!
//! Everything here is pure data: parsing is tested without a daemon or the SDK.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use warren_core::{InstanceType, NetworkMode};

/// Sandbox API base URL used when neither a flag, an environment variable nor
/// the config file names one.
pub const DEFAULT_API_URL: &str = "http://api.warren.localhost:7337";

/// Top-level invocation: global options plus one command.
#[derive(Debug, Parser)]
#[command(
    name = "warren",
    bin_name = "warren",
    version,
    about = "Persistent Linux computers for agents and people, reachable over HTTPS.",
    long_about = None,
    propagate_version = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Sandbox API base URL (for example http://api.warren.localhost:7337).
    #[arg(long, global = true, env = "WARREN_API_URL", value_name = "URL")]
    pub api_url: Option<String>,

    /// API key (sk_live_...). Falls back to the config file.
    #[arg(
        long,
        global = true,
        env = "WARREN_API_KEY",
        value_name = "KEY",
        hide_env_values = true
    )]
    pub api_key: Option<String>,

    /// Config file (default: $XDG_CONFIG_HOME/warren/config.json or ~/.config/warren/config.json).
    #[arg(long, global = true, env = "WARREN_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Print the raw JSON response instead of a table.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Every top-level command.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a workspace, print its API key and save it to the config file.
    Signup {
        /// Workspace name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Save --api-url and --api-key to the config file.
    Login,
    /// Show the workspace the configured key belongs to.
    Whoami,
    /// Templates: the built-in catalog and your own images.
    Templates {
        #[command(subcommand)]
        cmd: TemplatesCmd,
    },
    /// Instances: create, drive and inspect persistent computers.
    Instances {
        #[command(subcommand)]
        cmd: InstancesCmd,
    },
    /// API keys of the workspace.
    Keys {
        #[command(subcommand)]
        cmd: KeysCmd,
    },
    /// Talk to the agent running inside an instance.
    Chat(ChatArgs),
    /// Files in an instance's persistent home.
    Files {
        #[command(subcommand)]
        cmd: FilesCmd,
    },
    /// Daemon health (no authentication).
    Health,
    // ----- interactive terminal and workspace sync (shell.rs, runcmd.rs, sync.rs) -----
    /// Open an interactive shell, or run a command on a terminal, inside an instance.
    Shell(ShellArgs),
    /// Run a command in an instance bound to the current directory, syncing files both ways.
    Run(RunArgs),
    /// Copy the current directory to an instance and changed files back.
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
    },
    // ----- setup and integration (setup.rs, doctor.rs, mac.rs, connect.rs) -----
    /// Start the daemon on this machine as a service, wait for it and save an API key.
    Up(UpArgs),
    /// Stop the daemon `warren up` started; --purge also removes the service and, after confirmation, the data.
    Down(DownArgs),
    /// Service state, health, runtimes and instance count of the daemon on this machine.
    Status,
    /// Check this machine for everything Warren needs; exits 1 when something fails.
    Doctor,
    /// macOS instances: prepare this Mac (Lume and the base VM).
    Mac {
        #[command(subcommand)]
        cmd: MacCmd,
    },
    /// Register the Warren MCP server with a coding agent.
    Connect(ConnectArgs),
    /// Skill files that teach a coding agent how to use Warren.
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },
}

/// Container runtime choice of `warren up`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RuntimeChoice {
    /// Docker when it is reachable (gVisor when registered), else no containers.
    #[default]
    Auto,
    /// No Docker: only `mac` instances (macOS) or nothing but the API.
    None,
    /// Docker is required; fail when it is not reachable.
    Docker,
}

/// Arguments of `warren up`.
#[derive(Debug, Args, Default)]
pub struct UpArgs {
    /// Port of the API, the dashboard and the instance URLs (default 7337).
    #[arg(long, value_name = "N")]
    pub port: Option<u16>,
    /// Data directory (default: ~/Library/Application Support/Warren on macOS, $XDG_DATA_HOME/warren or ~/.local/share/warren on Linux).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Base domain of every URL (default warren.localhost).
    #[arg(long, value_name = "DOMAIN")]
    pub base_domain: Option<String>,
    /// macOS: the Lume base VM for mac instances (default warren-macos-26).
    #[arg(long, value_name = "NAME")]
    pub mac_base_vm: Option<String>,
    /// Tag of the built-in images the daemon uses (`warren/base:<tag>` and
    /// friends); the images must exist in the Docker engine (default dev).
    #[arg(long, value_name = "TAG")]
    pub image_tag: Option<String>,
    /// Container runtime: auto (Docker when reachable), none (no Docker) or docker (Docker required).
    #[arg(long, value_enum, default_value_t = RuntimeChoice::Auto)]
    pub runtime: RuntimeChoice,
    /// Development mode for the daemon (5 s idle timeouts, host.docker.internal in instances).
    #[arg(long)]
    pub dev: bool,
    /// Install no service: start warrend as a background process and record its pid in <data>/warrend.pid.
    #[arg(long)]
    pub no_service: bool,
    /// The warrend binary (default: next to this binary, then PATH).
    #[arg(long, env = "WARREN_WARREND", value_name = "PATH")]
    pub warrend_bin: Option<PathBuf>,
    /// Listen address handed to warrend (default 127.0.0.1:<port>; `:<port>` binds every interface).
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<String>,
}

/// Arguments of `warren down`.
#[derive(Debug, Args, Default)]
pub struct DownArgs {
    /// Also remove the service definition and, after confirmation, the data directory.
    #[arg(long)]
    pub purge: bool,
    /// Answer yes to the confirmation (needed to remove the data directory without a terminal).
    #[arg(long)]
    pub yes: bool,
}

/// `warren mac ...`
#[derive(Debug, Subcommand)]
pub enum MacCmd {
    /// Build the macOS base VM this Mac serves `mac` instances from (needs Lume 0.7.2 or newer).
    Init(MacInitArgs),
}

/// Arguments of `warren mac init`.
#[derive(Debug, Args, Default)]
pub struct MacInitArgs {
    /// An existing stock macOS VM to clone (user lume, SSH on, stopped). Without it one is created from a restore image.
    #[arg(long, value_name = "VM")]
    pub source: Option<String>,
    /// Restore image for the stock VM: `latest` or the path of an .ipsw file.
    #[arg(long, value_name = "latest|PATH", default_value = "latest")]
    pub ipsw: String,
    /// Name of the stock VM created without --source.
    #[arg(long, value_name = "NAME", default_value = "base")]
    pub stock: String,
    /// Lume unattended-setup preset for the stock VM: tahoe (macOS 26) or sequoia (macOS 15).
    #[arg(long, value_name = "PRESET", default_value = "tahoe")]
    pub unattended: String,
    /// Name of the base VM to build.
    #[arg(long, value_name = "NAME", default_value = "warren-macos-26")]
    pub target: String,
    /// Also install Node LTS and the claude, codex and opencode CLIs in the image (needs internet in the VM).
    #[arg(long)]
    pub with_agents: bool,
    /// Replace the target VM when it already exists.
    #[arg(long)]
    pub force: bool,
    /// Print every step and command without running anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// The coding agents `warren connect` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ConnectTarget {
    /// Claude Code (`claude mcp add`).
    Claude,
    /// Codex CLI (~/.codex/config.toml).
    Codex,
    /// Cursor (~/.cursor/mcp.json or .cursor/mcp.json).
    Cursor,
    /// Windsurf (~/.codeium/windsurf/mcp_config.json).
    Windsurf,
    /// Caret (~/.caret/mcp.json or .caret/mcp.json).
    Caret,
    /// OpenCode (~/.config/opencode/opencode.json or ./opencode.json).
    Opencode,
    /// Gemini CLI (~/.gemini/settings.json or .gemini/settings.json).
    Gemini,
}

/// Arguments of `warren connect`.
#[derive(Debug, Args)]
pub struct ConnectArgs {
    /// The agent to configure.
    #[arg(value_enum)]
    pub agent: ConnectTarget,
    /// Write the project-level configuration in the current directory instead of the user-level one.
    #[arg(long)]
    pub project: bool,
    /// Show what would be written or run instead of doing it.
    #[arg(long)]
    pub print: bool,
}

/// `warren skill ...`
#[derive(Debug, Subcommand)]
pub enum SkillCmd {
    /// Write the Warren skill to ~/.claude/skills/warren/SKILL.md (or .claude/skills/warren/ in the project).
    Install {
        /// Install into the current directory's .claude/skills instead of the home directory.
        #[arg(long)]
        project: bool,
    },
}

/// Arguments of `warren shell`.
#[derive(Debug, Args)]
pub struct ShellArgs {
    /// Instance id.
    #[arg(required_unless_present = "gateway_url")]
    pub id: Option<String>,
    /// Working directory inside the instance, relative to the home.
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<String>,
    /// Connect straight to a gateway (no daemon), for testing.
    #[arg(long, hide = true, value_name = "URL", requires = "gateway_token")]
    pub gateway_url: Option<String>,
    /// Instance token for --gateway-url.
    #[arg(long, hide = true, value_name = "TOKEN")]
    pub gateway_token: Option<String>,
    /// A command to run on the terminal instead of a login shell, after `--`.
    /// A single argument is passed to the shell verbatim; several are quoted and joined.
    #[arg(last = true, num_args = 0..)]
    pub command: Vec<String>,
}

/// Arguments of `warren run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Create a fresh instance even when the directory is already bound.
    #[arg(long)]
    pub new: bool,
    /// Instance type when creating: default, vm or mac.
    #[arg(long = "type", value_name = "default|vm|mac", value_parser = parse_instance_type)]
    pub kind: Option<InstanceType>,
    /// Template when creating (default warren-base; warren-macos for --type mac).
    #[arg(long)]
    pub template: Option<String>,
    /// Display name when creating.
    #[arg(long)]
    pub name: Option<String>,
    /// Remote directory the current directory maps to (default: <mount_path>/work/<directory name>).
    #[arg(long, value_name = "PATH")]
    pub remote_root: Option<String>,
    /// Neither push before nor pull after the command.
    #[arg(long)]
    pub no_sync: bool,
    /// Run through exec without a terminal and print the captured output when it ends.
    #[arg(long)]
    pub no_tty: bool,
    /// Forget the instance bound to the current directory.
    #[arg(long, conflicts_with_all = ["new", "no_sync", "no_tty", "command"])]
    pub unbind: bool,
    /// With --unbind: destroy the bound instance as well.
    #[arg(long, requires = "unbind")]
    pub delete: bool,
    /// The command to run, after `--`. A single argument is passed to the shell verbatim; several are quoted and joined.
    #[arg(last = true, num_args = 0.., required_unless_present = "unbind")]
    pub command: Vec<String>,
}

/// `warren sync ...`
#[derive(Debug, Subcommand)]
pub enum SyncCmd {
    /// Upload the current directory (honouring .gitignore) to the instance.
    Push(SyncArgs),
    /// Download the files changed on the instance since the last sync.
    Pull(SyncArgs),
}

/// Arguments of `warren sync push|pull`.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Instance id (default: the instance `warren run` bound to the current directory).
    pub id: Option<String>,
    /// Remote directory (default: the bound remote root, else <mount_path>/work/<directory name>).
    #[arg(long, value_name = "PATH")]
    pub remote_root: Option<String>,
    /// A directory or file name to leave out (repeatable), on top of the defaults.
    #[arg(long = "exclude", value_name = "NAME")]
    pub exclude: Vec<String>,
}

/// `warren templates ...`
#[derive(Debug, Subcommand)]
pub enum TemplatesCmd {
    /// List the system catalog and the workspace templates.
    List,
    /// Show one template (`name` or `name@revision`).
    Get {
        /// Template name.
        name: String,
    },
    /// Register (or publish a new revision of) a workspace template.
    Register(RegisterArgs),
    /// Delete a workspace template.
    Delete {
        /// Template name.
        name: String,
    },
}

/// Arguments of `warren templates register`.
#[derive(Debug, Args)]
pub struct RegisterArgs {
    /// Template name: lowercase letters, digits and dashes, 3..40 characters.
    #[arg(long)]
    pub name: String,
    /// Image reference, e.g. ghcr.io/you/agent:1.2.
    #[arg(long)]
    pub image: String,
    /// Port the main service listens on (0: the bare instance URL serves nothing).
    #[arg(long)]
    pub port: Option<u16>,
    /// Mount path of the persistent disk, e.g. /home/agent.
    #[arg(long, value_name = "PATH")]
    pub mount: Option<String>,
    /// The image serves the Agent API on its port.
    #[arg(long)]
    pub agent: bool,
    /// Human description.
    #[arg(long)]
    pub description: Option<String>,
    /// Default environment variable (repeatable).
    #[arg(long = "env", value_name = "K=V")]
    pub env: Vec<String>,
}

/// `warren instances ...`
#[derive(Debug, Subcommand)]
pub enum InstancesCmd {
    /// List the workspace's instances.
    List,
    /// Create an instance.
    Create(Box<CreateArgs>),
    /// Show one instance.
    Get { id: String },
    /// Destroy an instance and its disk.
    Delete { id: String },
    /// Stop an instance (bills disk only, never wakes on request).
    Stop { id: String },
    /// Start a stopped or sleeping instance.
    Start { id: String },
    /// Put an instance to sleep now.
    Sleep { id: String },
    /// Restart an instance.
    Restart { id: String },
    /// Change cpu, memory or disk (disk may only grow).
    Resize {
        id: String,
        /// vCPUs.
        #[arg(long)]
        cpu: Option<u32>,
        /// Memory in GB.
        #[arg(long)]
        memory: Option<u32>,
        /// Disk in GB.
        #[arg(long)]
        disk: Option<u32>,
    },
    /// Move an instance to another template or revision, keeping its disk.
    Update {
        id: String,
        /// Template `name` or `name@revision`; omitted means the latest revision of the current template.
        #[arg(long)]
        template: Option<String>,
    },
    /// Change the network policy, the allowed domains or the secrets of an instance.
    Network {
        id: String,
        /// New policy: allow-all, deny-all or allowlist. A running instance is recreated.
        #[arg(long, value_name = "allow-all|deny-all|allowlist", value_parser = parse_network)]
        mode: Option<NetworkMode>,
        /// Replace the allowlist with these hosts (repeatable); applies live.
        #[arg(long = "allow-domain", value_name = "DOMAIN")]
        allow_domains: Vec<String>,
        /// Replace the allowlist with an empty one.
        #[arg(long, conflicts_with = "allow_domains")]
        clear_domains: bool,
        /// Replace the secrets (repeatable; same syntax as `create --secret`).
        /// Values of secrets that keep their NAME apply live; new names at the next start.
        #[arg(long = "secret", value_name = "NAME=VALUE@UPSTREAM#HEADER#ENV")]
        secrets: Vec<String>,
        /// Remove every secret.
        #[arg(long, conflicts_with = "secrets")]
        clear_secrets: bool,
    },
    /// Copy an instance (disk, template and env) into a new one.
    Fork {
        id: String,
        /// Name of the copy.
        #[arg(long)]
        name: Option<String>,
    },
    /// Run a shell command inside an instance; exits with the command's exit code.
    Exec {
        id: String,
        /// Run as this user instead of the image default (e.g. root).
        #[arg(long)]
        user: Option<String>,
        /// Working directory inside the instance.
        #[arg(long)]
        workdir: Option<String>,
        /// Kill the command after this many seconds.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u32>,
        /// The command. A single argument is passed to the shell verbatim; several are quoted and joined.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Container or VM console logs.
    Logs {
        id: String,
        /// Only the last N lines.
        #[arg(long, value_name = "N")]
        tail: Option<u32>,
    },
    /// Per-minute cpu, memory and disk samples.
    Metrics {
        id: String,
        /// Look back this many hours (default 1).
        #[arg(long, value_name = "N")]
        hours: Option<u32>,
    },
    /// Mint a signed URL for an instance port.
    Url {
        id: String,
        /// Port (default: the template's default port).
        #[arg(long)]
        port: Option<u16>,
        /// Lifetime in seconds, 60..86400 (default 3600).
        #[arg(long, value_name = "SECS")]
        ttl: Option<u32>,
    },
    /// Print a signed URL and open it in the browser when xdg-open is available.
    Open {
        id: String,
        /// Port (default: the template's default port).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Disk backups of an instance.
    Backups {
        #[command(subcommand)]
        cmd: BackupsCmd,
    },
    /// Scheduled prompts for an agent instance.
    Crons {
        #[command(subcommand)]
        cmd: CronsCmd,
    },
}

/// Arguments of `warren instances create`.
#[derive(Debug, Args, Default)]
pub struct CreateArgs {
    /// Template `name` or `name@revision` (default warren-base).
    #[arg(long)]
    pub template: Option<String>,
    /// Instance type: default, vm or mac (macOS VM; needs an Apple silicon host).
    #[arg(long = "type", value_name = "default|vm|mac", value_parser = parse_instance_type)]
    pub kind: Option<InstanceType>,
    /// vCPUs.
    #[arg(long)]
    pub cpu: Option<u32>,
    /// Memory in GB.
    #[arg(long)]
    pub memory: Option<u32>,
    /// Disk in GB.
    #[arg(long)]
    pub disk: Option<u32>,
    /// Display name.
    #[arg(long)]
    pub name: Option<String>,
    /// Environment variable for the instance (repeatable).
    #[arg(long = "env", value_name = "K=V")]
    pub env: Vec<String>,
    /// Metadata entry (repeatable).
    #[arg(long = "metadata", value_name = "K=V")]
    pub metadata: Vec<String>,
    /// Sleep automatically after the idle timeout.
    #[arg(long)]
    pub auto_sleep: bool,
    /// Idle timeout in seconds (300..86400, default 900).
    #[arg(long, value_name = "SECS")]
    pub idle: Option<u32>,
    /// Outbound network policy: allow-all (default), deny-all (no route out) or
    /// allowlist (HTTP and HTTPS to the --allow-domain hosts only, through the
    /// host-side broker).
    #[arg(long, value_name = "allow-all|deny-all|allowlist", value_parser = parse_network)]
    pub network: Option<NetworkMode>,
    /// Host an allowlist instance may reach: a name (api.anthropic.com) or a
    /// wildcard (*.npmjs.org). Repeatable, at most 64.
    #[arg(long = "allow-domain", value_name = "DOMAIN")]
    pub allow_domains: Vec<String>,
    /// Credential the broker injects for the instance; the value never enters
    /// it. Repeatable, at most 16. Syntax NAME=VALUE@UPSTREAM#HEADER#ENV: NAME is
    /// the variable the instance sees (with a placeholder value), VALUE the
    /// credential (no `#`), UPSTREAM the https:// base URL, HEADER the header to
    /// add (x-api-key; Authorization gets "Bearer VALUE"), ENV the variable that
    /// receives the broker route URL. Example:
    /// 'ANTHROPIC_API_KEY=sk-ant-...@https://api.anthropic.com#x-api-key#ANTHROPIC_BASE_URL'
    #[arg(long = "secret", value_name = "NAME=VALUE@UPSTREAM#HEADER#ENV")]
    pub secrets: Vec<String>,
    /// Block until the instance is running.
    #[arg(long)]
    pub wait: bool,
}

/// `warren instances backups ...`
#[derive(Debug, Subcommand)]
pub enum BackupsCmd {
    /// List backups of an instance.
    List { id: String },
    /// Take a backup without stopping the instance.
    Create {
        id: String,
        /// Free-text note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Restore a backup into the instance.
    Restore { id: String, backup_id: String },
}

/// `warren instances crons ...`
#[derive(Debug, Subcommand)]
pub enum CronsCmd {
    /// List the crons of an instance.
    List { id: String },
    /// Schedule a prompt.
    Create {
        id: String,
        /// Prompt posted to the agent at each firing.
        #[arg(long)]
        prompt: String,
        /// Five-field cron expression, e.g. "0 9 * * 1-5".
        #[arg(long)]
        schedule: String,
        /// IANA time zone (default UTC).
        #[arg(long, value_name = "ZONE")]
        tz: Option<String>,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete a cron.
    Delete { id: String, cron_id: String },
}

/// `warren keys ...`
#[derive(Debug, Subcommand)]
pub enum KeysCmd {
    /// List API keys (prefixes only).
    List,
    /// Mint a key; the plaintext is shown once.
    Create {
        /// Key name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete a key by id.
    Delete { id: String },
}

/// Arguments of `warren chat`.
#[derive(Debug, Args)]
pub struct ChatArgs {
    /// Instance id.
    pub id: String,
    /// Continue an existing session.
    #[arg(long, value_name = "SID")]
    pub session: Option<String>,
    /// Wait for the whole answer instead of streaming deltas.
    #[arg(long)]
    pub no_stream: bool,
    /// The message; words are joined with spaces.
    #[arg(required = true, num_args = 1..)]
    pub message: Vec<String>,
}

/// `warren files ...`
#[derive(Debug, Subcommand)]
pub enum FilesCmd {
    /// List a directory (default: the home root).
    Ls { id: String, path: Option<String> },
    /// Print a file to stdout.
    Cat { id: String, path: String },
    /// Upload a local file.
    Put {
        id: String,
        local: PathBuf,
        remote: String,
    },
    /// Delete a file or directory.
    Rm { id: String, path: String },
    /// Download a directory as a gzip tarball (LOCAL `-` writes to stdout).
    Download {
        id: String,
        path: String,
        local: PathBuf,
    },
}

/// clap value parser for `--type`.
pub fn parse_instance_type(s: &str) -> Result<InstanceType, String> {
    InstanceType::parse(s.trim())
        .ok_or_else(|| format!("unknown instance type {s:?}; use default, vm or mac"))
}

/// clap value parser for `--network`.
pub fn parse_network(s: &str) -> Result<NetworkMode, String> {
    NetworkMode::parse(s.trim())
        .ok_or_else(|| format!("unknown network mode {s:?}; use allow-all, deny-all or allowlist"))
}

/// Verify the clap definitions are consistent (duplicate ids, bad settings).
/// Exposed so a test can run it; `Command::debug_assert` panics on problems.
pub fn debug_assert_cli() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("warren").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    fn fails(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(std::iter::once("warren").chain(args.iter().copied()))
            .err()
            .unwrap_or_else(|| panic!("{args:?} should fail"))
    }

    #[test]
    fn definitions_are_consistent() {
        debug_assert_cli();
    }

    #[test]
    fn global_flags_before_and_after_the_subcommand() {
        let c = parse(&[
            "--api-url",
            "http://x:1",
            "--api-key",
            "sk_live_a",
            "--json",
            "whoami",
        ]);
        assert_eq!(c.api_url.as_deref(), Some("http://x:1"));
        assert_eq!(c.api_key.as_deref(), Some("sk_live_a"));
        assert!(c.json);
        assert!(matches!(c.command, Command::Whoami));
        let c = parse(&["login", "--api-url", "http://y:2", "--api-key", "sk_live_b"]);
        assert_eq!(c.api_url.as_deref(), Some("http://y:2"));
        assert_eq!(c.api_key.as_deref(), Some("sk_live_b"));
        assert!(matches!(c.command, Command::Login));
        let c = parse(&["instances", "list", "--json", "--config", "/tmp/c.json"]);
        assert!(c.json);
        assert_eq!(
            c.config.as_deref(),
            Some(std::path::Path::new("/tmp/c.json"))
        );
        assert!(matches!(
            c.command,
            Command::Instances {
                cmd: InstancesCmd::List
            }
        ));
    }

    #[test]
    fn signup_and_health() {
        assert!(
            matches!(parse(&["signup", "--name", "acme"]).command, Command::Signup { name: Some(n) } if n == "acme")
        );
        assert!(matches!(
            parse(&["signup"]).command,
            Command::Signup { name: None }
        ));
        assert!(matches!(parse(&["health"]).command, Command::Health));
    }

    #[test]
    fn templates() {
        let c = parse(&[
            "templates",
            "register",
            "--name",
            "my-agent",
            "--image",
            "ghcr.io/x/y:1",
            "--port",
            "8080",
            "--mount",
            "/data",
            "--agent",
            "--description",
            "d",
            "--env",
            "A=1",
            "--env",
            "B=2",
        ]);
        let Command::Templates {
            cmd: TemplatesCmd::Register(r),
        } = c.command
        else {
            panic!("{:?}", c.command)
        };
        assert_eq!(r.name, "my-agent");
        assert_eq!(r.image, "ghcr.io/x/y:1");
        assert_eq!(r.port, Some(8080));
        assert_eq!(r.mount.as_deref(), Some("/data"));
        assert!(r.agent);
        assert_eq!(r.description.as_deref(), Some("d"));
        assert_eq!(r.env, vec!["A=1", "B=2"]);
        assert!(
            matches!(parse(&["templates", "get", "warren-agent@2"]).command, Command::Templates { cmd: TemplatesCmd::Get { name } } if name == "warren-agent@2")
        );
        assert!(
            matches!(parse(&["templates", "delete", "x"]).command, Command::Templates { cmd: TemplatesCmd::Delete { name } } if name == "x")
        );
        assert!(matches!(
            parse(&["templates", "list"]).command,
            Command::Templates {
                cmd: TemplatesCmd::List
            }
        ));
        assert_eq!(
            fails(&["templates", "register", "--name", "x"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn instances_create() {
        let c = parse(&[
            "instances",
            "create",
            "--template",
            "warren-agent",
            "--type",
            "vm",
            "--cpu",
            "4",
            "--memory",
            "8",
            "--disk",
            "20",
            "--name",
            "prod",
            "--env",
            "APP_ENV=production",
            "--env",
            "DEBUG=1",
            "--metadata",
            "plan=pro",
            "--auto-sleep",
            "--idle",
            "600",
            "--network",
            "allowlist",
            "--allow-domain",
            "api.anthropic.com",
            "--allow-domain",
            "*.npmjs.org",
            "--secret",
            "ANTHROPIC_API_KEY=sk-ant-x@https://api.anthropic.com#x-api-key#ANTHROPIC_BASE_URL",
            "--wait",
        ]);
        let Command::Instances {
            cmd: InstancesCmd::Create(a),
        } = c.command
        else {
            panic!("{:?}", c.command)
        };
        assert_eq!(a.template.as_deref(), Some("warren-agent"));
        assert_eq!(a.kind, Some(InstanceType::Vm));
        assert_eq!((a.cpu, a.memory, a.disk), (Some(4), Some(8), Some(20)));
        assert_eq!(a.name.as_deref(), Some("prod"));
        assert_eq!(a.env, vec!["APP_ENV=production", "DEBUG=1"]);
        assert_eq!(a.metadata, vec!["plan=pro"]);
        assert!(a.auto_sleep);
        assert_eq!(a.idle, Some(600));
        assert_eq!(a.network, Some(NetworkMode::Allowlist));
        assert_eq!(a.allow_domains, vec!["api.anthropic.com", "*.npmjs.org"]);
        assert_eq!(a.secrets.len(), 1);
        assert!(a.wait);
        let Command::Instances {
            cmd: InstancesCmd::Create(a),
        } = parse(&["instances", "create"]).command
        else {
            panic!()
        };
        assert!(a.template.is_none() && a.kind.is_none() && !a.wait && a.env.is_empty());
        let Command::Instances {
            cmd: InstancesCmd::Create(a),
        } = parse(&["instances", "create", "--type", "mac"]).command
        else {
            panic!()
        };
        assert_eq!(a.kind, Some(InstanceType::Mac));
        let err = fails(&["instances", "create", "--type", "huge"]);
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        assert!(err.to_string().contains("use default, vm or mac"), "{err}");
        assert_eq!(
            fails(&["instances", "create", "--network", "some"]).kind(),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            fails(&["instances", "create", "--cpu", "two"]).kind(),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn instances_network() {
        let Command::Instances {
            cmd:
                InstancesCmd::Network {
                    id,
                    mode,
                    allow_domains,
                    clear_domains,
                    secrets,
                    clear_secrets,
                },
        } = parse(&[
            "instances",
            "network",
            "ab12cd34ef",
            "--mode",
            "allowlist",
            "--allow-domain",
            "api.anthropic.com",
            "--clear-secrets",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(id, "ab12cd34ef");
        assert_eq!(mode, Some(NetworkMode::Allowlist));
        assert_eq!(allow_domains, vec!["api.anthropic.com"]);
        assert!(!clear_domains && secrets.is_empty() && clear_secrets);
        assert_eq!(
            fails(&[
                "instances",
                "network",
                "ab12cd34ef",
                "--allow-domain",
                "a.example.com",
                "--clear-domains"
            ])
            .kind(),
            ErrorKind::ArgumentConflict
        );
        assert_eq!(
            fails(&["instances", "network", "ab12cd34ef", "--mode", "open"]).kind(),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn instance_lifecycle_commands() {
        for (verb, want) in [
            ("get", "get"),
            ("delete", "delete"),
            ("stop", "stop"),
            ("start", "start"),
            ("sleep", "sleep"),
            ("restart", "restart"),
        ] {
            let c = parse(&["instances", verb, "ab12cd34ef"]);
            let Command::Instances { cmd } = c.command else {
                panic!()
            };
            let got = match cmd {
                InstancesCmd::Get { id } => ("get", id),
                InstancesCmd::Delete { id } => ("delete", id),
                InstancesCmd::Stop { id } => ("stop", id),
                InstancesCmd::Start { id } => ("start", id),
                InstancesCmd::Sleep { id } => ("sleep", id),
                InstancesCmd::Restart { id } => ("restart", id),
                other => panic!("{other:?}"),
            };
            assert_eq!(got, (want, "ab12cd34ef".to_string()));
        }
        let Command::Instances {
            cmd:
                InstancesCmd::Resize {
                    id,
                    cpu,
                    memory,
                    disk,
                },
        } = parse(&[
            "instances",
            "resize",
            "i",
            "--cpu",
            "4",
            "--memory",
            "16",
            "--disk",
            "30",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(
            (id.as_str(), cpu, memory, disk),
            ("i", Some(4), Some(16), Some(30))
        );
        let Command::Instances {
            cmd: InstancesCmd::Update { id, template },
        } = parse(&["instances", "update", "i", "--template", "t@3"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), template.as_deref()), ("i", Some("t@3")));
        let Command::Instances {
            cmd: InstancesCmd::Fork { id, name },
        } = parse(&["instances", "fork", "i", "--name", "copy"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), name.as_deref()), ("i", Some("copy")));
        let Command::Instances {
            cmd: InstancesCmd::Logs { id, tail },
        } = parse(&["instances", "logs", "i", "--tail", "50"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), tail), ("i", Some(50)));
        let Command::Instances {
            cmd: InstancesCmd::Metrics { id, hours },
        } = parse(&["instances", "metrics", "i", "--hours", "6"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), hours), ("i", Some(6)));
        let Command::Instances {
            cmd: InstancesCmd::Url { id, port, ttl },
        } = parse(&["instances", "url", "i", "--port", "7681", "--ttl", "600"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), port, ttl), ("i", Some(7681), Some(600)));
        let Command::Instances {
            cmd: InstancesCmd::Open { id, port },
        } = parse(&["instances", "open", "i"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), port), ("i", None));
        assert_eq!(
            fails(&["instances", "get"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn exec_takes_the_rest_of_the_line() {
        let Command::Instances {
            cmd:
                InstancesCmd::Exec {
                    id,
                    user,
                    workdir,
                    timeout,
                    command,
                },
        } = parse(&["instances", "exec", "i", "--", "ls", "-la", "/tmp"]).command
        else {
            panic!()
        };
        assert_eq!(id, "i");
        assert!(user.is_none() && workdir.is_none() && timeout.is_none());
        assert_eq!(command, vec!["ls", "-la", "/tmp"]);
        let Command::Instances {
            cmd: InstancesCmd::Exec { command, .. },
        } = parse(&["instances", "exec", "i", "ls", "-la"]).command
        else {
            panic!()
        };
        assert_eq!(command, vec!["ls", "-la"]);
        let Command::Instances {
            cmd:
                InstancesCmd::Exec {
                    user,
                    workdir,
                    timeout,
                    command,
                    ..
                },
        } = parse(&[
            "instances",
            "exec",
            "i",
            "--user",
            "root",
            "--workdir",
            "/",
            "--timeout",
            "30",
            "--",
            "id",
            "-u",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(
            (user.as_deref(), workdir.as_deref(), timeout),
            (Some("root"), Some("/"), Some(30))
        );
        assert_eq!(command, vec!["id", "-u"]);
        let Command::Instances {
            cmd: InstancesCmd::Exec { command, .. },
        } = parse(&["instances", "exec", "i", "--", "echo hi | wc -c"]).command
        else {
            panic!()
        };
        assert_eq!(command, vec!["echo hi | wc -c"]);
        assert_eq!(
            fails(&["instances", "exec", "i"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn backups_and_crons() {
        let Command::Instances {
            cmd:
                InstancesCmd::Backups {
                    cmd: BackupsCmd::Create { id, note },
                },
        } = parse(&[
            "instances",
            "backups",
            "create",
            "i",
            "--note",
            "before upgrade",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(
            (id.as_str(), note.as_deref()),
            ("i", Some("before upgrade"))
        );
        let Command::Instances {
            cmd:
                InstancesCmd::Backups {
                    cmd: BackupsCmd::Restore { id, backup_id },
                },
        } = parse(&["instances", "backups", "restore", "i", "b1"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), backup_id.as_str()), ("i", "b1"));
        assert!(
            matches!(parse(&["instances", "backups", "list", "i"]).command, Command::Instances { cmd: InstancesCmd::Backups { cmd: BackupsCmd::List { id } } } if id == "i")
        );
        let Command::Instances {
            cmd:
                InstancesCmd::Crons {
                    cmd:
                        CronsCmd::Create {
                            id,
                            prompt,
                            schedule,
                            tz,
                            name,
                        },
                },
        } = parse(&[
            "instances",
            "crons",
            "create",
            "i",
            "--prompt",
            "Summarize",
            "--schedule",
            "0 9 * * 1-5",
            "--tz",
            "Europe/Berlin",
            "--name",
            "daily",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(
            (id.as_str(), prompt.as_str(), schedule.as_str()),
            ("i", "Summarize", "0 9 * * 1-5")
        );
        assert_eq!(
            (tz.as_deref(), name.as_deref()),
            (Some("Europe/Berlin"), Some("daily"))
        );
        let Command::Instances {
            cmd:
                InstancesCmd::Crons {
                    cmd: CronsCmd::Delete { id, cron_id },
                },
        } = parse(&["instances", "crons", "delete", "i", "c1"]).command
        else {
            panic!()
        };
        assert_eq!((id.as_str(), cron_id.as_str()), ("i", "c1"));
        assert_eq!(
            fails(&["instances", "crons", "create", "i", "--prompt", "p"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn keys_chat_files() {
        assert!(matches!(
            parse(&["keys", "list"]).command,
            Command::Keys { cmd: KeysCmd::List }
        ));
        assert!(
            matches!(parse(&["keys", "create", "--name", "ci"]).command, Command::Keys { cmd: KeysCmd::Create { name: Some(n) } } if n == "ci")
        );
        assert!(
            matches!(parse(&["keys", "delete", "k1"]).command, Command::Keys { cmd: KeysCmd::Delete { id } } if id == "k1")
        );
        let Command::Chat(c) = parse(&[
            "chat",
            "i",
            "--session",
            "s1",
            "--no-stream",
            "hello",
            "there",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(
            (c.id.as_str(), c.session.as_deref(), c.no_stream),
            ("i", Some("s1"), true)
        );
        assert_eq!(c.message, vec!["hello", "there"]);
        assert_eq!(
            fails(&["chat", "i"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
        assert!(
            matches!(parse(&["files", "ls", "i"]).command, Command::Files { cmd: FilesCmd::Ls { id, path: None } } if id == "i")
        );
        assert!(
            matches!(parse(&["files", "ls", "i", "src"]).command, Command::Files { cmd: FilesCmd::Ls { path: Some(p), .. } } if p == "src")
        );
        assert!(
            matches!(parse(&["files", "cat", "i", "a.txt"]).command, Command::Files { cmd: FilesCmd::Cat { path, .. } } if path == "a.txt")
        );
        let Command::Files {
            cmd: FilesCmd::Put { id, local, remote },
        } = parse(&["files", "put", "i", "./x.bin", "data/x.bin"]).command
        else {
            panic!()
        };
        assert_eq!(
            (id.as_str(), local.to_str(), remote.as_str()),
            ("i", Some("./x.bin"), "data/x.bin")
        );
        assert!(
            matches!(parse(&["files", "rm", "i", "a"]).command, Command::Files { cmd: FilesCmd::Rm { path, .. } } if path == "a")
        );
        let Command::Files {
            cmd: FilesCmd::Download { path, local, .. },
        } = parse(&["files", "download", "i", "proj", "-"]).command
        else {
            panic!()
        };
        assert_eq!((path.as_str(), local.to_str()), ("proj", Some("-")));
    }

    #[test]
    fn shell_run_and_sync() {
        let Command::Shell(s) = parse(&["shell", "i"]).command else {
            panic!()
        };
        assert_eq!(s.id.as_deref(), Some("i"));
        assert!(s.command.is_empty() && s.cwd.is_none() && s.gateway_url.is_none());
        let Command::Shell(s) =
            parse(&["shell", "i", "--cwd", "work/app", "--", "ls", "-la"]).command
        else {
            panic!()
        };
        assert_eq!(s.cwd.as_deref(), Some("work/app"));
        assert_eq!(s.command, vec!["ls", "-la"]);
        let Command::Shell(s) = parse(&[
            "shell",
            "--gateway-url",
            "http://127.0.0.1:38737",
            "--gateway-token",
            "tok",
            "--",
            "echo hi; exit 4",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(s.id, None);
        assert_eq!(s.gateway_url.as_deref(), Some("http://127.0.0.1:38737"));
        assert_eq!(s.gateway_token.as_deref(), Some("tok"));
        assert_eq!(s.command, vec!["echo hi; exit 4"]);
        assert_eq!(fails(&["shell"]).kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(
            fails(&["shell", "--gateway-url", "http://x"]).kind(),
            ErrorKind::MissingRequiredArgument
        );

        let Command::Run(r) = parse(&["run", "--", "claude"]).command else {
            panic!()
        };
        assert_eq!(r.command, vec!["claude"]);
        assert!(!r.new && !r.no_sync && !r.no_tty && !r.unbind && !r.delete);
        assert!(r.kind.is_none() && r.template.is_none() && r.remote_root.is_none());
        let Command::Run(r) = parse(&[
            "run",
            "--new",
            "--type",
            "mac",
            "--template",
            "warren-macos",
            "--name",
            "ios",
            "--remote-root",
            "/Users/lume/work/app",
            "--no-sync",
            "--no-tty",
            "--",
            "xcodebuild",
            "-scheme",
            "App",
        ])
        .command
        else {
            panic!()
        };
        assert!(r.new && r.no_sync && r.no_tty);
        assert_eq!(r.kind, Some(InstanceType::Mac));
        assert_eq!(r.template.as_deref(), Some("warren-macos"));
        assert_eq!(r.name.as_deref(), Some("ios"));
        assert_eq!(r.remote_root.as_deref(), Some("/Users/lume/work/app"));
        assert_eq!(r.command, vec!["xcodebuild", "-scheme", "App"]);
        let Command::Run(r) = parse(&["run", "--unbind", "--delete"]).command else {
            panic!()
        };
        assert!(r.unbind && r.delete && r.command.is_empty());
        assert_eq!(fails(&["run"]).kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(
            fails(&["run", "--delete"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            fails(&["run", "--unbind", "--", "ls"]).kind(),
            ErrorKind::ArgumentConflict
        );

        let Command::Sync {
            cmd: SyncCmd::Push(a),
        } = parse(&[
            "sync",
            "push",
            "i",
            "--remote-root",
            "/x",
            "--exclude",
            "vendor",
            "--exclude",
            "tmp",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(a.id.as_deref(), Some("i"));
        assert_eq!(a.remote_root.as_deref(), Some("/x"));
        assert_eq!(a.exclude, vec!["vendor", "tmp"]);
        let Command::Sync {
            cmd: SyncCmd::Pull(a),
        } = parse(&["sync", "pull"]).command
        else {
            panic!()
        };
        assert!(a.id.is_none() && a.exclude.is_empty());
    }

    #[test]
    fn setup_commands() {
        let Command::Up(u) = parse(&["up"]).command else {
            panic!()
        };
        assert!(u.port.is_none() && u.data_dir.is_none() && !u.dev && !u.no_service);
        assert_eq!(u.runtime, RuntimeChoice::Auto);
        let Command::Up(u) = parse(&[
            "up",
            "--port",
            "7357",
            "--data-dir",
            "/tmp/w",
            "--base-domain",
            "lvh.me",
            "--mac-base-vm",
            "warren-macos-26",
            "--runtime",
            "none",
            "--dev",
            "--no-service",
            "--warrend-bin",
            "/x/warrend",
            "--listen",
            ":7357",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(u.port, Some(7357));
        assert_eq!(u.data_dir.as_deref(), Some(std::path::Path::new("/tmp/w")));
        assert_eq!(u.base_domain.as_deref(), Some("lvh.me"));
        assert_eq!(u.mac_base_vm.as_deref(), Some("warren-macos-26"));
        assert_eq!(u.runtime, RuntimeChoice::None);
        assert!(u.dev && u.no_service);
        assert_eq!(
            u.warrend_bin.as_deref(),
            Some(std::path::Path::new("/x/warrend"))
        );
        assert_eq!(u.listen.as_deref(), Some(":7357"));
        assert_eq!(
            fails(&["up", "--runtime", "podman"]).kind(),
            ErrorKind::InvalidValue
        );
        let Command::Down(d) = parse(&["down", "--purge", "--yes"]).command else {
            panic!()
        };
        assert!(d.purge && d.yes);
        let Command::Down(d) = parse(&["down"]).command else {
            panic!()
        };
        assert!(!d.purge && !d.yes);
        assert!(matches!(parse(&["status"]).command, Command::Status));
        assert!(matches!(parse(&["doctor"]).command, Command::Doctor));
    }

    #[test]
    fn mac_connect_and_skill() {
        let Command::Mac {
            cmd: MacCmd::Init(m),
        } = parse(&["mac", "init"]).command
        else {
            panic!()
        };
        assert!(m.source.is_none() && !m.with_agents && !m.force && !m.dry_run);
        assert_eq!(m.ipsw, "latest");
        assert_eq!(m.stock, "base");
        assert_eq!(m.unattended, "tahoe");
        assert_eq!(m.target, "warren-macos-26");
        let Command::Mac {
            cmd: MacCmd::Init(m),
        } = parse(&[
            "mac",
            "init",
            "--source",
            "sandbox",
            "--ipsw",
            "/tmp/x.ipsw",
            "--target",
            "t",
            "--with-agents",
            "--force",
            "--dry-run",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(m.source.as_deref(), Some("sandbox"));
        assert_eq!(m.ipsw, "/tmp/x.ipsw");
        assert_eq!(m.target, "t");
        assert!(m.with_agents && m.force && m.dry_run);
        assert_eq!(
            fails(&["mac"]).kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );

        let Command::Connect(c) = parse(&["connect", "claude"]).command else {
            panic!()
        };
        assert_eq!(c.agent, ConnectTarget::Claude);
        assert!(!c.project && !c.print);
        for (name, want) in [
            ("codex", ConnectTarget::Codex),
            ("cursor", ConnectTarget::Cursor),
            ("windsurf", ConnectTarget::Windsurf),
            ("caret", ConnectTarget::Caret),
            ("opencode", ConnectTarget::Opencode),
            ("gemini", ConnectTarget::Gemini),
        ] {
            let Command::Connect(c) = parse(&["connect", name, "--project", "--print"]).command
            else {
                panic!()
            };
            assert_eq!(c.agent, want);
            assert!(c.project && c.print);
        }
        assert_eq!(fails(&["connect", "emacs"]).kind(), ErrorKind::InvalidValue);
        assert_eq!(
            fails(&["connect"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
        assert!(matches!(
            parse(&["skill", "install"]).command,
            Command::Skill {
                cmd: SkillCmd::Install { project: false }
            }
        ));
        assert!(matches!(
            parse(&["skill", "install", "--project"]).command,
            Command::Skill {
                cmd: SkillCmd::Install { project: true }
            }
        ));
    }

    #[test]
    fn usage_errors_exit_2_and_help_exits_0() {
        let e = fails(&["instances", "bogus"]);
        assert_eq!(e.exit_code(), 2);
        assert_eq!(fails(&["--nope"]).exit_code(), 2);
        assert_eq!(fails(&[]).exit_code(), 2);
        let h = fails(&["--help"]);
        assert_eq!(h.kind(), ErrorKind::DisplayHelp);
        assert_eq!(h.exit_code(), 0);
        assert_eq!(fails(&["--version"]).kind(), ErrorKind::DisplayVersion);
        assert_eq!(fails(&["instances", "--help"]).exit_code(), 0);
    }
}
