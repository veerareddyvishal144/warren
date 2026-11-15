//! `templates` (the current row per `(workspace_id, name)`) and
//! `template_revisions` (every published revision).
//!
//! System templates have an empty `workspace_id` and are visible to every
//! workspace; workspace templates are visible to their workspace only. A
//! workspace template that already exists is republished as `revision + 1`;
//! system templates are replaced in place at the revision the caller gives.

use rusqlite::{Connection, Row, ToSql, TransactionBehavior, named_params, params};
use warren_core::{Error, InstanceType, Result, Shape, Template, TemplateScope};

use crate::{
    errors::map_err,
    rows::{
        col_json, col_parse, count, exec, exec_expect, insert_sql, query_many, query_one,
        query_opt, to_json,
    },
};

const COLS: &[&str] = &[
    "workspace_id",
    "name",
    "scope",
    "image_ref",
    "image_digest",
    "default_port",
    "mount_path",
    "agent",
    "description",
    "types",
    "shape_cpu",
    "shape_memory",
    "shape_disk",
    "env",
    "revision",
    "created_at",
    "updated_at",
];

fn cols() -> String {
    COLS.join(", ")
}

fn read(row: &Row<'_>) -> rusqlite::Result<Template> {
    Ok(Template {
        name: row.get("name")?,
        scope: col_parse(row, "scope", TemplateScope::parse)?,
        workspace_id: row.get("workspace_id")?,
        image_ref: row.get("image_ref")?,
        image_digest: row.get("image_digest")?,
        default_port: row.get("default_port")?,
        mount_path: row.get("mount_path")?,
        agent: row.get("agent")?,
        description: row.get("description")?,
        types: col_json::<Vec<InstanceType>>(row, "types")?,
        default_shape: Shape {
            cpu: row.get("shape_cpu")?,
            memory: row.get("shape_memory")?,
            disk: row.get("shape_disk")?,
        },
        env: col_json(row, "env")?,
        revision: row.get("revision")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Insert-or-replace `t` into `table` (`templates` or `template_revisions`).
fn write_row(conn: &Connection, table: &str, t: &Template) -> Result<()> {
    let types = to_json(&t.types)?;
    let env = to_json(&t.env)?;
    let scope = t.scope.as_str();
    let binds: &[(&str, &dyn ToSql)] = named_params! {
        ":workspace_id": t.workspace_id,
        ":name": t.name,
        ":scope": scope,
        ":image_ref": t.image_ref,
        ":image_digest": t.image_digest,
        ":default_port": t.default_port,
        ":mount_path": t.mount_path,
        ":agent": t.agent,
        ":description": t.description,
        ":types": types,
        ":shape_cpu": t.default_shape.cpu,
        ":shape_memory": t.default_shape.memory,
        ":shape_disk": t.default_shape.disk,
        ":env": env,
        ":revision": t.revision,
        ":created_at": t.created_at,
        ":updated_at": t.updated_at,
    };
    exec(conn, &insert_sql(table, COLS, true), binds).map_err(map_err)?;
    Ok(())
}

/// Publish `t`. Everything is stored as given except `revision`, which the store
/// owns for workspace templates: a new name starts at `t.revision` (or 1 when
/// 0), an existing name becomes `current + 1`. Both the current row and the
/// revision row are written in one transaction.
pub(crate) fn upsert(conn: &mut Connection, t: &Template) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_err)?;
    let current: Option<u32> = query_opt(
        &tx,
        "SELECT revision FROM templates WHERE workspace_id = ?1 AND name = ?2",
        params![t.workspace_id, t.name],
        |r| r.get(0),
    )?;
    let mut row = t.clone();
    row.revision = match current {
        Some(rev) if !t.workspace_id.is_empty() => rev
            .checked_add(1)
            .ok_or_else(|| Error::conflict("template revision limit reached"))?,
        _ if t.revision == 0 => 1,
        _ => t.revision,
    };
    write_row(&tx, "templates", &row)?;
    write_row(&tx, "template_revisions", &row)?;
    tx.commit().map_err(map_err)?;
    Ok(())
}

/// The workspace's own template wins over a system template of the same name.
pub(crate) fn get(conn: &Connection, workspace_id: &str, name: &str) -> Result<Template> {
    query_one(
        conn,
        &format!(
            "SELECT {} FROM templates WHERE name = ?2 AND (workspace_id = ?1 OR workspace_id = '') \
             ORDER BY (workspace_id = '') ASC LIMIT 1",
            cols()
        ),
        params![workspace_id, name],
        "template",
        read,
    )
}

pub(crate) fn get_revision(
    conn: &Connection,
    workspace_id: &str,
    name: &str,
    revision: u32,
) -> Result<Template> {
    query_one(
        conn,
        &format!(
            "SELECT {} FROM template_revisions WHERE name = ?2 AND revision = ?3 \
             AND (workspace_id = ?1 OR workspace_id = '') ORDER BY (workspace_id = '') ASC LIMIT 1",
            cols()
        ),
        params![workspace_id, name, revision],
        "template revision",
        read,
    )
}

/// System catalog first, then the workspace's templates, each group by name.
pub(crate) fn list(conn: &Connection, workspace_id: &str) -> Result<Vec<Template>> {
    query_many(
        conn,
        &format!(
            "SELECT {} FROM templates WHERE workspace_id = '' OR workspace_id = ?1 \
             ORDER BY (workspace_id <> '') ASC, name ASC",
            cols()
        ),
        params![workspace_id],
        read,
    )
}

/// Remove the current row and every revision of `(workspace_id, name)`.
pub(crate) fn delete(conn: &mut Connection, workspace_id: &str, name: &str) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_err)?;
    exec_expect(
        &tx,
        "DELETE FROM templates WHERE workspace_id = ?1 AND name = ?2",
        params![workspace_id, name],
        "template",
    )?;
    exec(
        &tx,
        "DELETE FROM template_revisions WHERE workspace_id = ?1 AND name = ?2",
        params![workspace_id, name],
    )
    .map_err(map_err)?;
    tx.commit().map_err(map_err)?;
    Ok(())
}

/// Instances of the workspace built from template `name`, ignoring rows already
/// marked `deleted`. An empty `workspace_id` counts across all workspaces.
pub(crate) fn count_instances_using(
    conn: &Connection,
    workspace_id: &str,
    name: &str,
) -> Result<u32> {
    count(
        conn,
        "SELECT COUNT(*) FROM instances WHERE template_name = ?2 AND status <> 'deleted' \
         AND (?1 = '' OR workspace_id = ?1)",
        params![workspace_id, name],
    )
}
