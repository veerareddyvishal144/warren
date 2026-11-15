//! Scheduled prompts on agent instances. This layer validates and stores
//! crons and computes `next_run_at`; firing them is the scheduler crate's job.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use serde_json::{Value, json};
use warren_core::{Cron, Error, Instance, Result, ids, json::List};

use crate::{
    Caller, CreateCronRequest, Deps, JsonBody, PatchCronRequest,
    routes::{check_instance_id, created, envelope, newest_first},
    schedule::{MAX_PROMPT_BYTES, next_run_at, normalize_schedule, parse_schedule, parse_timezone},
    validate,
};

/// The instance must belong to the caller and serve the Agent API.
async fn agent_instance(d: &Deps, caller: &Caller, id: &str) -> Result<Instance> {
    check_instance_id(id)?;
    let inst = d.lifecycle.get(caller.workspace_id(), id).await?;
    if !inst.agent_api {
        return Err(Error::not_supported(format!(
            "crons need an agent template (agent_api: true); instance {id} runs {} which has none",
            inst.template_name
        )));
    }
    Ok(inst)
}

fn check_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        return Err(Error::invalid("prompt must not be empty"));
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(Error::invalid(format!(
            "prompt is {} bytes; the limit is {MAX_PROMPT_BYTES}",
            prompt.len()
        )));
    }
    Ok(())
}

/// `POST /v1/instances/{id}/crons` -> `201` cron
pub async fn create(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<CreateCronRequest>,
) -> Result<Response> {
    let inst = agent_instance(&d, &caller, &id).await?;
    check_prompt(&req.prompt)?;
    let name = req.name.unwrap_or_default().trim().to_string();
    validate::name("name", &name)?;
    parse_schedule(&req.schedule)?;
    let schedule = normalize_schedule(&req.schedule);
    let timezone = req.timezone.unwrap_or_default().trim().to_string();
    parse_timezone(&timezone)?;
    let now = d.clock.now_unix();
    let cron = Cron {
        id: ids::new_id(),
        instance_id: inst.id.clone(),
        workspace_id: caller.workspace.id.clone(),
        name,
        prompt: req.prompt,
        next_run_at: next_run_at(&schedule, &timezone, now)?,
        schedule,
        timezone,
        enabled: req.enabled.unwrap_or(true),
        last_run_at: 0,
        last_status: String::new(),
        created_at: now,
        updated_at: now,
    };
    d.store.create_cron(&cron).await?;
    tracing::info!(workspace = %caller.workspace.id, instance = %inst.id, cron = %cron.id, schedule = %cron.schedule, "cron created");
    Ok(created(cron))
}

/// `GET /v1/instances/{id}/crons`
pub async fn list(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<List<Cron>>> {
    let inst = agent_instance(&d, &caller, &id).await?;
    let mut crons = d.store.list_crons(&inst.id).await?;
    newest_first(&mut crons, |c| c.created_at);
    Ok(envelope(crons))
}

/// `GET /v1/instances/{id}/crons/{cid}`
pub async fn get_one(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path((id, cid)): Path<(String, String)>,
) -> Result<Json<Cron>> {
    let inst = agent_instance(&d, &caller, &id).await?;
    Ok(Json(d.store.get_cron(&inst.id, &cid).await?))
}

/// `PATCH /v1/instances/{id}/crons/{cid}`; a new schedule or timezone
/// recomputes `next_run_at`.
pub async fn patch(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path((id, cid)): Path<(String, String)>,
    JsonBody(req): JsonBody<PatchCronRequest>,
) -> Result<Json<Cron>> {
    let inst = agent_instance(&d, &caller, &id).await?;
    let mut cron = d.store.get_cron(&inst.id, &cid).await?;
    let mut reschedule = false;
    if let Some(prompt) = req.prompt {
        check_prompt(&prompt)?;
        cron.prompt = prompt;
    }
    if let Some(name) = req.name {
        let name = name.trim().to_string();
        validate::name("name", &name)?;
        cron.name = name;
    }
    if let Some(schedule) = req.schedule {
        parse_schedule(&schedule)?;
        let schedule = normalize_schedule(&schedule);
        reschedule |= schedule != cron.schedule;
        cron.schedule = schedule;
    }
    if let Some(timezone) = req.timezone {
        let timezone = timezone.trim().to_string();
        parse_timezone(&timezone)?;
        reschedule |= timezone != cron.timezone;
        cron.timezone = timezone;
    }
    if let Some(enabled) = req.enabled {
        cron.enabled = enabled;
    }
    let now = d.clock.now_unix();
    if reschedule {
        cron.next_run_at = next_run_at(&cron.schedule, &cron.timezone, now)?;
    }
    cron.updated_at = now;
    d.store.update_cron(&cron).await?;
    Ok(Json(cron))
}

/// `DELETE /v1/instances/{id}/crons/{cid}` -> `{ id, deleted: true }`
pub async fn delete(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path((id, cid)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let inst = agent_instance(&d, &caller, &id).await?;
    d.store.delete_cron(&inst.id, &cid).await?;
    tracing::info!(workspace = %caller.workspace.id, instance = %inst.id, cron = %cid, "cron deleted");
    Ok(Json(json!({ "id": cid, "deleted": true })))
}
