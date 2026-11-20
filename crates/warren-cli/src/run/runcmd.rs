//! `warren run`: bind the current directory to an instance, push it, run a
//! command there on a terminal (or through exec with `--no-tty`), then pull
//! the changes back and exit with the command's code.
//!
//! The binding lives in `.warren/instance.json` (see `sync::Binding`); the
//! first run creates the instance (template by type: `warren-base`, or
//! `warren-macos` for `--type mac`) and waits for it to run. Later runs
//! reuse it, waking or starting it when it is asleep or stopped.
//! `warren run --unbind [--delete]` forgets (and optionally destroys) it.

use std::{
    io::Write as _,
    path::Path,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use warren_cli::{
    EXIT_OK, Failure,
    cli::RunArgs,
    util::{shell_join, shell_quote},
};
use warren_core::{CreateInstanceRequest, Error, ExecRequest, InstanceStatus, InstanceType};

use super::{
    Ctx,
    api::Api,
    shell,
    sync::{self, BINDING_FILE, Binding},
};

/// How long the instance may take to reach `running`.
const READY_TIMEOUT: Duration = Duration::from_secs(600);
/// Status poll interval.
const POLL: Duration = Duration::from_secs(1);
/// Exec timeout for `--no-tty` commands: the API's maximum (a day), so a
/// long build or test run is never cut short by the client.
const NO_TTY_TIMEOUT_SEC: u32 = 86_400;

/// The template used when `--template` is absent.
pub fn default_template(kind: Option<InstanceType>) -> &'static str {
    match kind {
        Some(InstanceType::Mac) => "warren-macos",
        _ => "warren-base",
    }
}

/// The create request for a first run in a directory called `dir_name`.
pub fn create_request(args: &RunArgs, dir_name: &str) -> CreateInstanceRequest {
    CreateInstanceRequest {
        template: Some(
            args.template
                .clone()
                .unwrap_or_else(|| default_template(args.kind).to_string()),
        ),
        kind: args.kind,
        name: Some(
            args.name
                .clone()
                .unwrap_or_else(|| format!("run: {dir_name}")),
        ),
        ..Default::default()
    }
}

/// The command run inside the instance: `cd <root> && <command>`.
pub fn remote_command(remote_root: &str, command: &[String]) -> String {
    format!("cd {} && {}", shell_quote(remote_root), shell_join(command))
}

/// True when `.gitignore` in `dir` already lists `.warren`.
pub fn gitignore_covers_warren(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(".gitignore"))
        .map(|text| {
            text.lines().any(|l| {
                let l = l.trim().trim_start_matches('/').trim_end_matches('/');
                l == ".warren"
            })
        })
        .unwrap_or(false)
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// `warren run [--new] [--type T] [--template T] [--name N] [--no-sync] [--no-tty] -- COMMAND...`
/// and `warren run --unbind [--delete]`.
pub async fn run(ctx: &Ctx, args: RunArgs) -> Result<i32, Failure> {
    let cwd =
        std::env::current_dir().map_err(|e| Failure::local(format!("current directory: {e}")))?;
    if args.unbind {
        return unbind(ctx, &cwd, args.delete).await;
    }
    if args.command.is_empty() {
        return Err(Failure::usage("a command is required after `--`"));
    }
    let api = ctx.api()?;
    let (binding, inst) = bind(&api, &cwd, &args).await?;
    let inst = ensure_running(&api, &binding.id, inst).await?;
    let excludes = sync::excludes(&[]);
    if !args.no_sync {
        let report = sync::push(&api, &binding.id, &cwd, &binding.remote_root, &excludes).await?;
        eprintln!(
            "warren: pushed {} files to {}:{}",
            report.files.len(),
            binding.id,
            binding.remote_root
        );
    }
    let command = remote_command(&binding.remote_root, &args.command);
    let code = if args.no_tty {
        exec_plain(&api, &binding.id, &command).await?
    } else {
        let target = shell::instance_target(ctx, &inst)?;
        shell::run_command(&target, Some(&command), None).await?
    };
    if !args.no_sync {
        let changed = sync::pull(&api, &binding.id, &cwd, &binding.remote_root, &excludes).await?;
        let mut err = std::io::stderr().lock();
        for f in &changed {
            writeln!(err, "warren: pulled {}", f.display())?;
        }
        writeln!(
            err,
            "warren: pulled {} changed files from {}:{}",
            changed.len(),
            binding.id,
            binding.remote_root
        )?;
    }
    Ok(code)
}

/// The binding for `cwd`: the existing one when it still names an instance
/// (unless `--new`), else a fresh instance, created and bound.
async fn bind(api: &Api, cwd: &Path, args: &RunArgs) -> Result<(Binding, Value), Failure> {
    if !args.new
        && let Some(existing) = Binding::load(cwd)?
    {
        match api.get_instance(&existing.id).await {
            Ok(inst) => {
                let mut binding = existing;
                if let Some(root) = &args.remote_root
                    && *root != binding.remote_root
                {
                    binding.remote_root = root.clone();
                    binding.save(cwd)?;
                }
                return Ok((binding, inst));
            }
            Err(Failure::Api(e)) if e.status == 404 => {
                eprintln!(
                    "warren: instance {} bound to this directory no longer exists; creating a new one",
                    existing.id
                );
            }
            Err(e) => return Err(e),
        }
    }
    let name = sync::dir_name(cwd);
    let req = create_request(args, &name);
    let created = api.create_instance(&req).await?;
    let id = str_field(&created, "id").to_string();
    if id.is_empty() {
        return Err(Failure::Api(Error::internal(
            "the create response carried no instance id",
        )));
    }
    eprintln!(
        "warren: created instance {id} ({}); waiting for it to run...",
        req.template.as_deref().unwrap_or_default()
    );
    let inst = ensure_running(api, &id, created).await?;
    let remote_root = args
        .remote_root
        .clone()
        .unwrap_or_else(|| sync::default_remote_root(str_field(&inst, "mount_path"), &name));
    let binding = Binding::new(&id, &remote_root);
    binding.save(cwd)?;
    eprintln!(
        "warren: bound {} to {id} in {BINDING_FILE}{}",
        cwd.display(),
        if gitignore_covers_warren(cwd) {
            ""
        } else {
            "; add .warren/ to your .gitignore"
        }
    );
    Ok((binding, inst))
}

/// Poll until the instance runs, starting it when it is asleep or stopped.
async fn ensure_running(api: &Api, id: &str, mut inst: Value) -> Result<Value, Failure> {
    let started = Instant::now();
    let mut asked_to_start = false;
    loop {
        let status = InstanceStatus::parse(str_field(&inst, "status"));
        match status {
            Some(InstanceStatus::Running) => return Ok(inst),
            Some(InstanceStatus::Sleeping) | Some(InstanceStatus::Stopped) if !asked_to_start => {
                eprintln!(
                    "warren: instance {id} is {}; starting it...",
                    status.map(|s| s.as_str()).unwrap_or("asleep")
                );
                api.start_instance(id).await?;
                asked_to_start = true;
            }
            Some(InstanceStatus::Failed)
            | Some(InstanceStatus::Deleting)
            | Some(InstanceStatus::Deleted) => {
                let reason = str_field(&inst, "status_reason");
                return Err(Failure::Api(Error::conflict(format!(
                    "instance {id} is {}{}",
                    status.map(|s| s.as_str()).unwrap_or("unknown"),
                    if reason.is_empty() {
                        String::new()
                    } else {
                        format!(": {reason}")
                    }
                ))));
            }
            _ => {}
        }
        if started.elapsed() >= READY_TIMEOUT {
            return Err(Failure::Api(Error::timeout(format!(
                "instance {id} did not reach running within {}s",
                READY_TIMEOUT.as_secs()
            ))));
        }
        tokio::time::sleep(POLL).await;
        inst = api.get_instance(id).await?;
    }
}

/// `--no-tty`: run through exec and print the captured streams at the end.
async fn exec_plain(api: &Api, id: &str, command: &str) -> Result<i32, Failure> {
    let req = ExecRequest {
        command: command.to_string(),
        timeout_seconds: NO_TTY_TIMEOUT_SEC,
        ..Default::default()
    };
    let result = api.exec(id, &req).await?;
    let mut out = std::io::stdout().lock();
    out.write_all(result.stdout.as_bytes())?;
    out.flush()?;
    let mut err = std::io::stderr().lock();
    err.write_all(result.stderr.as_bytes())?;
    if result.truncated {
        writeln!(err, "warren: output truncated at 1 MiB")?;
    }
    err.flush()?;
    Ok(result.exit_code)
}

/// `warren run --unbind [--delete]`
async fn unbind(ctx: &Ctx, cwd: &Path, delete: bool) -> Result<i32, Failure> {
    let Some(binding) = Binding::load(cwd)? else {
        return Err(Failure::usage(
            "this directory is not bound to an instance (no .warren/instance.json)",
        ));
    };
    let mut deleted = false;
    if delete {
        let api = ctx.api()?;
        match api.delete_instance(&binding.id).await {
            Ok(_) => deleted = true,
            Err(Failure::Api(e)) if e.status == 404 => {
                eprintln!("warren: instance {} was already gone", binding.id);
            }
            Err(e) => return Err(e),
        }
    }
    Binding::remove(cwd)?;
    let v = json!({ "id": binding.id, "unbound": true, "deleted": deleted });
    ctx.emit(&v, || {
        format!(
            "unbound {} from {}{}\n",
            cwd.display(),
            binding.id,
            if deleted { " (instance deleted)" } else { "" }
        )
    })
    .map(|_| EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(kind: Option<InstanceType>, template: Option<&str>, name: Option<&str>) -> RunArgs {
        RunArgs {
            new: false,
            kind,
            template: template.map(str::to_string),
            name: name.map(str::to_string),
            remote_root: None,
            no_sync: false,
            no_tty: false,
            unbind: false,
            delete: false,
            command: vec!["true".to_string()],
        }
    }

    #[test]
    fn templates_by_type() {
        assert_eq!(default_template(None), "warren-base");
        assert_eq!(default_template(Some(InstanceType::Default)), "warren-base");
        assert_eq!(default_template(Some(InstanceType::Vm)), "warren-base");
        assert_eq!(default_template(Some(InstanceType::Mac)), "warren-macos");
        let r = create_request(&args(Some(InstanceType::Mac), None, None), "proj");
        assert_eq!(r.template.as_deref(), Some("warren-macos"));
        assert_eq!(r.kind, Some(InstanceType::Mac));
        assert_eq!(r.name.as_deref(), Some("run: proj"));
        let r = create_request(&args(None, Some("my-tpl@2"), Some("x")), "proj");
        assert_eq!(r.template.as_deref(), Some("my-tpl@2"));
        assert_eq!(r.kind, None);
        assert_eq!(r.name.as_deref(), Some("x"));
        assert!(r.env.is_none() && r.shape.is_none());
    }

    #[test]
    fn remote_command_quotes_the_root() {
        assert_eq!(
            remote_command("/home/agent/work/app", &["claude".to_string()]),
            "cd /home/agent/work/app && claude"
        );
        assert_eq!(
            remote_command(
                "/Users/lume/work/my app",
                &[
                    "xcodebuild".to_string(),
                    "-scheme".to_string(),
                    "My App".to_string()
                ]
            ),
            "cd '/Users/lume/work/my app' && xcodebuild -scheme 'My App'"
        );
        assert_eq!(
            remote_command("w", &["npm test && echo ok".to_string()]),
            "cd w && npm test && echo ok"
        );
    }

    #[test]
    fn gitignore_check() {
        let dir = std::env::temp_dir().join(format!(
            "warren-run-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!gitignore_covers_warren(&dir));
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        assert!(!gitignore_covers_warren(&dir));
        std::fs::write(dir.join(".gitignore"), "target/\n.warren/\n").unwrap();
        assert!(gitignore_covers_warren(&dir));
        std::fs::write(dir.join(".gitignore"), "/.warren\n").unwrap();
        assert!(gitignore_covers_warren(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
