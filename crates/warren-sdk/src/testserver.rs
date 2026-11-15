//! A minimal HTTP/1.1 server on a tokio `TcpListener` for unit tests. It parses
//! one request per connection (request line, headers, `Content-Length` body),
//! hands it to a handler and writes the answer with `Connection: close`, either
//! as one body or as timed chunks (`Transfer-Encoding: chunked`) to exercise
//! incremental stream parsing.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Raw query string without the `?`.
    pub query: String,
    /// Lower-cased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("request body is not JSON: {e}"))
    }

    /// A percent-decoded query parameter.
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query
            .split('&')
            .filter(|p| !p.is_empty())
            .find_map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (percent_decode(k) == name).then(|| percent_decode(v))
            })
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub enum Body {
    Full(Vec<u8>),
    /// Each chunk is written after its delay and flushed.
    Chunked(Vec<(Vec<u8>, Duration)>),
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Body,
}

impl Response {
    pub fn json(status: u16, value: serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json".into(),
            body: Body::Full(value.to_string().into_bytes()),
        }
    }

    pub fn text(status: u16, text: &str) -> Response {
        Response {
            status,
            content_type: "text/plain".into(),
            body: Body::Full(text.as_bytes().to_vec()),
        }
    }

    pub fn bytes(status: u16, content_type: &str, bytes: Vec<u8>) -> Response {
        Response {
            status,
            content_type: content_type.into(),
            body: Body::Full(bytes),
        }
    }

    /// An error envelope.
    pub fn error(status: u16, code: &str, message: &str) -> Response {
        Self::json(
            status,
            serde_json::json!({ "error": { "code": code, "message": message } }),
        )
    }

    /// A `text/event-stream` answer written as timed chunks.
    pub fn sse(chunks: &[(&str, u64)]) -> Response {
        Response {
            status: 200,
            content_type: "text/event-stream".into(),
            body: Body::Chunked(
                chunks
                    .iter()
                    .map(|(c, ms)| (c.as_bytes().to_vec(), Duration::from_millis(*ms)))
                    .collect(),
            ),
        }
    }

    pub fn not_found() -> Response {
        Self::error(404, "not_found", "route not found")
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

pub struct TestServer {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    handle: JoinHandle<()>,
}

impl TestServer {
    pub async fn start(
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
    ) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = Arc::new(handler);
        let log = requests.clone();
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, handler, log).await;
                });
            }
        });
        TestServer {
            addr,
            requests,
            handle,
        }
    }

    /// `http://127.0.0.1:<port>`
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `http://127.0.0.1:<port>/v1`
    pub fn api_url(&self) -> String {
        format!("{}/v1", self.url())
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("requests lock").clone()
    }

    pub fn last_request(&self) -> Request {
        self.requests().pop().expect("at least one request")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn serve(
    mut stream: TcpStream,
    handler: Handler,
    log: Arc<Mutex<Vec<Request>>>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        let mut tmp = [0u8; 4096];
        let n = stream.read(&mut tmp).await?;
        if n == 0 || buf.len() > (1 << 20) {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut body = buf[head_end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while body.len() < len {
        let mut tmp = [0u8; 4096];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    let req = Request {
        method,
        path,
        query,
        headers,
        body,
    };
    let resp = handler(&req);
    log.lock().expect("requests lock").push(req);
    write_response(&mut stream, resp).await?;
    stream.shutdown().await
}

async fn write_response(stream: &mut TcpStream, resp: Response) -> std::io::Result<()> {
    let reason = match resp.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nConnection: close\r\nContent-Type: {}\r\n",
        resp.status, resp.content_type
    );
    match resp.body {
        Body::Full(b) => {
            head.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&b).await?;
        }
        Body::Chunked(chunks) => {
            head.push_str("Transfer-Encoding: chunked\r\n\r\n");
            stream.write_all(head.as_bytes()).await?;
            stream.flush().await?;
            for (chunk, delay) in chunks {
                if chunk.is_empty() {
                    continue;
                }
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                stream
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await?;
                stream.write_all(&chunk).await?;
                stream.write_all(b"\r\n").await?;
                stream.flush().await?;
            }
            stream.write_all(b"0\r\n\r\n").await?;
        }
    }
    stream.flush().await
}
