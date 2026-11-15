//! The files API: every path is relative to the persistent home directory and
//! confined to it. Resolution canonicalizes the deepest existing ancestor of
//! the requested path and requires the result to start with the canonical
//! home, so `..`, absolute paths and symlinks pointing outside are all
//! rejected with `forbidden`.
//!
//! Every file and directory created here is handed to the [`HomeOwner`]
//! (see `owner.rs`), so a root gateway does not leave root-owned files in a
//! home that belongs to the agent user on the host.

use std::{
    ffi::OsString,
    io::Write,
    path::{Component, Path, PathBuf},
};

use axum::{
    Json,
    body::Body,
    extract::{RawQuery, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use flate2::{Compression, write::GzEncoder};
use futures::StreamExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use warren_core::{
    Error, Result,
    agentapi::{DirListing, FileEntry, MoveRequest},
};

use crate::{
    extract::{JsonBody, required_query},
    owner::HomeOwner,
    server::SharedState,
};

/// Largest accepted upload through `PUT /v1/files/content`.
pub const MAX_UPLOAD_BYTES: u64 = 2 << 30;
/// Bytes sniffed to decide whether an unknown file type is text.
const SNIFF_BYTES: usize = 8 * 1024;

/// A canonical home directory with confined path resolution. Everything it
/// creates belongs to the home owner.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
    owner: HomeOwner,
}

/// Resolve `.` and `..` lexically. Returns `None` when `..` climbs above the
/// first component of a relative path.
pub fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) => return None,
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            Component::Normal(n) => out.push(n),
        }
    }
    Some(out)
}

fn outside() -> Error {
    Error::forbidden("path is outside the home directory")
}

impl Home {
    /// Create the directory when missing, canonicalize it and resolve its
    /// owner (see [`HomeOwner::detect`]).
    pub fn open(path: &Path) -> Result<Home> {
        std::fs::create_dir_all(path)?;
        let root = std::fs::canonicalize(path)?;
        let owner = HomeOwner::detect(&root);
        Ok(Home { root, owner })
    }

    /// Like [`Home::open`] with an owner resolved earlier.
    pub fn open_as(path: &Path, owner: HomeOwner) -> Result<Home> {
        std::fs::create_dir_all(path)?;
        Ok(Home {
            root: std::fs::canonicalize(path)?,
            owner,
        })
    }

    /// The canonical home directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Who created files and directories are handed to.
    pub fn owner(&self) -> &HomeOwner {
        &self.owner
    }

    /// Confine a client-supplied path. The result is absolute, canonical in
    /// its existing part and guaranteed to be inside the home; it may not
    /// exist yet.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf> {
        let requested = requested.trim();
        if requested.contains('\0') {
            return Err(Error::invalid("path contains a NUL byte"));
        }
        let joined = if requested.is_empty() || requested == "." {
            self.root.clone()
        } else if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.root.join(requested)
        };
        let normalized = lexical_normalize(&joined).ok_or_else(outside)?;
        if !joined.is_absolute() {
            return Err(outside());
        }
        // Walk up to the deepest existing ancestor without following a final symlink.
        let mut existing = normalized.as_path();
        let mut rest: Vec<OsString> = Vec::new();
        loop {
            match std::fs::symlink_metadata(existing) {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let name = existing.file_name().ok_or_else(outside)?.to_os_string();
                    rest.push(name);
                    existing = existing.parent().ok_or_else(outside)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(outside()),
                Err(e) => return Err(e.into()),
            }
        }
        let canonical = match std::fs::canonicalize(existing) {
            Ok(c) => c,
            // A dangling symlink: never write through it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::forbidden("path goes through a dangling symlink"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(outside()),
            Err(e) => return Err(e.into()),
        };
        if !canonical.starts_with(&self.root) {
            return Err(outside());
        }
        let mut out = canonical;
        for name in rest.iter().rev() {
            out.push(name);
        }
        Ok(out)
    }

    /// Display form of a confined path: relative to the home, `.` for the home.
    pub fn display(&self, abs: &Path) -> String {
        match abs.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => abs.to_string_lossy().into_owned(),
        }
    }

    /// Resolve and require existence.
    pub fn resolve_existing(&self, requested: &str) -> Result<PathBuf> {
        let p = self.resolve(requested)?;
        if std::fs::symlink_metadata(&p).is_err() {
            return Err(Error::not_found("path"));
        }
        Ok(p)
    }

    /// `GET /v1/files?path=`: entries sorted by name.
    pub async fn list(&self, requested: &str) -> Result<DirListing> {
        let dir = self.resolve_existing(requested)?;
        let meta = tokio::fs::metadata(&dir).await?;
        if !meta.is_dir() {
            return Err(Error::invalid(format!(
                "{} is not a directory",
                self.display(&dir)
            )));
        }
        let mut entries = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Follow symlinks for type and size; a broken link is reported as a file.
            let meta = match tokio::fs::metadata(entry.path()).await {
                Ok(m) => Some(m),
                Err(_) => entry.metadata().await.ok(),
            };
            let (kind, size, modified_at) = match meta {
                Some(m) => (
                    if m.is_dir() { "dir" } else { "file" },
                    if m.is_dir() { 0 } else { m.len() },
                    modified_ms(&m),
                ),
                None => ("file", 0, 0),
            };
            entries.push(FileEntry {
                name,
                kind: kind.to_string(),
                size,
                modified_at,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(DirListing {
            path: self.display(&dir),
            entries,
        })
    }

    /// Open a file for `GET /v1/files/content`: the handle, its size and a content type.
    pub async fn open_file(&self, requested: &str) -> Result<(tokio::fs::File, u64, String)> {
        let path = self.resolve_existing(requested)?;
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|_| Error::not_found("file"))?;
        if meta.is_dir() {
            return Err(Error::invalid(format!(
                "{} is a directory",
                self.display(&path)
            )));
        }
        let mut file = tokio::fs::File::open(&path).await?;
        let mut content_type = mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string();
        if content_type == "application/octet-stream" {
            let mut head = vec![0u8; SNIFF_BYTES.min(meta.len() as usize)];
            file.read_exact(&mut head).await?;
            if looks_like_text(&head) {
                content_type = "text/plain; charset=utf-8".to_string();
            }
            file.seek(std::io::SeekFrom::Start(0)).await?;
        }
        Ok((file, meta.len(), content_type))
    }

    /// Target for `PUT /v1/files/content`: missing parents are created (and
    /// handed to the owner); directories are refused.
    pub async fn prepare_write(&self, requested: &str) -> Result<PathBuf> {
        let path = self.resolve(requested)?;
        if path == self.root {
            return Err(Error::invalid("path must name a file"));
        }
        if let Ok(m) = tokio::fs::metadata(&path).await
            && m.is_dir()
        {
            return Err(Error::conflict(format!(
                "{} is a directory",
                self.display(&path)
            )));
        }
        if let Some(parent) = path.parent() {
            self.owner.create_dir_all_owned(parent)?;
        }
        Ok(path)
    }

    /// Write a whole file (used by the drivers' `write_file` tool). The file
    /// ends up owned by the home owner.
    pub async fn write_file(&self, requested: &str, bytes: &[u8]) -> Result<(String, u64)> {
        let path = self.prepare_write(requested).await?;
        tokio::fs::write(&path, bytes).await?;
        self.owner.own(&path);
        Ok((self.display(&path), bytes.len() as u64))
    }

    /// Read a whole file, capped at `cap` bytes (used by the drivers' `read_file` tool).
    pub async fn read_file(&self, requested: &str, cap: usize) -> Result<(Vec<u8>, bool)> {
        let (mut file, size, _) = self.open_file(requested).await?;
        let mut buf = Vec::with_capacity((size as usize).min(cap));
        let mut handle = (&mut file).take(cap as u64);
        handle.read_to_end(&mut buf).await?;
        Ok((buf, size as usize > cap))
    }

    /// `DELETE /v1/files?path=`: files and directories (recursively). The home itself is protected.
    pub async fn delete(&self, requested: &str) -> Result<String> {
        let path = self.resolve_existing(requested)?;
        if path == self.root {
            return Err(Error::forbidden(
                "the home directory itself cannot be deleted",
            ));
        }
        let meta = tokio::fs::symlink_metadata(&path).await?;
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(self.display(&path))
    }

    /// `POST /v1/files/move`: rename within the home, creating the target's
    /// parents (handed to the owner). The moved entry keeps its owner.
    pub async fn rename(&self, from: &str, to: &str) -> Result<(String, String)> {
        let src = self.resolve_existing(from)?;
        let dst = self.resolve(to)?;
        if src == self.root || dst == self.root {
            return Err(Error::forbidden(
                "the home directory itself cannot be moved",
            ));
        }
        if dst.starts_with(&src) && dst != src {
            return Err(Error::invalid("cannot move a directory into itself"));
        }
        if let Ok(m) = tokio::fs::metadata(&dst).await
            && m.is_dir()
        {
            return Err(Error::conflict(format!(
                "{} is an existing directory",
                self.display(&dst)
            )));
        }
        if let Some(parent) = dst.parent() {
            self.owner.create_dir_all_owned(parent)?;
        }
        tokio::fs::rename(&src, &dst).await?;
        Ok((self.display(&src), self.display(&dst)))
    }

    /// `GET /v1/files/download?path=`: a gzip tarball of a directory (or a
    /// single file) written to an unlinked temp file and returned for streaming.
    pub async fn archive(&self, requested: &str) -> Result<(tokio::fs::File, u64, String)> {
        let path = self.resolve_existing(requested)?;
        let display = self.display(&path);
        let base_name = if path == self.root {
            "home".to_string()
        } else {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "download".into())
        };
        let tmp = std::env::temp_dir().join(format!(
            "warren-download-{}.tar.gz",
            warren_core::ids::new_id()
        ));
        let (tmp_path, entry_name, src) = (tmp.clone(), base_name.clone(), path.clone());
        let built = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let file = std::fs::File::create(&tmp_path)?;
            let enc = GzEncoder::new(std::io::BufWriter::new(file), Compression::default());
            let mut tar = tar::Builder::new(enc);
            tar.follow_symlinks(false);
            if src.is_dir() {
                tar.append_dir_all(&entry_name, &src)?;
            } else {
                tar.append_path_with_name(&src, &entry_name)?;
            }
            let enc = tar.into_inner()?;
            let mut writer = enc.finish()?;
            writer.flush()?;
            Ok(())
        })
        .await
        .map_err(|e| Error::internal(format!("archive task failed: {e}")))?;
        if let Err(e) = built {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::internal(format!("cannot archive {display}: {e}")));
        }
        let file = tokio::fs::File::open(&tmp).await?;
        let size = file.metadata().await?.len();
        // Unlink now; the open handle keeps the data until the stream ends.
        let _ = tokio::fs::remove_file(&tmp).await;
        Ok((file, size, format!("{base_name}.tar.gz")))
    }
}

fn modified_ms(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Heuristic: valid UTF-8 without NUL bytes.
pub fn looks_like_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // A multi-byte sequence may be cut at the end of the sample.
        Err(e) => e.error_len().is_none(),
    }
}

// ----- handlers -----

/// `GET /v1/files?path=.`
pub async fn list_handler(
    State(state): State<SharedState>,
    RawQuery(q): RawQuery,
) -> Result<Json<DirListing>> {
    let path =
        crate::extract::query_param(q.as_deref(), "path")?.unwrap_or_else(|| ".".to_string());
    Ok(Json(state.files.list(&path).await?))
}

/// `GET /v1/files/content?path=`
pub async fn content_get_handler(
    State(state): State<SharedState>,
    RawQuery(q): RawQuery,
) -> Result<Response> {
    let path = required_query(q.as_deref(), "path")?;
    let (file, size, content_type) = state.files.open_file(&path).await?;
    let body = Body::from_stream(ReaderStream::new(file));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, size)
        .body(body)
        .map_err(|e| Error::internal(e.to_string()))
}

/// `PUT /v1/files/content?path=` with the raw body, streamed to a sibling
/// temp file that is handed to the home owner and renamed into place.
pub async fn content_put_handler(
    State(state): State<SharedState>,
    RawQuery(q): RawQuery,
    body: Body,
) -> Result<Json<serde_json::Value>> {
    let path = required_query(q.as_deref(), "path")?;
    let target = state.files.prepare_write(&path).await?;
    let tmp = target.with_extension(format!("part-{}", warren_core::ids::new_id()));
    let result = write_stream(&tmp, body).await;
    let size = match result {
        Ok(size) => size,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    state.files.owner().own(&tmp);
    if let Err(e) = tokio::fs::rename(&tmp, &target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(Json(
        json!({ "path": state.files.display(&target), "size": size }),
    ))
}

async fn write_stream(tmp: &Path, body: Body) -> Result<u64> {
    let mut file = tokio::fs::File::create(tmp).await?;
    let mut stream = body.into_data_stream();
    let mut size: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| Error::invalid(format!("error reading request body: {e}")))?;
        size += chunk.len() as u64;
        if size > MAX_UPLOAD_BYTES {
            return Err(Error::invalid(format!(
                "upload exceeds {MAX_UPLOAD_BYTES} bytes"
            )));
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(size)
}

/// `DELETE /v1/files?path=`
pub async fn delete_handler(
    State(state): State<SharedState>,
    RawQuery(q): RawQuery,
) -> Result<Json<serde_json::Value>> {
    let path = required_query(q.as_deref(), "path")?;
    let deleted = state.files.delete(&path).await?;
    Ok(Json(json!({ "path": deleted, "deleted": true })))
}

/// `POST /v1/files/move` body `{ "from", "to" }`
pub async fn move_handler(
    State(state): State<SharedState>,
    JsonBody(body): JsonBody,
) -> Result<Json<serde_json::Value>> {
    let req: MoveRequest = crate::extract::parse_json(&body, &["from", "to"])?;
    if req.from.trim().is_empty() || req.to.trim().is_empty() {
        return Err(Error::invalid("from and to are required"));
    }
    let (from, to) = state.files.rename(&req.from, &req.to).await?;
    Ok(Json(
        json!({ "from": from, "to": to, "path": to, "moved": true }),
    ))
}

/// `GET /v1/files/download?path=`: `application/gzip` tarball.
pub async fn download_handler(
    State(state): State<SharedState>,
    RawQuery(q): RawQuery,
) -> Result<Response> {
    let path =
        crate::extract::query_param(q.as_deref(), "path")?.unwrap_or_else(|| ".".to_string());
    let (file, size, filename) = state.files.archive(&path).await?;
    let disposition = format!(
        "attachment; filename=\"{}\"",
        filename.replace(['"', '\\'], "_")
    );
    let mut resp = Body::from_stream(ReaderStream::new(file)).into_response();
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::is_root;

    fn temp_home() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("warren-gw-files-{}", warren_core::ids::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn normalize() {
        assert_eq!(
            lexical_normalize(Path::new("a/./b/../c")).unwrap(),
            PathBuf::from("a/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("/x/y/../z")).unwrap(),
            PathBuf::from("/x/z")
        );
        assert!(lexical_normalize(Path::new("a/../..")).is_none());
        assert!(lexical_normalize(Path::new("..")).is_none());
        assert!(lexical_normalize(Path::new("/..")).is_none());
    }

    #[test]
    fn resolution_confines_to_home() {
        let dir = temp_home();
        let home = Home::open(&dir).unwrap();
        let root = home.root().to_path_buf();
        assert_eq!(home.resolve("").unwrap(), root);
        assert_eq!(home.resolve(".").unwrap(), root);
        assert_eq!(home.resolve("a/b.txt").unwrap(), root.join("a/b.txt"));
        assert_eq!(home.resolve("./a/../c").unwrap(), root.join("c"));
        assert_eq!(
            home.resolve(root.join("abs.txt").to_str().unwrap())
                .unwrap(),
            root.join("abs.txt")
        );
        for bad in [
            "..",
            "../x",
            "a/../../x",
            "/etc/passwd",
            "/etc/definitely-missing-file",
            "/",
        ] {
            let e = home.resolve(bad).unwrap_err();
            assert_eq!(e.status, 403, "{bad} -> {e}");
        }
        // A symlink inside the home pointing outside is rejected, as is a dangling one.
        std::os::unix::fs::symlink("/etc", root.join("etc-link")).unwrap();
        assert_eq!(home.resolve("etc-link/passwd").unwrap_err().status, 403);
        assert_eq!(home.resolve("etc-link").unwrap_err().status, 403);
        std::os::unix::fs::symlink("/nonexistent/target", root.join("dangling")).unwrap();
        assert_eq!(home.resolve("dangling").unwrap_err().status, 403);
        // Missing paths resolve (for writes) but are not found for reads.
        assert_eq!(
            home.resolve_existing("missing.txt").unwrap_err().status,
            404
        );
        assert_eq!(home.display(&root.join("x/y")), "x/y");
        assert_eq!(home.display(&root), ".");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn owner_follows_the_directory() {
        let dir = temp_home();
        let home = Home::open(&dir).unwrap();
        // Without root the owner is the gateway's own user and nothing switches.
        if !is_root() {
            assert_eq!(home.owner(), &HomeOwner::current());
            assert!(!home.owner().switches());
        }
        let fake = HomeOwner {
            name: "agent".into(),
            uid: 1000,
            gid: 1000,
            named: true,
            process_uid: 0,
        };
        let as_fake = Home::open_as(&dir, fake.clone()).unwrap();
        assert_eq!(as_fake.owner(), &fake);
        assert_eq!(as_fake.root(), home.root());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn file_operations() {
        let dir = temp_home();
        let home = Home::open(&dir).unwrap();
        let (p, size) = home.write_file("docs/notes.txt", b"hello").await.unwrap();
        assert_eq!((p.as_str(), size), ("docs/notes.txt", 5));
        let (bytes, truncated) = home.read_file("docs/notes.txt", 3).await.unwrap();
        assert_eq!((bytes.as_slice(), truncated), (&b"hel"[..], true));
        let listing = home.list("docs").await.unwrap();
        assert_eq!(listing.path, "docs");
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "notes.txt");
        assert_eq!(listing.entries[0].kind, "file");
        assert_eq!(listing.entries[0].size, 5);
        assert!(listing.entries[0].modified_at > 0);
        assert_eq!(home.list("docs/notes.txt").await.unwrap_err().status, 400);
        assert_eq!(home.list("nope").await.unwrap_err().status, 404);
        let (_, size, ct) = home.open_file("docs/notes.txt").await.unwrap();
        assert_eq!(size, 5);
        assert!(ct.starts_with("text/plain"), "{ct}");
        assert_eq!(home.open_file("docs").await.unwrap_err().status, 400);
        assert_eq!(home.write_file("docs", b"x").await.unwrap_err().status, 409);
        assert_eq!(home.write_file(".", b"x").await.unwrap_err().status, 400);
        // Deep parents are created on the way; a file in the way is an error.
        let (p, _) = home.write_file("deep/er/still/f.txt", b"d").await.unwrap();
        assert_eq!(p, "deep/er/still/f.txt");
        assert!(root_join(&home, "deep/er/still").is_dir());
        assert!(
            home.write_file("docs/notes.txt/child.txt", b"x")
                .await
                .is_err()
        );

        let (from, to) = home
            .rename("docs/notes.txt", "archive/old.txt")
            .await
            .unwrap();
        assert_eq!(
            (from.as_str(), to.as_str()),
            ("docs/notes.txt", "archive/old.txt")
        );
        assert!(root_join(&home, "archive/old.txt").is_file());
        assert_eq!(
            home.rename("archive", "archive/sub")
                .await
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(home.rename(".", "x").await.unwrap_err().status, 403);

        let (file, size, name) = home.archive("archive").await.unwrap();
        assert_eq!(name, "archive.tar.gz");
        assert!(size > 20);
        let mut f = file;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf.len() as u64, size);
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&buf[..]));
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.trim_end_matches('/') == "archive"),
            "{names:?}"
        );
        assert!(names.contains(&"archive/old.txt".to_string()), "{names:?}");

        assert_eq!(home.delete("archive").await.unwrap(), "archive");
        assert_eq!(home.delete("archive").await.unwrap_err().status, 404);
        assert_eq!(home.delete(".").await.unwrap_err().status, 403);
        assert!(looks_like_text(b"plain text"));
        assert!(!looks_like_text(b"bin\0ary"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn root_join(home: &Home, rel: &str) -> PathBuf {
        home.root().join(rel)
    }
}
