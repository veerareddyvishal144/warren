//! Human-readable tables and key/value listings over JSON values.
//!
//! Every renderer takes `serde_json::Value` so the formatting does not depend
//! on which concrete types the SDK returns; `--json` prints the value as is.

use serde_json::{Map, Value, json};

use crate::util::{format_bytes, format_timestamp, truncate};

/// Pretty JSON (no trailing newline).
pub fn to_json_pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Wrap items in the wire envelope `{"data": [...]}`.
pub fn list_envelope(items: Vec<Value>) -> Value {
    json!({ "data": items })
}

/// Items of `{"data": [...]}` or of a bare array; anything else is one item.
pub fn items_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Object(o) => match o.get("data") {
            Some(Value::Array(a)) => a.clone(),
            _ => vec![v.clone()],
        },
        Value::Null => vec![],
        other => vec![other.clone()],
    }
}

/// A left-aligned text table with a header row.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// A table with these column headers.
    pub fn new<I, S>(headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Table {
            headers: headers
                .into_iter()
                .map(|h| h.as_ref().to_string())
                .collect(),
            rows: Vec::new(),
        }
    }

    /// Append a row; short rows are padded with empty cells.
    pub fn row<I, S>(&mut self, cells: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut row: Vec<String> = cells.into_iter().map(Into::into).collect();
        row.resize(self.headers.len(), String::new());
        self.rows.push(row);
        self
    }

    /// Number of data rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no rows were added.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render with two spaces between columns; each line ends with a newline and
    /// carries no trailing spaces.
    pub fn render(&self) -> String {
        let width = |s: &str| s.chars().count();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| width(h)).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(width(cell));
            }
        }
        let mut out = String::new();
        let mut line = |cells: &[String]| {
            let mut l = String::new();
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    l.push_str("  ");
                }
                l.push_str(cell);
                if i + 1 < cells.len() {
                    l.extend(std::iter::repeat_n(' ', widths[i] - width(cell)));
                }
            }
            out.push_str(l.trim_end());
            out.push('\n');
        };
        line(&self.headers);
        for row in &self.rows {
            line(row);
        }
        out
    }
}

/// Which kind of list to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Instances,
    Templates,
    Keys,
    Backups,
    Crons,
    Metrics,
    Files,
    Sessions,
    Models,
}

impl Kind {
    /// The plural noun used in the empty message.
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Instances => "instances",
            Kind::Templates => "templates",
            Kind::Keys => "API keys",
            Kind::Backups => "backups",
            Kind::Crons => "crons",
            Kind::Metrics => "metric samples",
            Kind::Files => "entries",
            Kind::Sessions => "sessions",
            Kind::Models => "models",
        }
    }

    fn headers(self) -> &'static [&'static str] {
        match self {
            Kind::Instances => &[
                "ID", "NAME", "STATUS", "TEMPLATE", "TYPE", "CPU", "MEM", "DISK", "CREATED",
            ],
            Kind::Templates => &[
                "NAME",
                "SCOPE",
                "REV",
                "IMAGE",
                "PORT",
                "AGENT",
                "SHAPE",
                "DESCRIPTION",
            ],
            Kind::Keys => &["ID", "NAME", "PREFIX", "CREATED", "LAST USED", "EXPIRES"],
            Kind::Backups => &["ID", "SIZE", "CREATED", "NOTE"],
            Kind::Crons => &[
                "ID",
                "NAME",
                "SCHEDULE",
                "TZ",
                "ENABLED",
                "NEXT RUN",
                "LAST RUN",
                "LAST STATUS",
                "PROMPT",
            ],
            Kind::Metrics => &["AT", "CPU %", "MEMORY", "DISK"],
            Kind::Files => &["TYPE", "SIZE", "MODIFIED", "NAME"],
            Kind::Sessions => &["ID", "NAME", "MESSAGES", "CREATED", "UPDATED"],
            Kind::Models => &["ID"],
        }
    }

    fn cells(self, v: &Value) -> Vec<String> {
        match self {
            Kind::Instances => vec![
                text(v, "id"),
                text(v, "name"),
                text(v, "status"),
                text(v, "template_name").if_dash(|| text(v, "template")),
                text(v, "type"),
                text(&v["resources"], "cpu"),
                text(&v["resources"], "memory"),
                text(&v["resources"], "disk"),
                when(v, "created_at"),
            ],
            Kind::Templates => vec![
                text(v, "name"),
                text(v, "scope"),
                text(v, "revision"),
                text(v, "image_ref"),
                text(v, "default_port"),
                text(v, "agent"),
                shape(&v["default_shape"]),
                truncate(&text(v, "description").if_dash(String::new), 48),
            ],
            Kind::Keys => vec![
                text(v, "id"),
                text(v, "name"),
                text(v, "prefix"),
                when(v, "created_at"),
                when(v, "last_used_at").if_dash(|| "never".to_string()),
                when(v, "expires_at").if_dash(|| "never".to_string()),
            ],
            Kind::Backups => vec![
                text(v, "id"),
                bytes(v, "size_bytes"),
                when(v, "created_at"),
                text(v, "note").if_dash(String::new),
            ],
            Kind::Crons => vec![
                text(v, "id"),
                text(v, "name"),
                text(v, "schedule"),
                text(v, "timezone").if_dash(|| "UTC".to_string()),
                text(v, "enabled"),
                when(v, "next_run_at"),
                when(v, "last_run_at"),
                text(v, "last_status"),
                truncate(&text(v, "prompt"), 40),
            ],
            Kind::Metrics => vec![
                when(v, "at"),
                v.get("cpu_percent")
                    .and_then(Value::as_f64)
                    .map(|c| format!("{c:.1}"))
                    .unwrap_or_else(|| "-".into()),
                bytes(v, "memory_bytes"),
                bytes(v, "disk_bytes"),
            ],
            Kind::Files => vec![
                text(v, "type"),
                bytes(v, "size"),
                when(v, "modified_at"),
                text(v, "name"),
            ],
            Kind::Sessions => vec![
                text(v, "id"),
                text(v, "name"),
                text(v, "message_count"),
                when(v, "created_at"),
                when(v, "updated_at"),
            ],
            Kind::Models => vec![text(v, "id")],
        }
    }
}

/// Render a list as a table, or a short message when it is empty.
pub fn render_list(kind: Kind, items: &[Value]) -> String {
    if items.is_empty() {
        return format!("No {}.\n", kind.noun());
    }
    let mut t = Table::new(kind.headers());
    for item in items {
        t.row(kind.cells(item));
    }
    t.render()
}

/// Preferred key order for an instance object.
pub const INSTANCE_KEYS: &[&str] = &[
    "id",
    "name",
    "status",
    "status_reason",
    "url",
    "template",
    "template_name",
    "template_revision",
    "image_ref",
    "type",
    "resources",
    "region",
    "runtime",
    "network",
    "allowed_domains",
    "auto_sleep",
    "idle_timeout_seconds",
    "default_port",
    "mount_path",
    "agent_api",
    "parent_id",
    "workspace_id",
    "env",
    "metadata",
    "disk_used_bytes",
];
/// Preferred key order for a template object.
pub const TEMPLATE_KEYS: &[&str] = &[
    "name",
    "scope",
    "revision",
    "image_ref",
    "image_digest",
    "default_port",
    "mount_path",
    "agent",
    "description",
    "types",
    "default_shape",
    "env",
];
/// Preferred key order for a workspace object.
pub const WORKSPACE_KEYS: &[&str] = &["id", "name", "awake_count", "awake_limit", "created_at"];

/// Key/value listing of one object: `preferred` keys first (in that order), the
/// rest alphabetically; nested objects are flattened with dots, arrays printed
/// as compact JSON. Money, byte and timestamp fields get a readable form.
pub fn render_object(v: &Value, preferred: &[&str]) -> String {
    let Value::Object(map) = v else {
        return format!("{}\n", scalar(v));
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    flatten(map, "", &mut pairs);
    let rank = |k: &str| {
        let top = k.split('.').next().unwrap_or(k);
        preferred
            .iter()
            .position(|p| *p == top)
            .unwrap_or(preferred.len())
    };
    pairs.sort_by_key(|(k, _)| rank(k));
    if pairs.is_empty() {
        return "{}\n".to_string();
    }
    let width = pairs
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (k, val) in pairs {
        let pad = width - k.chars().count();
        out.push_str(&k);
        out.push(':');
        out.extend(std::iter::repeat_n(' ', pad + 2));
        out.push_str(&val);
        out.push('\n');
    }
    out
}

fn flatten(map: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, String)>) {
    for (k, v) in map {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Value::Object(inner) if !inner.is_empty() => flatten(inner, &key, out),
            Value::Object(_) => out.push((key, "{}".to_string())),
            other => {
                let shown = decorate(&key, other);
                out.push((key, shown));
            }
        }
    }
}

/// Numbers in `*_bytes`/`size` and `*_at`/`at` fields get a human hint; a null
/// `awake_limit` reads `unlimited`.
fn decorate(key: &str, v: &Value) -> String {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    if v.is_null() && leaf == "awake_limit" {
        return "unlimited".to_string();
    }
    match v.as_i64() {
        Some(n) if leaf.ends_with("_bytes") || leaf == "size" => {
            format!("{n} ({})", format_bytes(n))
        }
        Some(n) if n > 0 && (leaf.ends_with("_at") || leaf == "at") => {
            format!("{n} ({})", format_timestamp(n))
        }
        _ => scalar(v),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Null => "-".to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// A field as text; missing, null or empty is `-`.
fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        None | Some(Value::Null) => "-".to_string(),
        Some(Value::String(s)) if s.is_empty() => "-".to_string(),
        Some(other) => scalar(other),
    }
}

fn when(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_i64)
        .map(format_timestamp)
        .unwrap_or_else(|| "-".to_string())
}

fn bytes(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_i64)
        .map(format_bytes)
        .unwrap_or_else(|| "-".to_string())
}

fn shape(v: &Value) -> String {
    match (
        v.get("cpu").and_then(Value::as_u64),
        v.get("memory").and_then(Value::as_u64),
        v.get("disk").and_then(Value::as_u64),
    ) {
        (Some(c), Some(m), Some(d)) => format!("{c}v/{m}g/{d}d"),
        _ => "-".to_string(),
    }
}

trait IfDash {
    fn if_dash(self, f: impl FnOnce() -> String) -> String;
}

impl IfDash for String {
    fn if_dash(self, f: impl FnOnce() -> String) -> String {
        if self == "-" { f() } else { self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns_without_trailing_spaces() {
        let mut t = Table::new(["ID", "NAME"]);
        t.row(["ab12cd34ef", "x"]).row(["b", "longer name"]);
        assert_eq!(
            t.render(),
            "ID          NAME\nab12cd34ef  x\nb           longer name\n"
        );
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
        let mut short = Table::new(["A", "B", "C"]);
        short.row(["1"]);
        assert_eq!(short.render(), "A  B  C\n1\n");
    }

    #[test]
    fn instance_list() {
        let inst = json!({
            "id": "ab12cd34ef", "name": "Production agent", "status": "running", "template": "warren-agent",
            "template_name": "warren-agent", "type": "default", "resources": {"cpu": 2, "memory": 4, "disk": 4},
            "created_at": 1763000000
        });
        let out = render_list(Kind::Instances, &[inst]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("ID          NAME"));
        assert!(lines[1].contains("ab12cd34ef  Production agent  running  warren-agent  default  2    4    4     2025-11-13 02:13:20"));
        assert_eq!(render_list(Kind::Instances, &[]), "No instances.\n");
        assert_eq!(items_of(&json!({"data": [1, 2]})).len(), 2);
        assert_eq!(items_of(&json!([1])).len(), 1);
        assert_eq!(items_of(&json!({"id": "x"})).len(), 1);
        assert_eq!(list_envelope(vec![json!(1)]), json!({"data": [1]}));
    }

    #[test]
    fn other_lists_render_every_column() {
        let tpl = json!({"name": "warren-base", "scope": "system", "revision": 1, "image_ref": "warren/base:dev", "default_port": 3737,
            "agent": false, "default_shape": {"cpu": 2, "memory": 4, "disk": 4}, "description": "A plain Linux computer"});
        assert!(render_list(Kind::Templates, &[tpl]).contains("warren-base  system  1    warren/base:dev  3737  false  2v/4g/4d  A plain Linux computer"));
        let key =
            json!({"id": "k1", "name": "ci", "prefix": "sk_live_abcdef", "created_at": 1763000000});
        let out = render_list(Kind::Keys, &[key]);
        assert!(out.contains("never"));
        let metric = json!({"at": 1763000000, "cpu_percent": 12.345, "memory_bytes": 1048576, "disk_bytes": 2048});
        assert!(render_list(Kind::Metrics, &[metric]).contains("12.3   1.0 MB  2.0 KB"));
        let file = json!({"name": "notes.txt", "type": "file", "size": 10, "modified_at": 1763000000000i64});
        assert!(
            render_list(Kind::Files, &[file])
                .contains("file  10 B  2025-11-13 02:13:20  notes.txt")
        );
        let cron = json!({"id": "c1", "name": "", "schedule": "0 9 * * *", "timezone": "", "enabled": true, "prompt": "Summarize the inbox"});
        let out = render_list(Kind::Crons, &[cron]);
        assert!(out.contains("UTC"));
        assert!(out.contains("Summarize the inbox"));
        let backup = json!({"id": "b1", "size_bytes": 1536, "created_at": 1763000000, "note": ""});
        assert!(render_list(Kind::Backups, &[backup]).contains("b1  1.5 KB  2025-11-13 02:13:20"));
    }

    #[test]
    fn object_rendering_flattens_and_decorates() {
        let v = json!({"template_name": "warren-base", "id": "ws_x", "nested": {"a": 1, "b": {"c": true}}, "list": [1, 2], "empty": {}, "created_at": 1763000000, "size_bytes": 2048, "awake_limit": null});
        let out = render_object(&v, &["id", "template_name"]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "id:             ws_x");
        assert_eq!(lines[1], "template_name:  warren-base");
        assert!(lines.contains(&"awake_limit:    unlimited"));
        assert!(lines.contains(&"nested.b.c:     true"));
        assert!(lines.contains(&"list:           [1,2]"));
        assert!(lines.contains(&"empty:          {}"));
        assert!(lines.contains(&"created_at:     1763000000 (2025-11-13 02:13:20)"));
        assert!(lines.contains(&"size_bytes:     2048 (2.0 KB)"));
        assert_eq!(render_object(&json!("plain"), &[]), "plain\n");
        assert_eq!(render_object(&json!({}), &[]), "{}\n");
    }
}
