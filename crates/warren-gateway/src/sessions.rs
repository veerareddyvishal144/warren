//! Chat sessions persisted as JSON files under `<home>/.warren/sessions/<id>.json`.
//!
//! A session is the message list a driver sees as history. Files are written
//! atomically (temp file + rename) and every mutation runs under one async
//! mutex so concurrent turns on the same session cannot lose messages.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::Mutex;
use warren_core::{
    Clock, Error, Result,
    agentapi::{Message, SessionDetail, SessionSummary},
    ids,
};

use crate::owner::HomeOwner;

/// Longest accepted session id (also the file name stem).
pub const MAX_ID_LEN: usize = 64;
/// Session names derived from the first message are cut at this length.
const AUTO_NAME_CHARS: usize = 60;

/// File-backed session store.
pub struct SessionStore {
    dir: PathBuf,
    clock: Arc<dyn Clock>,
    owner: HomeOwner,
    write_lock: Mutex<()>,
}

/// Session ids are safe file names: `[A-Za-z0-9_-]{1,64}`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn check_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "invalid session id {id:?}: use 1-{MAX_ID_LEN} letters, digits, - or _"
        )))
    }
}

/// Write `bytes` to `path` through a sibling temp file and an atomic rename.
/// Created directories and the file itself are handed to `owner`.
pub(crate) async fn write_atomic(path: &Path, bytes: &[u8], owner: &HomeOwner) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", ids::new_id()));
    if let Some(parent) = path.parent() {
        owner.create_dir_all_owned(parent)?;
    }
    tokio::fs::write(&tmp, bytes).await?;
    owner.own(&tmp);
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(())
}

impl SessionStore {
    /// Store rooted at `<state_dir>/sessions`; its files belong to `owner`.
    pub fn new(state_dir: &Path, clock: Arc<dyn Clock>, owner: HomeOwner) -> Self {
        SessionStore {
            dir: state_dir.join("sessions"),
            clock,
            owner,
            write_lock: Mutex::new(()),
        }
    }

    /// Directory holding the session files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    async fn read(&self, id: &str) -> Result<SessionDetail> {
        check_id(id)?;
        match tokio::fs::read(self.path(id)).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Error::internal(format!("corrupt session file {id}.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::not_found("session")),
            Err(e) => Err(e.into()),
        }
    }

    async fn write(&self, s: &SessionDetail) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(s)?;
        write_atomic(&self.path(&s.id), &bytes, &self.owner).await
    }

    /// All sessions, most recently updated first.
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut out = Vec::new();
        let mut rd = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            match self.read(id).await {
                Ok(s) => out.push(SessionSummary {
                    id: s.id,
                    name: s.name,
                    created_at: s.created_at,
                    updated_at: s.updated_at,
                    message_count: s.messages.len() as u32,
                }),
                Err(e) => tracing::warn!(id, error = %e, "skipping unreadable session file"),
            }
        }
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then(b.created_at.cmp(&a.created_at))
                .then(b.id.cmp(&a.id))
        });
        Ok(out)
    }

    /// One session with its messages.
    pub async fn get(&self, id: &str) -> Result<SessionDetail> {
        self.read(id).await
    }

    /// Create a session; a caller-chosen id must be unused.
    pub async fn create(&self, id: Option<&str>, name: &str) -> Result<SessionDetail> {
        let _guard = self.write_lock.lock().await;
        let id = match id {
            Some(id) => {
                check_id(id)?;
                if tokio::fs::try_exists(self.path(id)).await? {
                    return Err(Error::conflict(format!("session {id} already exists")));
                }
                id.to_string()
            }
            None => ids::new_id(),
        };
        let now = self.clock.now_unix_ms();
        let s = SessionDetail {
            id,
            name: name.to_string(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
        };
        self.write(&s).await?;
        Ok(s)
    }

    /// The session with `id`, created when it does not exist yet. Without an
    /// id a fresh session is created.
    pub async fn get_or_create(&self, id: Option<&str>) -> Result<SessionDetail> {
        match id {
            None => self.create(None, "").await,
            Some(id) => match self.read(id).await {
                Ok(s) => Ok(s),
                Err(e) if e.is_not_found() => match self.create(Some(id), "").await {
                    Ok(s) => Ok(s),
                    // Lost a race with a concurrent creator: read what they wrote.
                    Err(e) if e.code == warren_core::errors::CODE_CONFLICT => self.read(id).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
        }
    }

    /// Append a message and bump `updated_at`. An unnamed session takes its
    /// name from its first user message.
    pub async fn append_message(
        &self,
        id: &str,
        role: &str,
        content: &str,
    ) -> Result<SessionDetail> {
        let _guard = self.write_lock.lock().await;
        let mut s = self.read(id).await?;
        let now = self.clock.now_unix_ms();
        if s.name.is_empty() && role == "user" {
            s.name = auto_name(content);
        }
        s.messages.push(Message {
            role: role.to_string(),
            content: content.to_string(),
            at: now,
        });
        s.updated_at = now;
        self.write(&s).await?;
        Ok(s)
    }

    /// Rename a session.
    pub async fn rename(&self, id: &str, name: &str) -> Result<SessionDetail> {
        let _guard = self.write_lock.lock().await;
        let mut s = self.read(id).await?;
        s.name = name.trim().to_string();
        s.updated_at = self.clock.now_unix_ms();
        self.write(&s).await?;
        Ok(s)
    }

    /// Delete a session; `not_found` when it does not exist.
    pub async fn delete(&self, id: &str) -> Result<()> {
        check_id(id)?;
        let _guard = self.write_lock.lock().await;
        match tokio::fs::remove_file(self.path(id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::not_found("session")),
            Err(e) => Err(e.into()),
        }
    }
}

/// First line of the first message, trimmed to a display-friendly length.
pub fn auto_name(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut name: String = first_line.chars().take(AUTO_NAME_CHARS).collect();
    if first_line.chars().count() > AUTO_NAME_CHARS {
        name.push('…');
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct TickClock(AtomicI64);
    impl Clock for TickClock {
        fn now_unix(&self) -> i64 {
            self.now_unix_ms() / 1000
        }
        fn now_unix_ms(&self) -> i64 {
            self.0.fetch_add(1000, Ordering::SeqCst)
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warren-gw-sessions-{tag}-{}", ids::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn create_append_list_rename_delete() {
        let dir = temp_dir("crud");
        let store = SessionStore::new(
            &dir,
            Arc::new(TickClock(AtomicI64::new(1_000_000))),
            HomeOwner::current(),
        );
        assert!(store.list().await.unwrap().is_empty());

        let a = store.create(None, "").await.unwrap();
        assert_eq!(a.id.len(), 32);
        let b = store.get_or_create(Some("custom-id_1")).await.unwrap();
        assert_eq!(b.id, "custom-id_1");
        let b_again = store.get_or_create(Some("custom-id_1")).await.unwrap();
        assert_eq!(b_again.created_at, b.created_at);
        assert!(
            store
                .create(Some("custom-id_1"), "")
                .await
                .unwrap_err()
                .code
                .contains("conflict")
        );
        assert_eq!(
            store
                .get_or_create(Some("../evil"))
                .await
                .unwrap_err()
                .status,
            400
        );

        // Appending to `a` makes it the most recently updated.
        let a2 = store
            .append_message(&a.id, "user", "hello there\nsecond line")
            .await
            .unwrap();
        assert_eq!(a2.name, "hello there");
        assert_eq!(a2.messages.len(), 1);
        store
            .append_message(&a.id, "assistant", "hi")
            .await
            .unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, a.id);
        assert_eq!(list[0].message_count, 2);
        assert_eq!(list[1].id, "custom-id_1");

        let renamed = store.rename(&a.id, "  My chat ").await.unwrap();
        assert_eq!(renamed.name, "My chat");
        assert_eq!(store.get(&a.id).await.unwrap().name, "My chat");

        store.delete(&a.id).await.unwrap();
        assert!(store.get(&a.id).await.unwrap_err().is_not_found());
        assert!(store.delete(&a.id).await.unwrap_err().is_not_found());
        assert_eq!(store.list().await.unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_and_ids() {
        assert!(valid_id("abc-DEF_123"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"x".repeat(65)));
        assert_eq!(auto_name("\n\n  short  \nmore"), "short");
        let long = "y".repeat(100);
        let n = auto_name(&long);
        assert_eq!(n.chars().count(), AUTO_NAME_CHARS + 1);
        assert!(n.ends_with('…'));
    }
}
