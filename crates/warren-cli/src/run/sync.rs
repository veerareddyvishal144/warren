//! `warren sync push|pull` and the workspace synchronisation `warren run`
//! uses: the current directory goes to an instance as one tarball and the
//! files that changed there come back the same way.
//!
//! Push: every file under the current directory that `.gitignore` (and
//! `.ignore`, the global excludes) does not ignore, minus the default
//! excludes ([`DEFAULT_EXCLUDES`]) and `--exclude` names at any depth, with
//! `.git` included, as a gzip tarball uploaded with the files API to
//! `~/.warren-sync/push.tgz` and unpacked by exec into the remote root
//! (`tar -xzm`, so the extracted files carry the extraction time); then a
//! marker file is touched.
//!
//! Pull: exec lists the regular files under the remote root newer than the
//! marker (same excludes), archives them, the archive is fetched with the
//! files API and unpacked into the current directory; then the marker moves
//! to the time the listing started so nothing written during the pull is
//! missed next time.
//!
//! The planning functions below are pure and unit-tested; the two `async`
//! functions at the end run the plan against the API.

use std::{
    collections::BTreeSet,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use warren_cli::{
    EXIT_OK, Failure,
    cli::{SyncArgs, SyncCmd},
    util::{format_bytes, shell_quote},
};
use warren_core::{ExecRequest, ExecResult};

use super::{Ctx, api::Api};

/// Directory names never synced, at any depth, in either direction.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "__pycache__",
    ".cache",
    ".warren",
];
/// Where the binding of a directory to an instance is kept (`warren run`).
pub const BINDING_FILE: &str = ".warren/instance.json";
/// State directory inside the remote home.
pub const REMOTE_STATE_DIR: &str = ".warren-sync";
/// Uploaded push archive, relative to the remote home.
pub const PUSH_ARCHIVE: &str = ".warren-sync/push.tgz";
/// Archive of changed files built by pull, relative to the remote home.
pub const PULL_ARCHIVE: &str = ".warren-sync/pull.tgz";
/// List of changed files built by pull, relative to the remote home.
pub const PULL_LIST: &str = ".warren-sync/pull.list";
/// The sync marker: files newer than it are pulled.
pub const MARKER: &str = ".warren-sync/marker";
/// Timeout for the remote `tar` and `find` commands.
const REMOTE_TIMEOUT_SEC: u32 = 600;

/// The binding of a local directory to an instance, stored in
/// [`BINDING_FILE`] by `warren run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// Instance id.
    pub id: String,
    /// Remote directory the local directory maps to.
    pub remote_root: String,
    /// Unix seconds when the binding was created.
    pub created_at: i64,
}

impl Binding {
    /// Path of the binding file for `dir`.
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(BINDING_FILE)
    }

    /// Read the binding of `dir`; `None` when there is none.
    pub fn load(dir: &Path) -> Result<Option<Binding>, Failure> {
        let path = Self::path(dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Failure::local(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Failure::local(format!("{}: {e}", path.display()))),
        }
    }

    /// Write the binding of `dir`, creating `.warren/`.
    pub fn save(&self, dir: &Path) -> Result<(), Failure> {
        let path = Self::path(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Failure::local(format!("{}: {e}", parent.display())))?;
        }
        let body = format!(
            "{}\n",
            serde_json::to_string_pretty(self).map_err(|e| Failure::local(e.to_string()))?
        );
        std::fs::write(&path, body).map_err(|e| Failure::local(format!("{}: {e}", path.display())))
    }

    /// Remove the binding of `dir` (and an empty `.warren/`). Returns
    /// whether there was one.
    pub fn remove(dir: &Path) -> Result<bool, Failure> {
        let path = Self::path(dir);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::remove_dir(parent); // only when empty
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Failure::local(format!("{}: {e}", path.display()))),
        }
    }

    /// A binding created now.
    pub fn new(id: &str, remote_root: &str) -> Binding {
        Binding {
            id: id.to_string(),
            remote_root: remote_root.to_string(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }
    }
}

// ----- planning (pure) -------------------------------------------------------------

/// The default excludes followed by `extra`, trimmed, without duplicates.
pub fn excludes(extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in DEFAULT_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().map(|s| s.trim().to_string()))
    {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Name of the local directory as used in the remote root; `workspace`
/// when the path has no final component.
pub fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "workspace".to_string())
}

/// `<mount_path>/work/<dir_name>`; `work/<dir_name>` (relative to the home,
/// where exec and the terminal start) when the mount path is unknown.
pub fn default_remote_root(mount_path: &str, dir_name: &str) -> String {
    let mount = mount_path.trim().trim_end_matches('/');
    if mount.is_empty() {
        format!("work/{dir_name}")
    } else {
        format!("{mount}/work/{dir_name}")
    }
}

/// Files under `root` to push: relative paths, sorted, honouring
/// `.gitignore`, `.ignore` and the global git excludes, minus `excludes`
/// (matched against every path component). Dot-files and `.git` are
/// included. Directories themselves are not listed; `tar` recreates them.
pub fn local_manifest(root: &Path, excludes: &[String]) -> Result<Vec<PathBuf>, Failure> {
    let names: BTreeSet<String> = excludes.iter().cloned().collect();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false);
    builder.filter_entry(move |entry| {
        entry.depth() == 0
            || !entry
                .file_name()
                .to_str()
                .is_some_and(|n| names.contains(n))
    });
    let mut files = Vec::new();
    for entry in builder.build() {
        let entry =
            entry.map_err(|e| Failure::local(format!("scanning {}: {e}", root.display())))?;
        if entry.depth() == 0 || entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| Failure::local(format!("{}: {e}", entry.path().display())))?
            .to_path_buf();
        files.push(rel);
    }
    files.sort();
    Ok(files)
}

/// A gzip tarball of `files` (relative to `root`) with the same relative
/// names inside; symlinks are stored as links.
pub fn archive(root: &Path, files: &[PathBuf]) -> Result<Vec<u8>, Failure> {
    let enc = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);
    for rel in files {
        tar.append_path_with_name(root.join(rel), rel)
            .map_err(|e| Failure::local(format!("adding {}: {e}", rel.display())))?;
    }
    let enc = tar
        .into_inner()
        .map_err(|e| Failure::local(format!("building the archive: {e}")))?;
    enc.finish()
        .map_err(|e| Failure::local(format!("compressing the archive: {e}")))
}

/// Unpack a gzip tarball into `into`, refusing paths that escape it.
/// Returns the extracted paths, relative to `into`.
pub fn unpack(bytes: &[u8], into: &Path) -> Result<Vec<PathBuf>, Failure> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    let mut out = Vec::new();
    let entries = archive
        .entries()
        .map_err(|e| Failure::local(format!("reading the archive: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| Failure::local(format!("reading the archive: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| Failure::local(format!("reading the archive: {e}")))?
            .into_owned();
        let unpacked = entry
            .unpack_in(into)
            .map_err(|e| Failure::local(format!("extracting {}: {e}", path.display())))?;
        if unpacked && !entry.header().entry_type().is_dir() {
            out.push(clean_relative(&path));
        }
    }
    Ok(out)
}

/// Drop leading `./` components.
fn clean_relative(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// A quoted `"$HOME/<rel>"` reference for the remote shell.
fn home_path(rel: &str) -> String {
    format!("\"$HOME/{rel}\"")
}

/// The exec command that unpacks the pushed archive into `remote_root` and
/// touches the marker.
pub fn extract_command(remote_root: &str) -> String {
    let root = shell_quote(remote_root);
    format!(
        "mkdir -p {root} && tar -xzmf {archive} -C {root} && rm -f {archive} && touch {marker}",
        archive = home_path(PUSH_ARCHIVE),
        marker = home_path(MARKER),
    )
}

/// The exec command that records the start of a pull, lists the regular
/// files under `remote_root` newer than the marker (minus `excludes`) into
/// the pull list and prints that list. Runs even before the first push: a
/// missing marker is created in the past so everything counts as changed.
pub fn pull_list_command(remote_root: &str, excludes: &[String]) -> String {
    let prune = if excludes.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = excludes
            .iter()
            .map(|n| format!("-name {}", shell_quote(n)))
            .collect();
        format!("\\( {} \\) -prune -o ", names.join(" -o "))
    };
    format!(
        "mkdir -p {dir} && {{ [ -e {marker} ] || touch -t 197001020000 {marker}; }} && touch {next} && cd {root} && find . {prune}-type f -newer {marker} -print > {list} && cat {list}",
        dir = home_path(REMOTE_STATE_DIR),
        marker = home_path(MARKER),
        next = home_path(&format!("{MARKER}.next")),
        root = shell_quote(remote_root),
        list = home_path(PULL_LIST),
    )
}

/// The exec command that archives the listed files from `remote_root`.
pub fn pull_archive_command(remote_root: &str) -> String {
    format!(
        "cd {root} && tar -czf {archive} -T {list}",
        root = shell_quote(remote_root),
        archive = home_path(PULL_ARCHIVE),
        list = home_path(PULL_LIST),
    )
}

/// The exec command that moves the marker to the start of the pull and
/// removes the pull files.
pub fn pull_finish_command() -> String {
    format!(
        "mv -f {next} {marker} && rm -f {archive} {list}",
        next = home_path(&format!("{MARKER}.next")),
        marker = home_path(MARKER),
        archive = home_path(PULL_ARCHIVE),
        list = home_path(PULL_LIST),
    )
}

/// The file names printed by the pull listing, without the `./` prefix.
pub fn parse_pull_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.strip_prefix("./").unwrap_or(l).to_string())
        .collect()
}

// ----- running the plan -----------------------------------------------------------

/// What a push moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushReport {
    /// Files in the archive, relative to the local directory.
    pub files: Vec<PathBuf>,
    /// Size of the compressed archive.
    pub bytes: usize,
}

async fn remote(api: &Api, id: &str, command: &str) -> Result<ExecResult, Failure> {
    let req = ExecRequest {
        command: command.to_string(),
        timeout_seconds: REMOTE_TIMEOUT_SEC,
        ..Default::default()
    };
    let r = api.exec(id, &req).await?;
    if r.exit_code != 0 {
        let detail = if r.stderr.trim().is_empty() {
            r.stdout.trim().to_string()
        } else {
            r.stderr.trim().to_string()
        };
        return Err(Failure::local(format!(
            "remote command failed (exit {}): {detail}",
            r.exit_code
        )));
    }
    Ok(r)
}

/// Push `local` to `remote_root` on instance `id`.
pub async fn push(
    api: &Api,
    id: &str,
    local: &Path,
    remote_root: &str,
    excludes: &[String],
) -> Result<PushReport, Failure> {
    let files = local_manifest(local, excludes)?;
    let bytes = archive(local, &files)?;
    let size = bytes.len();
    api.files_write(id, PUSH_ARCHIVE, bytes).await?;
    remote(api, id, &extract_command(remote_root)).await?;
    Ok(PushReport { files, bytes: size })
}

/// Pull the files changed under `remote_root` on instance `id` since the
/// last sync into `local`. Returns the extracted paths.
pub async fn pull(
    api: &Api,
    id: &str,
    local: &Path,
    remote_root: &str,
    excludes: &[String],
) -> Result<Vec<PathBuf>, Failure> {
    let listed = remote(api, id, &pull_list_command(remote_root, excludes)).await?;
    let changed = parse_pull_list(&listed.stdout);
    let mut extracted = Vec::new();
    if !changed.is_empty() {
        remote(api, id, &pull_archive_command(remote_root)).await?;
        let bytes = api.files_read(id, PULL_ARCHIVE).await?;
        extracted = unpack(&bytes, local)?;
    }
    remote(api, id, &pull_finish_command()).await?;
    Ok(extracted)
}

/// The instance and remote root for a sync: the id given or the bound one;
/// the root given, else the bound one, else the default under the
/// instance's mount path.
pub async fn resolve_target(
    api: &Api,
    cwd: &Path,
    id: Option<&str>,
    remote_root: Option<&str>,
) -> Result<(String, String), Failure> {
    let binding = Binding::load(cwd)?;
    let id = match id {
        Some(i) => i.to_string(),
        None => binding.as_ref().map(|b| b.id.clone()).ok_or_else(|| {
            Failure::usage(
                "no instance id given and this directory is not bound to one: pass an id or run 'warren run' first",
            )
        })?,
    };
    if let Some(root) = remote_root {
        return Ok((id, root.to_string()));
    }
    if let Some(b) = binding.filter(|b| b.id == id) {
        return Ok((id, b.remote_root));
    }
    let inst = api.get_instance(&id).await?;
    let mount = inst
        .get("mount_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let root = default_remote_root(mount, &dir_name(cwd));
    Ok((id, root))
}

/// `warren sync push|pull [ID] [--remote-root PATH] [--exclude NAME]...`
pub async fn run(ctx: &Ctx, cmd: SyncCmd) -> Result<i32, Failure> {
    let api = ctx.api()?;
    let cwd =
        std::env::current_dir().map_err(|e| Failure::local(format!("current directory: {e}")))?;
    let (args, pushing): (SyncArgs, bool) = match cmd {
        SyncCmd::Push(a) => (a, true),
        SyncCmd::Pull(a) => (a, false),
    };
    let (id, remote_root) =
        resolve_target(&api, &cwd, args.id.as_deref(), args.remote_root.as_deref()).await?;
    let ex = excludes(&args.exclude);
    if pushing {
        let report = push(&api, &id, &cwd, &remote_root, &ex).await?;
        let v = json!({
            "id": id,
            "remote_root": remote_root,
            "files": report.files,
            "bytes": report.bytes,
        });
        return ctx.emit(&v, || {
            format!(
                "pushed {} files ({}) to {id}:{remote_root}\n",
                report.files.len(),
                format_bytes(report.bytes as i64)
            )
        });
    }
    let changed = pull(&api, &id, &cwd, &remote_root, &ex).await?;
    if ctx.json {
        let v = json!({ "id": id, "remote_root": remote_root, "files": changed });
        println!("{}", warren_cli::output::to_json_pretty(&v));
    } else {
        let mut out = std::io::stdout().lock();
        for f in &changed {
            writeln!(out, "{}", f.display())?;
        }
        out.flush()?;
        eprintln!(
            "pulled {} changed files from {id}:{remote_root}",
            changed.len()
        );
    }
    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "warren-sync-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn strings(paths: &[PathBuf]) -> Vec<String> {
        paths.iter().map(|p| p.display().to_string()).collect()
    }

    #[test]
    fn excludes_and_roots() {
        let ex = excludes(&["vendor".to_string(), " target ".to_string(), "".to_string()]);
        assert_eq!(ex.len(), DEFAULT_EXCLUDES.len() + 1);
        assert_eq!(ex.last().map(String::as_str), Some("vendor"));
        assert!(ex.contains(&".warren".to_string()));
        assert_eq!(excludes(&[]), DEFAULT_EXCLUDES);
        assert_eq!(
            default_remote_root("/home/agent", "myapp"),
            "/home/agent/work/myapp"
        );
        assert_eq!(
            default_remote_root("/Users/lume/", "myapp"),
            "/Users/lume/work/myapp"
        );
        assert_eq!(default_remote_root("", "myapp"), "work/myapp");
        assert_eq!(dir_name(Path::new("/tmp/proj")), "proj");
        assert_eq!(dir_name(Path::new("/")), "workspace");
    }

    #[test]
    fn commands_are_quoted() {
        let root = "/home/agent/work/my app's";
        let q = "'/home/agent/work/my app'\\''s'";
        assert_eq!(
            extract_command(root),
            format!(
                "mkdir -p {q} && tar -xzmf \"$HOME/.warren-sync/push.tgz\" -C {q} && rm -f \"$HOME/.warren-sync/push.tgz\" && touch \"$HOME/.warren-sync/marker\""
            )
        );
        let list = pull_list_command(root, &["node_modules".to_string(), "a b".to_string()]);
        assert_eq!(
            list,
            format!(
                "mkdir -p \"$HOME/.warren-sync\" && {{ [ -e \"$HOME/.warren-sync/marker\" ] || touch -t 197001020000 \"$HOME/.warren-sync/marker\"; }} && touch \"$HOME/.warren-sync/marker.next\" && cd {q} && find . \\( -name node_modules -o -name 'a b' \\) -prune -o -type f -newer \"$HOME/.warren-sync/marker\" -print > \"$HOME/.warren-sync/pull.list\" && cat \"$HOME/.warren-sync/pull.list\""
            )
        );
        let plain = pull_list_command("/home/agent/work/app", &[]);
        assert!(plain.contains("cd /home/agent/work/app && find . -type f -newer"));
        assert!(!plain.contains("-prune"));
        assert_eq!(
            pull_archive_command(root),
            format!(
                "cd {q} && tar -czf \"$HOME/.warren-sync/pull.tgz\" -T \"$HOME/.warren-sync/pull.list\""
            )
        );
        assert_eq!(
            pull_finish_command(),
            "mv -f \"$HOME/.warren-sync/marker.next\" \"$HOME/.warren-sync/marker\" && rm -f \"$HOME/.warren-sync/pull.tgz\" \"$HOME/.warren-sync/pull.list\""
        );
        assert_eq!(
            parse_pull_list("./src/main.rs\n./README.md\n\n./a b.txt\r\n"),
            vec!["src/main.rs", "README.md", "a b.txt"]
        );
        assert!(parse_pull_list("").is_empty());
    }

    #[test]
    fn manifest_honours_gitignore_and_excludes_but_keeps_git() {
        let dir = temp_dir("manifest");
        write(&dir, ".gitignore", "*.log\nsecret/\n");
        write(&dir, "src/main.rs", "fn main() {}");
        write(&dir, "README.md", "# hi");
        write(&dir, ".env", "X=1");
        write(&dir, "debug.log", "noise");
        write(&dir, "secret/key", "k");
        write(&dir, "node_modules/left-pad/index.js", "x");
        write(&dir, "target/debug/app", "bin");
        write(&dir, "sub/target/keep.txt", "nested exclude");
        write(&dir, "vendor/lib.rs", "v");
        write(&dir, ".git/HEAD", "ref: refs/heads/main");
        write(&dir, ".git/objects/ab/cd", "blob");
        write(&dir, ".warren/instance.json", "{}");
        let files = local_manifest(&dir, &excludes(&["vendor".to_string()])).unwrap();
        assert_eq!(
            strings(&files),
            vec![
                ".env",
                ".git/HEAD",
                ".git/objects/ab/cd",
                ".gitignore",
                "README.md",
                "src/main.rs",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn archive_round_trip() {
        let src = temp_dir("archive-src");
        write(&src, "a/b.txt", "bee");
        write(&src, "c.txt", "sea");
        #[cfg(unix)]
        std::os::unix::fs::symlink("c.txt", src.join("link")).unwrap();
        let files = local_manifest(&src, &excludes(&[])).unwrap();
        assert_eq!(strings(&files), vec!["a/b.txt", "c.txt", "link"]);
        let bytes = archive(&src, &files).unwrap();
        assert!(bytes.len() > 20);
        let dst = temp_dir("archive-dst");
        let extracted = unpack(&bytes, &dst).unwrap();
        assert_eq!(strings(&extracted), vec!["a/b.txt", "c.txt", "link"]);
        assert_eq!(std::fs::read_to_string(dst.join("a/b.txt")).unwrap(), "bee");
        assert_eq!(std::fs::read_to_string(dst.join("link")).unwrap(), "sea");
        assert!(
            std::fs::symlink_metadata(dst.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // Pull archives name entries `./x`; the prefix is dropped.
        let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        tar.append_path_with_name(src.join("c.txt"), "./deep/c.txt")
            .unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        let extracted = unpack(&bytes, &dst).unwrap();
        assert_eq!(strings(&extracted), vec!["deep/c.txt"]);
        assert_eq!(
            std::fs::read_to_string(dst.join("deep/c.txt")).unwrap(),
            "sea"
        );
        std::fs::remove_dir_all(&src).unwrap();
        std::fs::remove_dir_all(&dst).unwrap();
    }

    #[test]
    fn binding_round_trip() {
        let dir = temp_dir("binding");
        assert_eq!(Binding::load(&dir).unwrap(), None);
        let b = Binding::new("ab12cd34ef", "/home/agent/work/proj");
        assert!(b.created_at > 0);
        b.save(&dir).unwrap();
        assert_eq!(Binding::load(&dir).unwrap(), Some(b.clone()));
        let text = std::fs::read_to_string(dir.join(BINDING_FILE)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["id"], "ab12cd34ef");
        assert_eq!(v["remote_root"], "/home/agent/work/proj");
        assert!(Binding::remove(&dir).unwrap());
        assert!(!dir.join(".warren").exists());
        assert!(!Binding::remove(&dir).unwrap());
        std::fs::write(dir.join(".warren"), "not a dir").ok();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
