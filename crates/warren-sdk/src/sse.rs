//! Incremental `text/event-stream` parsing and the event stream returned by
//! [`crate::AgentClient::stream`].
//!
//! The parser follows the WHATWG EventSource rules: lines end with CRLF, LF or
//! CR; `:` lines are comments; `event:` names the event; several `data:` lines
//! are joined with `\n`; an empty line dispatches. Input arrives in arbitrary
//! chunks, so a line, a CRLF pair or a multi-byte character may be split across
//! calls to [`SseParser::push`].

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};

use crate::error::SdkError;
use crate::http::send_checked;

/// One server-sent event. `data` is the parsed JSON payload; a payload that is
/// not JSON is kept verbatim as a JSON string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SseEvent {
    /// The `event:` name, `message` when the server sent none.
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    /// A string field of the payload (`delta`, `output_text`, `error`...).
    pub fn str_field(&self, name: &str) -> Option<&str> {
        self.data.get(name).and_then(|v| v.as_str())
    }
}

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Incremental SSE parser. Feed bytes with [`push`](Self::push), collect the
/// complete events it returns, and call [`finish`](Self::finish) at end of
/// stream.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    started: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk and return every event completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        if !self.started {
            if self.buf.len() < BOM.len() && BOM.starts_with(&self.buf) {
                return out;
            }
            if self.buf.starts_with(BOM) {
                self.buf.drain(..BOM.len());
            }
            self.started = true;
        }
        let mut start = 0;
        while let Some(pos) = self.buf[start..]
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
        {
            let end = start + pos;
            let next = if self.buf[end] == b'\r' {
                if end + 1 >= self.buf.len() {
                    // A CR at the very end may be the first half of a CRLF.
                    break;
                }
                if self.buf[end + 1] == b'\n' {
                    end + 2
                } else {
                    end + 1
                }
            } else {
                end + 1
            };
            let line = self.buf[start..end].to_vec();
            self.handle_line(&line, &mut out);
            start = next;
        }
        self.buf.drain(..start);
        out
    }

    /// End of stream: handle a final unterminated line and dispatch a pending
    /// event that never got its blank line (lenient; the spec would drop it).
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buf);
        if !rest.is_empty() {
            let line = rest.strip_suffix(b"\r").unwrap_or(&rest);
            self.handle_line(line, &mut out);
        }
        if self.has_data {
            self.dispatch(&mut out);
        }
        out
    }

    fn handle_line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line[0] == b':' {
            return;
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(b" ").unwrap_or(v))
            }
            None => (line, &line[line.len()..]),
        };
        match field {
            b"event" => self.event = String::from_utf8_lossy(value).into_owned(),
            b"data" => {
                self.data.push_str(&String::from_utf8_lossy(value));
                self.data.push('\n');
                self.has_data = true;
            }
            _ => {} // id, retry and unknown fields are not needed here
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        let event = std::mem::take(&mut self.event);
        let mut data = std::mem::take(&mut self.data);
        if !std::mem::replace(&mut self.has_data, false) {
            return;
        }
        if data.ends_with('\n') {
            data.pop();
        }
        let data = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => serde_json::Value::String(data),
        };
        out.push(SseEvent {
            event: if event.is_empty() {
                "message".to_string()
            } else {
                event
            },
            data,
        });
    }
}

type ConnectFuture = Pin<Box<dyn Future<Output = Result<reqwest::Response, SdkError>> + Send>>;
type BodyStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

enum State {
    Connecting(ConnectFuture),
    Streaming {
        body: BodyStream,
        parser: SseParser,
        pending: VecDeque<SseEvent>,
    },
    Draining(VecDeque<SseEvent>),
    Done,
}

/// A stream of [`SseEvent`]s. The request is sent lazily on the first poll; a
/// non-2xx answer surfaces as the first (and only) item, an `Err`.
pub struct EventStream {
    state: State,
}

impl EventStream {
    /// Send `rb` on first poll and parse its body as `text/event-stream`.
    pub fn connect(rb: RequestBuilder) -> Self {
        EventStream {
            state: State::Connecting(Box::pin(send_checked(rb))),
        }
    }

    /// Parse the body of an already received response.
    pub fn from_response(resp: reqwest::Response) -> Self {
        EventStream {
            state: streaming(resp),
        }
    }
}

fn streaming(resp: reqwest::Response) -> State {
    State::Streaming {
        body: Box::pin(resp.bytes_stream()),
        parser: SseParser::new(),
        pending: VecDeque::new(),
    }
}

impl Stream for EventStream {
    type Item = Result<SseEvent, SdkError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                State::Connecting(fut) => match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => {
                        this.state = State::Done;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Ready(Ok(resp)) => this.state = streaming(resp),
                },
                State::Streaming {
                    body,
                    parser,
                    pending,
                } => {
                    if let Some(ev) = pending.pop_front() {
                        return Poll::Ready(Some(Ok(ev)));
                    }
                    match body.as_mut().poll_next(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(chunk))) => pending.extend(parser.push(&chunk)),
                        Poll::Ready(Some(Err(e))) => {
                            this.state = State::Done;
                            return Poll::Ready(Some(Err(SdkError::Http(e))));
                        }
                        Poll::Ready(None) => {
                            let rest = parser.finish();
                            this.state = State::Draining(rest.into());
                        }
                    }
                }
                State::Draining(pending) => match pending.pop_front() {
                    Some(ev) => return Poll::Ready(Some(Ok(ev))),
                    None => this.state = State::Done,
                },
                State::Done => return Poll::Ready(None),
            }
        }
    }
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            State::Connecting(_) => "connecting",
            State::Streaming { .. } => "streaming",
            State::Draining(_) => "draining",
            State::Done => "done",
        };
        f.debug_struct("EventStream")
            .field("state", &state)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const STREAM: &str = "event: response.created\ndata: {\"id\":\"r1\",\"session_id\":\"s1\"}\n\n\
        : keep-alive comment\n\
        event: response.output_text.delta\r\ndata: {\"delta\":\"hel\"}\r\n\r\n\
        event: response.output_text.delta\rdata: {\"delta\":\"lo\"}\r\r\
        data: {\"a\":\ndata: [1,\ndata: 2]}\n\n\
        event: response.completed\ndata: {\"id\":\"r1\",\"session_id\":\"s1\",\"output_text\":\"hello\",\"status\":\"completed\"}\n\n";

    fn expected() -> Vec<SseEvent> {
        vec![
            SseEvent {
                event: "response.created".into(),
                data: json!({"id":"r1","session_id":"s1"}),
            },
            SseEvent {
                event: "response.output_text.delta".into(),
                data: json!({"delta":"hel"}),
            },
            SseEvent {
                event: "response.output_text.delta".into(),
                data: json!({"delta":"lo"}),
            },
            SseEvent {
                event: "message".into(),
                data: json!({"a":[1,2]}),
            },
            SseEvent {
                event: "response.completed".into(),
                data: json!({"id":"r1","session_id":"s1","output_text":"hello","status":"completed"}),
            },
        ]
    }

    fn parse_split(input: &[u8], at: &[usize]) -> Vec<SseEvent> {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        let mut start = 0;
        for &i in at {
            out.extend(p.push(&input[start..i]));
            start = i;
        }
        out.extend(p.push(&input[start..]));
        out.extend(p.finish());
        out
    }

    #[test]
    fn parses_whole_stream() {
        assert_eq!(parse_split(STREAM.as_bytes(), &[]), expected());
    }

    #[test]
    fn every_split_point_yields_the_same_events() {
        let bytes = STREAM.as_bytes();
        for i in 1..bytes.len() {
            assert_eq!(parse_split(bytes, &[i]), expected(), "split at {i}");
        }
        // Three-way splits around the CRLF and CR-only terminators.
        for i in 60..bytes.len() - 1 {
            assert_eq!(
                parse_split(bytes, &[i, i + 1]),
                expected(),
                "split at {i},{}",
                i + 1
            );
        }
    }

    #[test]
    fn byte_at_a_time() {
        let bytes = STREAM.as_bytes();
        let all: Vec<usize> = (1..bytes.len()).collect();
        assert_eq!(parse_split(bytes, &all), expected());
    }

    #[test]
    fn bom_multibyte_and_non_json_data() {
        let input = "\u{feff}event: note\ndata: héllo wörld\n\n".as_bytes();
        let at: Vec<usize> = (1..input.len()).collect();
        let got = parse_split(input, &at);
        assert_eq!(
            got,
            vec![SseEvent {
                event: "note".into(),
                data: json!("héllo wörld")
            }]
        );
        let got = parse_split(input, &[1, 2]);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn fields_without_data_do_not_dispatch_and_eof_flushes() {
        let mut p = SseParser::new();
        assert!(p.push(b"event: lonely\n\nid: 7\nretry: 100\n\n").is_empty());
        assert!(p.push(b"event: last\ndata: {\"x\":1}").is_empty());
        assert_eq!(
            p.finish(),
            vec![SseEvent {
                event: "last".into(),
                data: json!({"x":1})
            }]
        );
        let mut p = SseParser::new();
        assert!(
            p.push(b"data:\n\n").len() == 1,
            "an empty data line still dispatches"
        );
        assert_eq!(p.finish(), vec![]);
    }

    #[test]
    fn colon_handling() {
        let mut p = SseParser::new();
        let got = p.push(
            b"data:no-space\ndata:  two-spaces\n\nfieldwithoutcolon\ndata: {\"k\":\"a:b\"}\n\n",
        );
        assert_eq!(got[0].data, json!("no-space\n two-spaces"));
        assert_eq!(got[1].data, json!({"k":"a:b"}));
        assert_eq!(got[1].str_field("k"), Some("a:b"));
    }

    #[test]
    fn event_stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<EventStream>();
    }
}
