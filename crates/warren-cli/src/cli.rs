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
    /// Instance type: default or vm.
    #[arg(long = "type", value_name = "default|vm", value_parser = parse_instance_type)]
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
    /// Outbound network policy.
    #[arg(long, value_name = "allow-all|deny-all", value_parser = parse_network)]
    pub network: Option<NetworkMode>,
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
        .ok_or_else(|| format!("unknown instance type {s:?}; use default or vm"))
}

/// clap value parser for `--network`.
pub fn parse_network(s: &str) -> Result<NetworkMode, String> {
    NetworkMode::parse(s.trim())
        .ok_or_else(|| format!("unknown network mode {s:?}; use allow-all or deny-all"))
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
            "deny-all",
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
        assert_eq!(a.network, Some(NetworkMode::DenyAll));
        assert!(a.wait);
        let Command::Instances {
            cmd: InstancesCmd::Create(a),
        } = parse(&["instances", "create"]).command
        else {
            panic!()
        };
        assert!(a.template.is_none() && a.kind.is_none() && !a.wait && a.env.is_empty());
        assert_eq!(
            fails(&["instances", "create", "--type", "huge"]).kind(),
            ErrorKind::ValueValidation
        );
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
