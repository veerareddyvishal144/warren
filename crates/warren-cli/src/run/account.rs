//! signup, login, whoami, health and keys.

use serde_json::Value;
use warren_cli::cli::KeysCmd;
use warren_cli::config::FileConfig;
use warren_cli::output::{Kind, WORKSPACE_KEYS, items_of, render_list, render_object};
use warren_cli::{EXIT_OK, Failure};

use super::Ctx;

/// `warren signup [--name]`: create a workspace, print the key, save it.
pub async fn signup(ctx: &Ctx, name: Option<&str>) -> Result<i32, Failure> {
    let api = ctx.api_anonymous()?;
    let v = api.signup(name).await?;
    let key = v
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let saved = save_key(ctx, &key)?;
    if ctx.json {
        println!("{}", warren_cli::output::to_json_pretty(&v));
    } else {
        let ws = &v["workspace"];
        println!(
            "workspace:  {}",
            ws.get("id").and_then(Value::as_str).unwrap_or("-")
        );
        println!(
            "name:       {}",
            ws.get("name").and_then(Value::as_str).unwrap_or("-")
        );
        println!("api_key:    {key}");
        println!("api_url:    {}", ctx.settings.api_url);
    }
    match saved {
        Some(path) => eprintln!("Saved the key to {path}. Keep it secret; it is shown only once."),
        None => {
            eprintln!("warning: HOME is unset, the key was not saved; export WARREN_API_KEY={key}")
        }
    }
    Ok(EXIT_OK)
}

/// `warren login --api-url URL --api-key KEY`: write the config file.
pub fn login(ctx: &Ctx, key_from_flag_or_env: Option<&str>) -> Result<i32, Failure> {
    let key = key_from_flag_or_env.map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| {
        Failure::usage("login needs --api-key KEY (or WARREN_API_KEY); pass --api-url URL to point at another daemon")
    })?;
    let path = save_key(ctx, key)?.ok_or_else(|| {
        Failure::local(
            "cannot save the config: neither XDG_CONFIG_HOME nor HOME is set; pass --config PATH",
        )
    })?;
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({ "api_url": ctx.settings.api_url, "config": path, "key_prefix": prefix(key) })
        );
    } else {
        println!(
            "Saved {} for {} to {path}",
            prefix(key),
            ctx.settings.api_url
        );
    }
    Ok(EXIT_OK)
}

/// Write `{api_url, api_key}`; returns the path, or `None` when there is nowhere to write.
fn save_key(ctx: &Ctx, key: &str) -> Result<Option<String>, Failure> {
    let Some(path) = &ctx.config_path else {
        return Ok(None);
    };
    if key.is_empty() {
        return Err(Failure::local("the server did not return an API key"));
    }
    FileConfig {
        api_url: Some(ctx.settings.api_url.clone()),
        api_key: Some(key.to_string()),
    }
    .save(path)
    .map_err(|e| Failure::local(e.message))?;
    Ok(Some(path.display().to_string()))
}

/// `sk_live_abcdef...` for messages.
fn prefix(key: &str) -> String {
    let shown: String = key.chars().take(14).collect();
    if key.chars().count() > 14 {
        format!("{shown}...")
    } else {
        shown
    }
}

/// `warren whoami`
pub async fn whoami(ctx: &Ctx) -> Result<i32, Failure> {
    let v = ctx.api()?.workspace().await?;
    ctx.emit(&v, || render_object(&v, WORKSPACE_KEYS))
}

/// `warren health`
pub async fn health(ctx: &Ctx) -> Result<i32, Failure> {
    let v = ctx.api_anonymous()?.health().await?;
    ctx.emit(&v, || {
        render_object(&v, &["healthy", "version", "runtimes", "time"])
    })
}

/// `warren keys ...`
pub async fn keys(ctx: &Ctx, cmd: KeysCmd) -> Result<i32, Failure> {
    let api = ctx.api()?;
    match cmd {
        KeysCmd::List => {
            let v = api.list_api_keys().await?;
            ctx.emit(&v, || render_list(Kind::Keys, &items_of(&v)))
        }
        KeysCmd::Create { name } => {
            let v = api.create_api_key(name.as_deref()).await?;
            ctx.emit(&v, || {
                let mut out = render_object(
                    &v["key"],
                    &["id", "name", "prefix", "created_at", "expires_at"],
                );
                out.push_str(&format!(
                    "api_key:     {}\n",
                    v.get("api_key").and_then(Value::as_str).unwrap_or("-")
                ));
                out.push_str("The plaintext key is shown only once.\n");
                out
            })
        }
        KeysCmd::Delete { id } => {
            let v = api.delete_api_key(&id).await?;
            ctx.emit(&v, || {
                format!(
                    "deleted {}\n",
                    v.get("id").and_then(Value::as_str).unwrap_or(&id)
                )
            })
        }
    }
}
