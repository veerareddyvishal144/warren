//! Talking to the gateway inside a guest: waiting for its port, `POST /v1/exec`
//! with the instance token, and `POST /v1/shutdown`.

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use warren_core::{
    DEFAULT_EXEC_TIMEOUT_SEC, Error, ExecRequest, ExecResult, GATEWAY_PORT, HEADER_INSTANCE_TOKEN,
    Result, json::ErrorEnvelope,
};

/// Poll a TCP connect to `ip:port` until it succeeds, `timeout` passes, or
/// `still_alive` reports that the VM is gone.
pub async fn wait_for_port(
    ip: Ipv4Addr,
    port: u16,
    timeout: Duration,
    mut still_alive: impl FnMut() -> bool,
) -> Result<()> {
    let addr = SocketAddr::from((ip, port));
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(addr))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return Ok(());
        }
        if !still_alive() {
            return Err(Error::internal(format!(
                "the VM exited before the gateway listened on {addr}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(Error::timeout(format!(
                "gateway at {addr} did not accept connections within {}s",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn base_url(ip: Ipv4Addr) -> String {
    format!("http://{ip}:{GATEWAY_PORT}")
}

fn map_transport(ip: Ipv4Addr, e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::timeout(format!("the gateway at {ip} did not answer in time"))
    } else if e.is_connect() {
        Error::bad_gateway(format!("cannot reach the gateway at {ip}: {e}"))
    } else {
        Error::bad_gateway(format!("gateway request to {ip} failed: {e}"))
    }
}

/// Run a command through the gateway's exec endpoint.
pub async fn exec(
    http: &reqwest::Client,
    ip: Ipv4Addr,
    token: &str,
    req: &ExecRequest,
) -> Result<ExecResult> {
    let secs = if req.timeout_seconds == 0 {
        DEFAULT_EXEC_TIMEOUT_SEC
    } else {
        req.timeout_seconds
    };
    let resp = http
        .post(format!("{}/v1/exec", base_url(ip)))
        .header(HEADER_INSTANCE_TOKEN, token)
        .json(req)
        .timeout(Duration::from_secs(u64::from(secs) + 30))
        .send()
        .await
        .map_err(|e| map_transport(ip, e))?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| map_transport(ip, e))?;
    if status.is_success() {
        return serde_json::from_slice::<ExecResult>(&body).map_err(|e| {
            Error::bad_gateway(format!("gateway returned an unreadable exec result: {e}"))
        });
    }
    Err(error_from_body(status.as_u16(), &body))
}

/// Turn a non-2xx gateway answer into an `Error`: the envelope when it is one,
/// otherwise a bad_gateway with the start of the body.
pub fn error_from_body(status: u16, body: &[u8]) -> Error {
    match serde_json::from_slice::<ErrorEnvelope>(body) {
        Ok(env) => {
            let mut e = env.error;
            e.status = status;
            e
        }
        Err(_) => {
            let text: String = String::from_utf8_lossy(body).chars().take(200).collect();
            Error::bad_gateway(format!("gateway answered HTTP {status}: {text}"))
        }
    }
}

/// Ask the gateway to power the guest off. The guest usually dies before the
/// response is complete, so only transport-level refusal is reported.
pub async fn shutdown(http: &reqwest::Client, ip: Ipv4Addr, token: &str) -> Result<()> {
    let resp = http
        .post(format!("{}/v1/shutdown", base_url(ip)))
        .header(HEADER_INSTANCE_TOKEN, token)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| map_transport(ip, e))?;
    let status = resp.status();
    if status.is_success() || status.is_server_error() {
        Ok(())
    } else {
        let body = resp.bytes().await.unwrap_or_default();
        Err(error_from_body(status.as_u16(), &body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn error_bodies() {
        let e = error_from_body(409, br#"{"error":{"code":"conflict","message":"busy"}}"#);
        assert_eq!(e.code, "conflict");
        assert_eq!(e.status, 409);
        let e = error_from_body(502, b"<html>nope</html>");
        assert_eq!(e.code, warren_core::errors::CODE_BAD_GATEWAY);
        assert!(e.message.contains("502") && e.message.contains("nope"));
    }

    #[tokio::test]
    async fn waits_for_a_listener_and_gives_up() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        wait_for_port(Ipv4Addr::LOCALHOST, port, Duration::from_secs(5), || true)
            .await
            .unwrap();
        drop(listener);
        // Port 1 (tcpmux) never has a listener on a developer machine, unlike the
        // just-freed ephemeral port that a concurrent test may rebind at once.
        let closed_port = 1;
        let err = wait_for_port(
            Ipv4Addr::LOCALHOST,
            closed_port,
            Duration::from_millis(600),
            || true,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_TIMEOUT);
        let err = wait_for_port(
            Ipv4Addr::LOCALHOST,
            closed_port,
            Duration::from_secs(5),
            || false,
        )
        .await
        .unwrap_err();
        assert!(err.is_internal());
    }

    /// A one-shot HTTP server that answers with `status` and `body`.
    async fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let mut got = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let head = &text[..i];
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if got.len() >= i + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            String::from_utf8_lossy(&got).to_string()
        });
        (port, handle)
    }

    #[tokio::test]
    async fn exec_posts_token_and_maps_results() {
        // The gateway module always talks to GATEWAY_PORT, so exercise the HTTP
        // plumbing through the same client against a local server.
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let (port, server) = serve_once(
            "200 OK",
            r#"{"exit_code":3,"stdout":"out","stderr":"err","duration_ms":12,"truncated":true}"#,
        )
        .await;
        let req = ExecRequest {
            command: "ls".into(),
            user: "root".into(),
            timeout_seconds: 5,
            ..Default::default()
        };
        let resp = http
            .post(format!("http://127.0.0.1:{port}/v1/exec"))
            .header(HEADER_INSTANCE_TOKEN, "tok")
            .json(&req)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.bytes().await.unwrap();
        assert!(status.is_success());
        let res: ExecResult = serde_json::from_slice(&body).unwrap();
        assert_eq!(res.exit_code, 3);
        assert!(res.truncated);
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/exec HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-warren-instance-token: tok")
        );
        assert!(request.contains(r#""command":"ls""#));
        assert!(request.contains(r#""user":"root""#));
    }
}
