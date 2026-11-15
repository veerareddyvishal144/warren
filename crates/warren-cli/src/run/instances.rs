//! `warren instances ...`: lifecycle, exec, logs, metrics, URLs, backups, crons.

use std::io::Write as _;
use std::time::Duration;

use serde_json::Value;
use warren_cli::cli::{BackupsCmd, CreateArgs, CronsCmd, InstancesCmd};
use warren_cli::output::{
    INSTANCE_KEYS, Kind, items_of, render_list, render_object, to_json_pretty,
};
use warren_cli::util::{find_in_path, parse_env_pairs, parse_pairs, shell_join};
use warren_cli::{EXIT_OK, Failure};
use warren_core::{
    CreateInstanceRequest, Error, ExecRequest, ForkRequest, InstanceStatus, ShapeRequest,
};

use super::api::Api;
use super::{Ctx, status_line, usage};

/// How long `--wait` polls before giving up.
const WAIT_TIMEOUT: Duration = Duration::from_secs(600);
/// Poll interval for `--wait`.
const WAIT_INTERVAL: Duration = Duration::from_secs(1);

pub async fn run(ctx: &Ctx, cmd: InstancesCmd) -> Result<i32, Failure> {
    let api = ctx.api()?;
    match cmd {
        InstancesCmd::List => {
            let v = api.list_instances().await?;
            ctx.emit(&v, || render_list(Kind::Instances, &items_of(&v)))
        }
        InstancesCmd::Create(args) => create(ctx, &api, &args).await,
        InstancesCmd::Get { id } => {
            let v = api.get_instance(&id).await?;
            ctx.emit(&v, || render_object(&v, INSTANCE_KEYS))
        }
        InstancesCmd::Delete { id } => {
            let v = api.delete_instance(&id).await?;
            ctx.emit(&v, || status_line(&v))
        }
        InstancesCmd::Stop { id } => {
            let v = api.stop_instance(&id).await?;
            ctx.emit(&v, || status_line(&v))
        }
        InstancesCmd::Start { id } => {
            let v = api.start_instance(&id).await?;
            ctx.emit(&v, || status_line(&v))
        }
        InstancesCmd::Sleep { id } => {
            let v = api.sleep_instance(&id).await?;
            ctx.emit(&v, || status_line(&v))
        }
        InstancesCmd::Restart { id } => {
            let v = api.restart_instance(&id).await?;
            ctx.emit(&v, || status_line(&v))
        }
        InstancesCmd::Resize {
            id,
            cpu,
            memory,
            disk,
        } => {
            if cpu.is_none() && memory.is_none() && disk.is_none() {
                return Err(Failure::usage(
                    "resize needs at least one of --cpu, --memory, --disk",
                ));
            }
            let v = api
                .resize_instance(&id, &ShapeRequest { cpu, memory, disk })
                .await?;
            ctx.emit(&v, || status_line(&v))
        }
        InstancesCmd::Update { id, template } => {
            let v = api.update_instance(&id, template.as_deref()).await?;
            ctx.emit(&v, || status_line(&v))
        }
        InstancesCmd::Fork { id, name } => {
            let v = api
                .fork_instance(
                    &id,
                    &ForkRequest {
                        name,
                        ..Default::default()
                    },
                )
                .await?;
            ctx.emit(&v, || render_object(&v, INSTANCE_KEYS))
        }
        InstancesCmd::Exec {
            id,
            user,
            workdir,
            timeout,
            command,
        } => exec(ctx, &api, &id, user, workdir, timeout, &command).await,
        InstancesCmd::Logs { id, tail } => {
            let v = api.logs(&id, tail).await?;
            ctx.emit(&v, || {
                let mut text = v
                    .get("logs")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text
            })
        }
        InstancesCmd::Metrics { id, hours } => {
            let v = api.metrics(&id, hours).await?;
            ctx.emit(&v, || render_list(Kind::Metrics, &items_of(&v)))
        }
        InstancesCmd::Url { id, port, ttl } => {
            let v = api.signed_url(&id, port, ttl).await?;
            ctx.emit(&v, || format!("{}\n", url_of(&v)))
        }
        InstancesCmd::Open { id, port } => open(ctx, &api, &id, port).await,
        InstancesCmd::Backups { cmd } => backups(ctx, &api, cmd).await,
        InstancesCmd::Crons { cmd } => crons(ctx, &api, cmd).await,
    }
}

/// Translate the create flags into the request body. Local validation
/// failures are usage errors.
pub fn create_request(args: &CreateArgs) -> Result<CreateInstanceRequest, Failure> {
    let shape = (args.cpu.is_some() || args.memory.is_some() || args.disk.is_some()).then_some(
        ShapeRequest {
            cpu: args.cpu,
            memory: args.memory,
            disk: args.disk,
        },
    );
    let env = parse_env_pairs(&args.env).map_err(usage)?;
    let metadata = parse_pairs(&args.metadata).map_err(usage)?;
    if let Some(idle) = args.idle
        && !(warren_core::MIN_IDLE_TIMEOUT_SEC..=warren_core::MAX_IDLE_TIMEOUT_SEC).contains(&idle)
    {
        return Err(Failure::usage(format!(
            "--idle must be between {} and {} seconds",
            warren_core::MIN_IDLE_TIMEOUT_SEC,
            warren_core::MAX_IDLE_TIMEOUT_SEC
        )));
    }
    Ok(CreateInstanceRequest {
        template: args.template.clone(),
        shape,
        kind: args.kind,
        region: None,
        name: args.name.clone(),
        metadata: (!metadata.is_empty()).then_some(metadata),
        env: (!env.is_empty()).then_some(env),
        budget: None,
        auto_sleep: args.auto_sleep.then_some(true),
        idle_timeout_seconds: args.idle,
        network: args.network,
    })
}

async fn create(ctx: &Ctx, api: &Api, args: &CreateArgs) -> Result<i32, Failure> {
    let req = create_request(args)?;
    let mut v = api.create_instance(&req).await?;
    if args.wait {
        let id = v
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !ctx.json {
            eprintln!("created {id}; waiting for it to run...");
        }
        v = wait_running(api, &id).await?;
    }
    ctx.emit(&v, || render_object(&v, INSTANCE_KEYS))
}

/// Poll until the instance is `running`; `failed` and `deleted` are errors.
async fn wait_running(api: &Api, id: &str) -> Result<Value, Failure> {
    let started = std::time::Instant::now();
    loop {
        let v = api.get_instance(id).await?;
        let status = v
            .get("status")
            .and_then(Value::as_str)
            .and_then(InstanceStatus::parse);
        match status {
            Some(InstanceStatus::Running) => return Ok(v),
            Some(InstanceStatus::Failed)
            | Some(InstanceStatus::Deleted)
            | Some(InstanceStatus::Deleting) => {
                let reason = v.get("status_reason").and_then(Value::as_str).unwrap_or("");
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
        if started.elapsed() >= WAIT_TIMEOUT {
            return Err(Failure::Api(Error::timeout(format!(
                "instance {id} did not reach running within {}s",
                WAIT_TIMEOUT.as_secs()
            ))));
        }
        tokio::time::sleep(WAIT_INTERVAL).await;
    }
}

async fn exec(
    ctx: &Ctx,
    api: &Api,
    id: &str,
    user: Option<String>,
    workdir: Option<String>,
    timeout: Option<u32>,
    command: &[String],
) -> Result<i32, Failure> {
    let req = ExecRequest {
        command: shell_join(command),
        user: user.unwrap_or_default(),
        timeout_seconds: timeout.unwrap_or(0),
        workdir: workdir.unwrap_or_default(),
        env: Default::default(),
    };
    let result = api.exec(id, &req).await?;
    if ctx.json {
        println!(
            "{}",
            to_json_pretty(
                &serde_json::to_value(&result).map_err(|e| Failure::local(e.to_string()))?
            )
        );
    } else {
        let mut out = std::io::stdout().lock();
        out.write_all(result.stdout.as_bytes())?;
        out.flush()?;
        let mut err = std::io::stderr().lock();
        err.write_all(result.stderr.as_bytes())?;
        if result.truncated {
            writeln!(err, "warren: output truncated at 1 MiB")?;
        }
        err.flush()?;
    }
    Ok(result.exit_code)
}

fn url_of(v: &Value) -> String {
    v.get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

async fn open(ctx: &Ctx, api: &Api, id: &str, port: Option<u16>) -> Result<i32, Failure> {
    let v = api.signed_url(id, port, None).await?;
    let url = url_of(&v);
    ctx.emit(&v, || format!("{url}\n"))?;
    match find_in_path("xdg-open") {
        Some(bin) => {
            let spawned = std::process::Command::new(bin)
                .arg(&url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Err(e) = spawned {
                eprintln!("warren: xdg-open failed: {e}");
            }
        }
        None => eprintln!("warren: xdg-open not found; open the URL above in a browser"),
    }
    Ok(EXIT_OK)
}

async fn backups(ctx: &Ctx, api: &Api, cmd: BackupsCmd) -> Result<i32, Failure> {
    match cmd {
        BackupsCmd::List { id } => {
            let v = api.list_backups(&id).await?;
            ctx.emit(&v, || render_list(Kind::Backups, &items_of(&v)))
        }
        BackupsCmd::Create { id, note } => {
            let v = api.create_backup(&id, note.as_deref()).await?;
            ctx.emit(&v, || {
                render_object(
                    &v,
                    &["id", "instance_id", "size_bytes", "created_at", "note"],
                )
            })
        }
        BackupsCmd::Restore { id, backup_id } => {
            let v = api.restore_backup(&id, &backup_id).await?;
            ctx.emit(&v, || render_object(&v, INSTANCE_KEYS))
        }
    }
}

async fn crons(ctx: &Ctx, api: &Api, cmd: CronsCmd) -> Result<i32, Failure> {
    match cmd {
        CronsCmd::List { id } => {
            let v = api.list_crons(&id).await?;
            ctx.emit(&v, || render_list(Kind::Crons, &items_of(&v)))
        }
        CronsCmd::Create {
            id,
            prompt,
            schedule,
            tz,
            name,
        } => {
            if schedule.split_whitespace().count() != 5 {
                return Err(Failure::usage(format!(
                    "--schedule must be a five-field cron expression, got {schedule:?}"
                )));
            }
            let v = api
                .create_cron(&id, &prompt, &schedule, tz.as_deref(), name.as_deref())
                .await?;
            ctx.emit(&v, || {
                render_object(
                    &v,
                    &[
                        "id",
                        "name",
                        "schedule",
                        "timezone",
                        "enabled",
                        "next_run_at",
                        "prompt",
                    ],
                )
            })
        }
        CronsCmd::Delete { id, cron_id } => {
            let v = api.delete_cron(&id, &cron_id).await?;
            ctx.emit(&v, || {
                format!(
                    "deleted {}\n",
                    v.get("id").and_then(Value::as_str).unwrap_or(&cron_id)
                )
            })
        }
    }
}
