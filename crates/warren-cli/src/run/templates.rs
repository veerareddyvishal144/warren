//! `warren templates ...`

use serde_json::Value;
use warren_cli::Failure;
use warren_cli::cli::{RegisterArgs, TemplatesCmd};
use warren_cli::output::{Kind, TEMPLATE_KEYS, items_of, render_list, render_object};
use warren_cli::util::parse_env_pairs;
use warren_core::RegisterTemplateRequest;

use super::{Ctx, usage};

pub async fn run(ctx: &Ctx, cmd: TemplatesCmd) -> Result<i32, Failure> {
    let api = ctx.api()?;
    match cmd {
        TemplatesCmd::List => {
            let v = api.list_templates().await?;
            ctx.emit(&v, || render_list(Kind::Templates, &items_of(&v)))
        }
        TemplatesCmd::Get { name } => {
            let v = api.get_template(&name).await?;
            ctx.emit(&v, || render_object(&v, TEMPLATE_KEYS))
        }
        TemplatesCmd::Register(args) => {
            let req = register_request(&args)?;
            let v = api.register_template(&req).await?;
            ctx.emit(&v, || render_object(&v, TEMPLATE_KEYS))
        }
        TemplatesCmd::Delete { name } => {
            let v = api.delete_template(&name).await?;
            ctx.emit(&v, || {
                format!(
                    "deleted {}\n",
                    v.get("name").and_then(Value::as_str).unwrap_or(&name)
                )
            })
        }
    }
}

/// Build the request body; env keys are validated locally (exit 2).
pub fn register_request(args: &RegisterArgs) -> Result<RegisterTemplateRequest, Failure> {
    if !warren_core::ids::valid_template_name(&args.name) {
        return Err(Failure::usage(format!(
            "invalid template name {:?}: lowercase letters, digits and dashes, 3 to 40 characters",
            args.name
        )));
    }
    Ok(RegisterTemplateRequest {
        name: args.name.clone(),
        image_ref: args.image.clone(),
        default_port: args.port.unwrap_or(0),
        mount_path: args.mount.clone().unwrap_or_default(),
        agent: args.agent,
        description: args.description.clone().unwrap_or_default(),
        env: parse_env_pairs(&args.env).map_err(usage)?,
    })
}
