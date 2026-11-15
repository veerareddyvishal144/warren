//! warren-gateway runs inside every instance: Agent API, files API, exec, health,
//! terminal supervision, and PID 1 duties inside microVMs.
//!
//! Usage: `warren-gateway [--serve | --init | --help | --version]`, or
//! `warren-gateway [--serve] --wrap -- <command> [args...]` to serve while
//! supervising an image's own entrypoint; the default is `--serve`.
//! Configuration comes from `WARREN_*` variables (see docs/API.md).

use tracing_subscriber::EnvFilter;
use warren_gateway::{GatewayConfig, WrapSpec, init, server};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Serve; `wrap` is `Some(argv)` in wrap mode (`argv` may be empty).
    Serve {
        wrap: Option<Vec<String>>,
    },
    Init,
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Mode, String> {
    parse_args_for_pid(args, std::process::id())
}

/// Parse the command line. `pid` decides the default without an explicit
/// flag: PID 1 means we were started as a VM's init.
fn parse_args_for_pid(args: &[String], pid: u32) -> Result<Mode, String> {
    let mut explicit: Option<Mode> = None;
    let mut wrap: Option<Vec<String>> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--serve" | "serve" => explicit = Some(Mode::Serve { wrap: None }),
            "--init" | "init" => explicit = Some(Mode::Init),
            "--wrap" | "wrap" => {
                wrap.get_or_insert_with(Vec::new);
            }
            "--" => {
                let Some(argv) = wrap.as_mut() else {
                    return Err("a command after `--` needs --wrap".to_string());
                };
                argv.extend(it.by_ref().cloned());
            }
            "-h" | "--help" | "help" => return Ok(Mode::Help),
            "-V" | "--version" | "version" => return Ok(Mode::Version),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let mode = explicit.unwrap_or(if wrap.is_none() && pid == 1 {
        Mode::Init
    } else {
        Mode::Serve { wrap: None }
    });
    match (mode, wrap) {
        (Mode::Init, Some(_)) => Err("--init cannot be combined with --wrap".to_string()),
        (Mode::Init, None) => Ok(Mode::Init),
        (_, wrap) => Ok(Mode::Serve { wrap }),
    }
}

const USAGE: &str = "warren-gateway: the in-instance server of Warren

USAGE:
    warren-gateway [--serve]   serve the Agent API, files, exec and health on WARREN_GATEWAY_PORT (default)
    warren-gateway [--serve] --wrap -- <command> [args...]
                               wrap mode: serve, then run <command> (the image's own entrypoint and
                               command) as a supervised child in the current directory. The child
                               inherits the environment without the WARREN_* variables (WARREN_HOME
                               stays), its stdout/stderr are the container's, it is not restarted
                               when it exits (health reports it under \"wrapped\"), and on SIGTERM or
                               SIGINT it receives SIGTERM, then SIGKILL after 10 s, before the gateway
                               exits with its exit code. Without a command, wrap mode just serves.
    warren-gateway --init      run as PID 1 inside a microVM, then serve
    warren-gateway --version
    warren-gateway --help

ENVIRONMENT:
    WARREN_HOME, WARREN_GATEWAY_PORT, WARREN_GATEWAY_BIND, WARREN_INSTANCE_ID, WARREN_INSTANCE_TOKEN,
    WARREN_AGENT_DRIVER (echo|openai|command|none; default none), WARREN_AGENT_COMMAND, WARREN_MODEL,
    OPENAI_BASE_URL, OPENAI_API_KEY, WARREN_TERMINAL (off to disable ttyd), RUST_LOG
";

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,reqwest=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_args(&args) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("warren-gateway: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    match mode {
        Mode::Help => print!("{USAGE}"),
        Mode::Version => println!("warren-gateway {}", warren_gateway::VERSION),
        Mode::Init => {
            init_tracing();
            if let Err(e) = init::run_init() {
                tracing::error!(error = %e, details = ?e.details, "init failed");
                std::process::exit(1);
            }
        }
        Mode::Serve { wrap } => {
            init_tracing();
            let mut config = match GatewayConfig::from_env() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, details = ?e.details, "invalid configuration");
                    std::process::exit(2);
                }
            };
            if let Some(argv) = wrap {
                // The wrapped program runs where the container started us; an
                // unreadable current directory is simply inherited.
                config.wrap = Some(WrapSpec {
                    argv,
                    cwd: std::env::current_dir().ok(),
                });
            }
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "cannot start the async runtime");
                    std::process::exit(1);
                }
            };
            match runtime.block_on(server::serve(config)) {
                Ok(code) => {
                    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
                    if code != 0 {
                        std::process::exit(code);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, details = ?e.details, "gateway exited with an error");
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Mode, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args_for_pid(&args, 4242)
    }

    fn argv(words: &[&str]) -> Option<Vec<String>> {
        Some(words.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn classic_modes() {
        assert_eq!(parse(&[]).unwrap(), Mode::Serve { wrap: None });
        assert_eq!(parse(&["--serve"]).unwrap(), Mode::Serve { wrap: None });
        assert_eq!(parse(&["serve"]).unwrap(), Mode::Serve { wrap: None });
        assert_eq!(parse(&["--init"]).unwrap(), Mode::Init);
        assert_eq!(parse(&["--help"]).unwrap(), Mode::Help);
        assert_eq!(parse(&["-V"]).unwrap(), Mode::Version);
        assert!(parse(&["--bogus"]).unwrap_err().contains("--bogus"));
        assert!(parse(&["sh"]).is_err());
        // PID 1 without a flag is the VM's init; an explicit flag wins.
        let none: Vec<String> = Vec::new();
        assert_eq!(parse_args_for_pid(&none, 1).unwrap(), Mode::Init);
        assert_eq!(
            parse_args_for_pid(&["--serve".to_string()], 1).unwrap(),
            Mode::Serve { wrap: None }
        );
        assert_eq!(
            parse_args_for_pid(
                &["--wrap".to_string(), "--".to_string(), "sh".to_string()],
                1
            )
            .unwrap(),
            Mode::Serve {
                wrap: argv(&["sh"])
            }
        );
    }

    #[test]
    fn wrap_mode() {
        assert_eq!(
            parse(&[
                "--serve",
                "--wrap",
                "--",
                "sh",
                "-c",
                "echo started; sleep 30"
            ])
            .unwrap(),
            Mode::Serve {
                wrap: argv(&["sh", "-c", "echo started; sleep 30"])
            }
        );
        // --wrap alone implies --serve; everything after -- is the command,
        // including words that look like our own flags.
        assert_eq!(
            parse(&["--wrap", "--", "/usr/bin/app", "--init", "--help", "--"]).unwrap(),
            Mode::Serve {
                wrap: argv(&["/usr/bin/app", "--init", "--help", "--"])
            }
        );
        assert_eq!(
            parse(&["--wrap"]).unwrap(),
            Mode::Serve {
                wrap: Some(Vec::new())
            }
        );
        assert_eq!(
            parse(&["--wrap", "--"]).unwrap(),
            Mode::Serve {
                wrap: Some(Vec::new())
            }
        );
        assert_eq!(
            parse(&["--wrap", "--serve", "--", "app"]).unwrap(),
            Mode::Serve {
                wrap: argv(&["app"])
            }
        );
        assert!(parse(&["--", "sh"]).unwrap_err().contains("--wrap"));
        assert!(parse(&["--init", "--wrap", "--", "sh"]).is_err());
        assert!(parse(&["--wrap", "--init"]).is_err());
        assert_eq!(parse(&["--wrap", "--help"]).unwrap(), Mode::Help);
    }

    #[test]
    fn usage_documents_wrap() {
        assert!(USAGE.contains("--wrap -- <command>"));
        assert!(USAGE.contains("WARREN_HOME"));
        assert!(USAGE.contains("SIGKILL after 10 s"));
    }
}
