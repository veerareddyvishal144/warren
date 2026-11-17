//! Command execution: resolves settings, builds the SDK client and dispatches.
//!
//! Only `api.rs` touches `warren-sdk`; the other modules work on the JSON values
//! it returns so output is identical for `--json` and the human tables.

mod account;
mod agent;
mod api;
mod instances;
mod templates;

use std::path::PathBuf;

use serde_json::Value;
use warren_cli::cli::{Cli, Command};
use warren_cli::config::{self, FileConfig, Settings};
use warren_cli::output::to_json_pretty;
use warren_cli::{EXIT_OK, Failure};

use self::api::Api;

/// Everything a command needs besides its own arguments.
pub struct Ctx {
    /// Merged flag/env/file settings.
    pub settings: Settings,
    /// Where `login` and `signup` write; `None` when no home directory is known.
    pub config_path: Option<PathBuf>,
    /// `--json`: print raw responses.
    pub json: bool,
}

impl Ctx {
    /// Print the raw value with `--json`, otherwise the human rendering (which
    /// must end with a newline). Returns the success exit code for chaining.
    pub fn emit(&self, raw: &Value, human: impl FnOnce() -> String) -> Result<i32, Failure> {
        if self.json {
            println!("{}", to_json_pretty(raw));
        } else {
            print!("{}", human());
        }
        Ok(EXIT_OK)
    }

    /// A client for authenticated calls; a usage error when no key is configured.
    pub fn api(&self) -> Result<Api, Failure> {
        if self.settings.api_key.is_none() {
            return Err(Failure::usage(
                "no API key configured: run 'warren login --api-url URL --api-key KEY', 'warren signup', or set WARREN_API_KEY",
            ));
        }
        Api::new(&self.settings)
    }

    /// A client for the unauthenticated endpoints (health, signup).
    pub fn api_anonymous(&self) -> Result<Api, Failure> {
        Api::new(&self.settings)
    }
}

/// Run one parsed invocation and return the process exit code.
pub async fn run(cli: Cli) -> Result<i32, Failure> {
    let Cli {
        api_url,
        api_key,
        config: config_flag,
        json,
        command,
    } = cli;
    let config_path = config_flag.or_else(FileConfig::default_path);
    let file = match &config_path {
        Some(p) => FileConfig::load(p)?,
        None => FileConfig::default(),
    };
    let settings = config::resolve(api_url.as_deref(), api_key.as_deref(), &file);
    let ctx = Ctx {
        settings,
        config_path,
        json,
    };
    match command {
        Command::Signup { name } => account::signup(&ctx, name.as_deref()).await,
        Command::Login => account::login(&ctx, api_key.as_deref()),
        Command::Whoami => account::whoami(&ctx).await,
        Command::Health => account::health(&ctx).await,
        Command::Keys { cmd } => account::keys(&ctx, cmd).await,
        Command::Templates { cmd } => templates::run(&ctx, cmd).await,
        Command::Instances { cmd } => instances::run(&ctx, cmd).await,
        Command::Chat(args) => agent::chat(&ctx, args).await,
        Command::Files { cmd } => agent::files(&ctx, cmd).await,
    }
}

/// `id  status` for the lifecycle verbs that answer `{ "id", "status" }`.
pub fn status_line(v: &Value) -> String {
    let id = v.get("id").and_then(Value::as_str).unwrap_or("-");
    let status = v.get("status").and_then(Value::as_str).unwrap_or("-");
    let mut line = format!("{id}  {status}");
    if let Some(r) = v
        .get("status_reason")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
    {
        line.push_str(&format!("  ({r})"));
    }
    if let Some(res) = v.get("resources").filter(|r| r.is_object()) {
        line.push_str(&format!("  {}", shape_text(res)));
    }
    if let Some(img) = v.get("image_ref").and_then(Value::as_str) {
        line.push_str(&format!("  {img}"));
    }
    line.push('\n');
    line
}

fn shape_text(v: &Value) -> String {
    format!(
        "{}v/{}g/{}d",
        v.get("cpu").and_then(Value::as_u64).unwrap_or(0),
        v.get("memory").and_then(Value::as_u64).unwrap_or(0),
        v.get("disk").and_then(Value::as_u64).unwrap_or(0)
    )
}

/// A usage failure from an input-validation error (exit 2 instead of 1).
pub fn usage(e: warren_core::Error) -> Failure {
    Failure::usage(e.message)
}
