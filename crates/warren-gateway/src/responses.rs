//! `POST /v1/responses` and friends: one response per agent turn.
//!
//! A turn runs in its own task. Every event it produces is appended to an
//! in-memory record, fanned out on a `tokio::sync::broadcast` channel for
//! live streams and persisted to `<home>/.warren/responses/<id>.json`, so
//! `GET /v1/responses/{id}/stream` can replay what happened so far and then
//! follow the live tail. Cancelling aborts the driver task and marks the
//! response `cancelled`; the stream always ends with `response.completed`.

use std::{
    collections::HashMap,
    convert::Infallible,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json,
    extract::{Path as UrlPath, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures::Stream;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, broadcast::error::RecvError, watch},
    task::JoinHandle,
};
use warren_core::{
    Clock, Error, Result,
    agentapi::{
        EVENT_COMPLETED, EVENT_CREATED, EVENT_DELTA, EVENT_FAILED, EVENT_TOOL_CALL, ResponseObject,
        ResponseStatus, ResponsesRequest,
    },
    errors::CODE_NOT_SUPPORTED,
    ids,
};

use crate::{
    drivers::{Driver, EventSink, Reply, Turn, describe_error},
    extract::{JsonBody, parse_json},
    owner::HomeOwner,
    server::SharedState,
    sessions::{SessionStore, valid_id},
};

/// Fields accepted by `POST /v1/responses`.
const RESPONSE_FIELDS: &[&str] = &[
    "input",
    "session_id",
    "stream",
    "model",
    "files",
    "metadata",
];
/// Live events buffered per response for slow subscribers.
const BROADCAST_CAPACITY: usize = 1024;
/// Deltas between two disk flushes of an in-progress response.
const PERSIST_EVERY: usize = 20;

/// One persisted SSE event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub seq: u64,
    pub event: String,
    pub data: Value,
}

impl StoredEvent {
    /// The stream ends with `response.completed`.
    pub fn is_terminal(&self) -> bool {
        self.event == EVENT_COMPLETED
    }
    fn to_sse(&self) -> Event {
        Event::default()
            .event(self.event.clone())
            .data(self.data.to_string())
    }
}

/// The on-disk form of a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseRecord {
    pub response: ResponseObject,
    pub events: Vec<StoredEvent>,
}

impl ResponseRecord {
    fn push(&mut self, tx: &broadcast::Sender<StoredEvent>, event: &str, data: Value) {
        let seq = self.events.last().map(|e| e.seq + 1).unwrap_or(1);
        if event == EVENT_DELTA
            && let Some(d) = data.get("delta").and_then(Value::as_str)
        {
            self.response.output_text.push_str(d);
        }
        let ev = StoredEvent {
            seq,
            event: event.to_string(),
            data,
        };
        let _ = tx.send(ev.clone());
        self.events.push(ev);
    }
}

/// A response whose turn is still running (or just finished).
pub struct Live {
    record: Mutex<ResponseRecord>,
    tx: broadcast::Sender<StoredEvent>,
    done_tx: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    unsaved: AtomicUsize,
    path: PathBuf,
    owner: HomeOwner,
}

impl Live {
    /// Current snapshot of the response object.
    pub fn response(&self) -> ResponseObject {
        self.record.lock().response.clone()
    }

    fn persist_now(&self, bytes: Vec<u8>) {
        self.unsaved.store(0, Ordering::SeqCst);
        if let Err(e) = write_sync(&self.path, &bytes, &self.owner) {
            tracing::warn!(path = %self.path.display(), error = %e, "cannot persist response");
        }
    }

    /// Append an event; `force` flushes to disk immediately.
    fn emit(&self, event: &str, data: Value, force: bool) {
        let bytes = {
            let mut rec = self.record.lock();
            rec.push(&self.tx, event, data);
            let pending = self.unsaved.fetch_add(1, Ordering::SeqCst) + 1;
            if force || pending >= PERSIST_EVERY {
                serde_json::to_vec_pretty(&*rec).ok()
            } else {
                None
            }
        };
        if let Some(bytes) = bytes {
            self.persist_now(bytes);
        }
    }
}

/// Atomic write through a sibling temp file; created directories and the
/// file itself are handed to `owner`.
fn write_sync(path: &Path, bytes: &[u8], owner: &HomeOwner) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        owner.create_dir_all_owned(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", ids::new_id()));
    std::fs::write(&tmp, bytes)?;
    owner.own(&tmp);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The [`EventSink`] handed to drivers.
struct Emitter {
    live: Arc<Live>,
}

impl EventSink for Emitter {
    fn delta(&self, text: &str) {
        self.live.emit(EVENT_DELTA, json!({ "delta": text }), false);
    }
    fn tool_call(&self, name: &str, arguments: &Value) {
        self.live.emit(
            EVENT_TOOL_CALL,
            json!({ "name": name, "arguments": arguments }),
            true,
        );
    }
}

/// What a finished turn produced.
enum Outcome {
    Completed(Reply),
    Failed(String),
    Cancelled,
}

/// Everything a handler needs right after starting a turn.
pub struct Started {
    pub live: Arc<Live>,
    pub replay: Vec<StoredEvent>,
    pub rx: broadcast::Receiver<StoredEvent>,
    pub done: watch::Receiver<bool>,
}

/// Owns live responses and the on-disk archive.
pub struct ResponseManager {
    dir: PathBuf,
    home: PathBuf,
    owner: HomeOwner,
    sessions: Arc<SessionStore>,
    driver: Option<Arc<dyn Driver>>,
    disabled_reason: String,
    clock: Arc<dyn Clock>,
    live: RwLock<HashMap<String, Arc<Live>>>,
}

impl ResponseManager {
    /// `driver == None` disables `POST /v1/responses` with `404 not_supported`;
    /// `disabled_reason` is appended to that error when non-empty.
    pub fn new(
        state_dir: &Path,
        home: PathBuf,
        owner: HomeOwner,
        sessions: Arc<SessionStore>,
        driver: Option<Arc<dyn Driver>>,
        disabled_reason: String,
        clock: Arc<dyn Clock>,
    ) -> Self {
        ResponseManager {
            dir: state_dir.join("responses"),
            home,
            owner,
            sessions,
            driver,
            disabled_reason,
            clock,
            live: RwLock::new(HashMap::new()),
        }
    }

    /// True when a driver is configured.
    pub fn enabled(&self) -> bool {
        self.driver.is_some()
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn not_supported(&self) -> Error {
        let mut msg = "this instance has no agent; the Agent API is not available on this template"
            .to_string();
        if !self.disabled_reason.is_empty() {
            msg = format!("{msg} ({})", self.disabled_reason);
        }
        Error::new(CODE_NOT_SUPPORTED, 404, msg)
    }

    /// Validate the request, record the user message and start the turn.
    pub async fn start(self: &Arc<Self>, req: ResponsesRequest) -> Result<Started> {
        let driver = self.driver.clone().ok_or_else(|| self.not_supported())?;
        if req.input.trim().is_empty() {
            return Err(Error::invalid("input must not be empty"));
        }
        let session = self
            .sessions
            .get_or_create(
                req.session_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
            )
            .await?;
        let history = session.messages.clone();
        self.sessions
            .append_message(&session.id, "user", &req.input)
            .await?;

        let id = ids::new_id();
        let now = self.clock.now_unix_ms();
        let response = ResponseObject {
            id: id.clone(),
            session_id: session.id.clone(),
            status: ResponseStatus::InProgress,
            output_text: String::new(),
            created_at: now,
            completed_at: None,
            usage: None,
            error: None,
        };
        let (tx, rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (done_tx, done) = watch::channel(false);
        let live = Arc::new(Live {
            record: Mutex::new(ResponseRecord {
                response,
                events: Vec::new(),
            }),
            tx,
            done_tx,
            task: Mutex::new(None),
            unsaved: AtomicUsize::new(0),
            path: self.path(&id),
            owner: self.owner.clone(),
        });
        self.live.write().insert(id.clone(), live.clone());
        live.emit(
            EVENT_CREATED,
            json!({ "id": id, "session_id": session.id }),
            true,
        );
        let replay = live.record.lock().events.clone();

        let turn = Turn {
            response_id: id.clone(),
            session_id: session.id.clone(),
            input: req.input.clone(),
            model: req.model.clone(),
            files: req.files.clone().unwrap_or_default(),
            history,
            home: self.home.clone(),
            owner: self.owner.clone(),
            sink: Arc::new(Emitter { live: live.clone() }),
        };
        let manager = self.clone();
        let task_live = live.clone();
        let driver_name = driver.name().to_string();
        let handle = tokio::spawn(async move {
            let outcome = match driver.run(&turn).await {
                Ok(reply) => {
                    if let Err(e) = manager
                        .sessions
                        .append_message(&turn.session_id, "assistant", &reply.text)
                        .await
                    {
                        tracing::warn!(session = %turn.session_id, error = %e, "cannot record assistant message");
                    }
                    Outcome::Completed(reply)
                }
                Err(e) => {
                    tracing::warn!(response = %turn.response_id, error = %e, "turn failed");
                    Outcome::Failed(describe_error(&e))
                }
            };
            manager.finish(&task_live, outcome);
        });
        *live.task.lock() = Some(handle);
        tracing::info!(response = %id, session = %session.id, driver = %driver_name, "turn started");
        Ok(Started {
            live,
            replay,
            rx,
            done,
        })
    }

    /// Move a live response to a terminal state exactly once.
    fn finish(&self, live: &Arc<Live>, outcome: Outcome) {
        let (id, bytes) = {
            let mut rec = live.record.lock();
            if rec.response.status != ResponseStatus::InProgress {
                return;
            }
            let now = self.clock.now_unix_ms();
            let id = rec.response.id.clone();
            match outcome {
                Outcome::Completed(reply) => {
                    rec.response.status = ResponseStatus::Completed;
                    rec.response.output_text = reply.text;
                    rec.response.usage = reply.usage;
                }
                Outcome::Failed(error) => {
                    rec.response.status = ResponseStatus::Failed;
                    rec.response.error = Some(error.clone());
                    rec.push(&live.tx, EVENT_FAILED, json!({ "id": id, "error": error }));
                }
                Outcome::Cancelled => rec.response.status = ResponseStatus::Cancelled,
            }
            rec.response.completed_at = Some(now);
            let completed = json!({
                "id": id,
                "session_id": rec.response.session_id,
                "output_text": rec.response.output_text,
                "status": rec.response.status,
            });
            rec.push(&live.tx, EVENT_COMPLETED, completed);
            (id, serde_json::to_vec_pretty(&*rec).ok())
        };
        if let Some(bytes) = bytes {
            live.persist_now(bytes);
        }
        let _ = live.done_tx.send(true);
        self.live.write().remove(&id);
        tracing::info!(response = %id, status = ?live.record.lock().response.status, "turn finished");
    }

    /// Cancel a running turn. Finished responses are returned unchanged.
    pub async fn cancel(&self, id: &str) -> Result<ResponseObject> {
        let live = self.live.read().get(id).cloned();
        match live {
            Some(live) => {
                if let Some(handle) = live.task.lock().take() {
                    handle.abort();
                }
                self.finish(&live, Outcome::Cancelled);
                Ok(live.response())
            }
            None => Ok(self.load(id).await?.response),
        }
    }

    /// The current response object.
    pub async fn get(&self, id: &str) -> Result<ResponseObject> {
        let live = self.live.read().get(id).cloned();
        match live {
            Some(live) => Ok(live.response()),
            None => Ok(self.load(id).await?.response),
        }
    }

    /// Stored events plus, for a live response, a receiver for the rest.
    pub async fn subscribe(
        &self,
        id: &str,
    ) -> Result<(
        Vec<StoredEvent>,
        Option<broadcast::Receiver<StoredEvent>>,
        Option<Arc<Live>>,
    )> {
        let live = self.live.read().get(id).cloned();
        if let Some(live) = live {
            let rec = live.record.lock();
            let rx = live.tx.subscribe();
            let snapshot = rec.events.clone();
            drop(rec);
            return Ok((snapshot, Some(rx), Some(live)));
        }
        Ok((self.load(id).await?.events, None, None))
    }

    /// Read a finished response from disk. A record left `in_progress` by a
    /// crash is repaired to `failed` so its stream terminates.
    pub async fn load(&self, id: &str) -> Result<ResponseRecord> {
        if !valid_id(id) {
            return Err(Error::not_found("response"));
        }
        let bytes = match tokio::fs::read(self.path(id)).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::not_found("response"));
            }
            Err(e) => return Err(e.into()),
        };
        let mut rec: ResponseRecord = serde_json::from_slice(&bytes)
            .map_err(|e| Error::internal(format!("corrupt response file {id}.json: {e}")))?;
        if rec.response.status == ResponseStatus::InProgress {
            let error = "the gateway restarted while this response was in progress".to_string();
            rec.response.status = ResponseStatus::Failed;
            rec.response.error = Some(error.clone());
            rec.response.completed_at = Some(self.clock.now_unix_ms());
            let (tx, _rx) = broadcast::channel(1);
            rec.push(&tx, EVENT_FAILED, json!({ "id": id, "error": error }));
            let completed = json!({ "id": id, "session_id": rec.response.session_id, "output_text": rec.response.output_text, "status": rec.response.status });
            rec.push(&tx, EVENT_COMPLETED, completed);
            if let Ok(bytes) = serde_json::to_vec_pretty(&rec) {
                let _ = write_sync(&self.path(id), &bytes, &self.owner);
            }
        }
        Ok(rec)
    }
}

/// An SSE body that replays `replay`, then follows `rx` until the terminal event.
pub fn sse_response(
    replay: Vec<StoredEvent>,
    rx: Option<broadcast::Receiver<StoredEvent>>,
    live: Option<Arc<Live>>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    struct Follow {
        replay: std::vec::IntoIter<StoredEvent>,
        rx: Option<broadcast::Receiver<StoredEvent>>,
        live: Option<Arc<Live>>,
        last_seq: u64,
        done: bool,
    }
    let state = Follow {
        replay: replay.into_iter(),
        rx,
        live,
        last_seq: 0,
        done: false,
    };
    let stream = futures::stream::unfold(state, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            if let Some(ev) = st.replay.next() {
                st.last_seq = ev.seq;
                st.done = ev.is_terminal();
                return Some((Ok(ev.to_sse()), st));
            }
            let rx = st.rx.as_mut()?;
            match rx.recv().await {
                Ok(ev) => {
                    if ev.seq <= st.last_seq {
                        continue;
                    }
                    st.last_seq = ev.seq;
                    st.done = ev.is_terminal();
                    return Some((Ok(ev.to_sse()), st));
                }
                Err(RecvError::Lagged(_)) => {
                    // Resynchronize from the record; everything is kept there.
                    if let Some(live) = &st.live {
                        let missed: Vec<StoredEvent> = {
                            let rec = live.record.lock();
                            rec.events
                                .iter()
                                .filter(|e| e.seq > st.last_seq)
                                .cloned()
                                .collect()
                        };
                        st.replay = missed.into_iter();
                    }
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ----- handlers -----

/// `POST /v1/responses`
pub async fn create_handler(
    State(state): State<SharedState>,
    JsonBody(body): JsonBody,
) -> Result<Response> {
    let req: ResponsesRequest = parse_json(&body, RESPONSE_FIELDS)?;
    let stream = req.stream.unwrap_or(false);
    let started = state.responses.start(req).await?;
    if stream {
        return Ok(
            sse_response(started.replay, Some(started.rx), Some(started.live)).into_response(),
        );
    }
    let mut done = started.done;
    let _ = done.wait_for(|d| *d).await;
    Ok(Json(started.live.response()).into_response())
}

/// `GET /v1/responses/{id}`
pub async fn get_handler(
    State(state): State<SharedState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<ResponseObject>> {
    Ok(Json(state.responses.get(&id).await?))
}

/// `GET /v1/responses/{id}/stream`
pub async fn stream_handler(
    State(state): State<SharedState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response> {
    let (replay, rx, live) = state.responses.subscribe(&id).await?;
    Ok(sse_response(replay, rx, live).into_response())
}

/// `POST /v1/responses/{id}/cancel`
pub async fn cancel_handler(
    State(state): State<SharedState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>> {
    let r = state.responses.cancel(&id).await?;
    Ok(Json(json!({ "id": r.id, "status": r.status })))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use warren_core::RealClock;

    use super::*;
    use crate::drivers::echo::EchoDriver;

    fn manager(driver: Option<Arc<dyn Driver>>) -> (Arc<ResponseManager>, PathBuf) {
        let home = std::env::temp_dir().join(format!("warren-gw-responses-{}", ids::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(RealClock);
        let state_dir = home.join(".warren");
        let owner = HomeOwner::current();
        let sessions = Arc::new(SessionStore::new(&state_dir, clock.clone(), owner.clone()));
        let m = ResponseManager::new(
            &state_dir,
            home.clone(),
            owner,
            sessions,
            driver,
            String::new(),
            clock,
        );
        (Arc::new(m), home)
    }

    async fn wait_done(started: &mut Started) {
        let mut done = started.done.clone();
        tokio::time::timeout(Duration::from_secs(20), done.wait_for(|d| *d))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn completes_persists_and_replays() {
        let (m, home) = manager(Some(Arc::new(EchoDriver)));
        let req = ResponsesRequest {
            input: "hi".into(),
            ..Default::default()
        };
        let mut started = m.start(req).await.unwrap();
        assert_eq!(started.replay.len(), 1);
        assert_eq!(started.replay[0].event, EVENT_CREATED);
        wait_done(&mut started).await;
        let r = started.live.response();
        assert_eq!(r.status, ResponseStatus::Completed);
        assert_eq!(r.output_text, "echo: hi");
        assert!(r.completed_at.is_some());

        // The live map is drained and the record is on disk.
        assert!(m.live.read().is_empty());
        let loaded = m.load(&r.id).await.unwrap();
        assert_eq!(loaded.response.status, ResponseStatus::Completed);
        let kinds: Vec<&str> = loaded.events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(kinds[0], EVENT_CREATED);
        assert_eq!(*kinds.last().unwrap(), EVENT_COMPLETED);
        assert!(kinds.iter().filter(|k| **k == EVENT_DELTA).count() >= 2);
        let seqs: Vec<u64> = loaded.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
        assert_eq!(m.get(&r.id).await.unwrap().output_text, "echo: hi");
        assert!(m.get("nope").await.unwrap_err().is_not_found());
        assert!(m.get("../x").await.unwrap_err().is_not_found());

        // The session holds both messages and the next turn sees history.
        let s = m.sessions.get(&r.session_id).await.unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[1].content, "echo: hi");

        // Cancelling a finished response is a no-op.
        assert_eq!(
            m.cancel(&r.id).await.unwrap().status,
            ResponseStatus::Completed
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn stream_follows_live_and_cancel_terminates() {
        let (m, home) = manager(Some(Arc::new(EchoDriver)));
        let req = ResponsesRequest {
            input: "!sleep 30".into(),
            stream: Some(true),
            ..Default::default()
        };
        let started = m.start(req).await.unwrap();
        let id = started.live.response().id.clone();
        // A second follower subscribes while the turn runs.
        let (replay, rx, live) = m.subscribe(&id).await.unwrap();
        assert!(rx.is_some());
        let sse = sse_response(replay, rx, live);
        let follower = tokio::spawn(async move {
            let mut body = sse.into_response().into_body().into_data_stream();
            let mut text = String::new();
            while let Some(chunk) = body.next().await {
                text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            }
            text
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let cancelled = m.cancel(&id).await.unwrap();
        assert_eq!(cancelled.status, ResponseStatus::Cancelled);
        let text = tokio::time::timeout(Duration::from_secs(10), follower)
            .await
            .unwrap()
            .unwrap();
        assert!(text.contains("event: response.created"), "{text}");
        assert!(text.contains("event: response.tool_call"), "{text}");
        assert!(
            text.trim_end().ends_with("\"status\":\"cancelled\"}"),
            "{text}"
        );
        assert_eq!(m.get(&id).await.unwrap().status, ResponseStatus::Cancelled);
        // Replay of the finished response ends with the same terminal event.
        let (replay, rx, _) = m.subscribe(&id).await.unwrap();
        assert!(rx.is_none());
        assert!(replay.last().unwrap().is_terminal());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn validation_and_disabled_agent() {
        let (m, home) = manager(Some(Arc::new(EchoDriver)));
        let e = m
            .start(ResponsesRequest {
                input: "  ".into(),
                ..Default::default()
            })
            .await
            .err()
            .unwrap();
        assert_eq!(e.status, 400);
        let e = m
            .start(ResponsesRequest {
                input: "x".into(),
                session_id: Some("bad/id".into()),
                ..Default::default()
            })
            .await
            .err()
            .unwrap();
        assert_eq!(e.status, 400);
        let (none, home2) = manager(None);
        let e = none
            .start(ResponsesRequest {
                input: "hi".into(),
                ..Default::default()
            })
            .await
            .err()
            .unwrap();
        assert_eq!((e.status, e.code.as_str()), (404, CODE_NOT_SUPPORTED));
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&home2).unwrap();
    }

    #[tokio::test]
    async fn stale_in_progress_records_are_repaired() {
        let (m, home) = manager(None);
        let rec = ResponseRecord {
            response: ResponseObject {
                id: "stale1".into(),
                session_id: "s".into(),
                status: ResponseStatus::InProgress,
                output_text: "partial".into(),
                created_at: 1,
                completed_at: None,
                usage: None,
                error: None,
            },
            events: vec![StoredEvent {
                seq: 1,
                event: EVENT_CREATED.into(),
                data: json!({}),
            }],
        };
        write_sync(
            &m.path("stale1"),
            &serde_json::to_vec(&rec).unwrap(),
            &m.owner,
        )
        .unwrap();
        let loaded = m.load("stale1").await.unwrap();
        assert_eq!(loaded.response.status, ResponseStatus::Failed);
        assert!(loaded.events.last().unwrap().is_terminal());
        assert_eq!(loaded.events.len(), 3);
        let again = m.load("stale1").await.unwrap();
        assert_eq!(again.events.len(), 3);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
