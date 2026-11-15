//! `warren chat` and `warren files`: the Agent API inside an instance.

use std::io::Write as _;
use std::path::Path;

use futures::StreamExt;
use serde_json::{Value, json};
use warren_cli::cli::{ChatArgs, FilesCmd};
use warren_cli::output::{Kind, items_of, render_list, render_object, to_json_pretty};
use warren_cli::{EXIT_API, EXIT_OK, Failure};
use warren_core::agentapi::{
    EVENT_COMPLETED, EVENT_CREATED, EVENT_DELTA, EVENT_FAILED, EVENT_TOOL_CALL, ResponsesRequest,
};

use super::Ctx;

/// `warren chat ID [--session SID] [--no-stream] MESSAGE...`
pub async fn chat(ctx: &Ctx, args: ChatArgs) -> Result<i32, Failure> {
    let api = ctx.api()?;
    let req = ResponsesRequest {
        input: args.message.join(" "),
        session_id: args.session.clone(),
        stream: Some(!args.no_stream),
        ..Default::default()
    };
    if args.no_stream {
        let v = api.chat(&args.id, &req).await?;
        if ctx.json {
            println!("{}", to_json_pretty(&v));
        } else {
            if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                eprintln!("session: {sid}");
            }
            let text = v
                .get("output_text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            println!("{text}");
        }
        let failed = v.get("status").and_then(Value::as_str) == Some("failed");
        if failed {
            eprintln!(
                "warren: response failed: {}",
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
            return Ok(EXIT_API);
        }
        return Ok(EXIT_OK);
    }

    let mut events = api.chat_stream(&args.id, &req).await?;
    let mut stdout = std::io::stdout().lock();
    let mut wrote_text = false;
    let mut code = EXIT_OK;
    let mut completed = false;
    while let Some(event) = events.next().await {
        let event = event?;
        if ctx.json {
            writeln!(
                stdout,
                "{}",
                json!({ "event": event.name, "data": event.data })
            )?;
            stdout.flush()?;
            if event.name == EVENT_FAILED {
                code = EXIT_API;
            }
            if event.name == EVENT_COMPLETED || event.name == EVENT_FAILED {
                completed = true;
                break;
            }
            continue;
        }
        match event.name.as_str() {
            EVENT_CREATED => {
                if let Some(sid) = event.data.get("session_id").and_then(Value::as_str) {
                    eprintln!("session: {sid}");
                }
            }
            EVENT_DELTA => {
                if let Some(delta) = event.data.get("delta").and_then(Value::as_str) {
                    stdout.write_all(delta.as_bytes())?;
                    stdout.flush()?;
                    wrote_text = wrote_text || !delta.is_empty();
                }
            }
            EVENT_TOOL_CALL => {
                let name = event
                    .data
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                eprintln!("[tool call: {name}]");
            }
            EVENT_COMPLETED => {
                if !wrote_text {
                    // Nothing streamed: print the final text instead.
                    if let Some(text) = event.data.get("output_text").and_then(Value::as_str) {
                        stdout.write_all(text.as_bytes())?;
                        wrote_text = !text.is_empty();
                    }
                }
                if wrote_text {
                    stdout.write_all(b"\n")?;
                }
                stdout.flush()?;
                completed = true;
                break;
            }
            EVENT_FAILED => {
                if wrote_text {
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
                eprintln!(
                    "warren: response failed: {}",
                    event
                        .data
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                );
                code = EXIT_API;
                completed = true;
                break;
            }
            _ => {}
        }
    }
    if !completed {
        if wrote_text && !ctx.json {
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        eprintln!("warren: the stream ended before the response completed");
        code = EXIT_API;
    }
    Ok(code)
}

/// `warren files ...`
pub async fn files(ctx: &Ctx, cmd: FilesCmd) -> Result<i32, Failure> {
    let api = ctx.api()?;
    match cmd {
        FilesCmd::Ls { id, path } => {
            let v = api.files_list(&id, path.as_deref().unwrap_or(".")).await?;
            ctx.emit(&v, || render_list(Kind::Files, &items_of(&v["entries"])))
        }
        FilesCmd::Cat { id, path } => {
            let bytes = api.files_read(&id, &path).await?;
            let mut out = std::io::stdout().lock();
            out.write_all(&bytes)?;
            out.flush()?;
            Ok(EXIT_OK)
        }
        FilesCmd::Put { id, local, remote } => {
            let bytes = std::fs::read(&local)
                .map_err(|e| Failure::local(format!("reading {}: {e}", local.display())))?;
            let v = api.files_write(&id, &remote, bytes).await?;
            ctx.emit(&v, || render_object(&v, &["path", "size"]))
        }
        FilesCmd::Rm { id, path } => {
            let v = api.files_delete(&id, &path).await?;
            ctx.emit(&v, || {
                format!(
                    "deleted {}\n",
                    v.get("path").and_then(Value::as_str).unwrap_or(&path)
                )
            })
        }
        FilesCmd::Download { id, path, local } => {
            let bytes = api.files_download(&id, &path).await?;
            if local == Path::new("-") {
                let mut out = std::io::stdout().lock();
                out.write_all(&bytes)?;
                out.flush()?;
            } else {
                std::fs::write(&local, &bytes)
                    .map_err(|e| Failure::local(format!("writing {}: {e}", local.display())))?;
                if ctx.json {
                    println!(
                        "{}",
                        json!({ "path": path, "local": local, "size": bytes.len() })
                    );
                } else {
                    println!("wrote {} ({} bytes)", local.display(), bytes.len());
                }
            }
            Ok(EXIT_OK)
        }
    }
}
