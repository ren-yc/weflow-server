//! Graceful shutdown: the originally-reported symptom was that the binary
//! printed nothing on Ctrl+C and died instantly, because no signal handler was
//! ever installed — `AppState::shutdown` was constructed and subscribed, but
//! `send(true)` existed nowhere, so `sync::watch`'s exit branch was dead code
//! and the watcher directory handles were only released by process death
//! (which matters on Windows, where an open handle blocks directory removal).
//!
//! A real `CTRL_C_EVENT` cannot be delivered to another process from a test on
//! Windows, so these drive `serve_with_shutdown` with a channel instead. What
//! that still covers is the whole composition: signal -> log -> `shutdown`
//! broadcast -> axum drain -> bounded grace period -> return.

use std::time::Duration;

use weflow_server::config::Config;

/// Reserve a free port by binding and immediately releasing it. The window
/// between release and re-bind is a race in principle, but on a loopback test
/// port it is far more reliable than hardcoding a number that may be in use.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn test_cfg(dir: &std::path::Path, port: u16) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port,
        log: "info".into(),
        watch_debounce_ms: 20,
        watch_fallback_ms: 0,
        media_export_dir: dir.join("media"),
        base_url: None,
        show_token: false,
        data_dir: dir.join("data"),
    }
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("weflow-shutdown-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Cross-test serialization for the (`TOKEN_SERVICE`, `TOKEN_USER`) keyring
/// entry. Both tests below boot a real server in-process, so both call
/// `load_token()`; on a fresh runner the entry does not exist yet, and two
/// concurrent writers can leave `show_token()` handing one test the OTHER
/// server's token (the 401 this file used to flake on). Serializing spawn +
/// "server is really up" makes each server load a settled entry.
///
/// `tokio::sync::Mutex` rather than `std::sync::Mutex` so the guard can be
/// held across `.await`; the same helper name and shape exist in the qqflow
/// sibling so the two files stay comparable.
async fn credential_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GUARD
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// A server that has passed all three readiness stages.
struct ServerProbe {
    token: String,
}

/// Which stage the predicate last got stuck on. The three have different
/// remedies, and collapsing them into one "server up and token readable"
/// expect (what this file used to do) is exactly what made the flake
/// undiagnosable: a 401 from a credential-store race, a missing keyring entry
/// and a port that never bound all produced the same panic line.
enum UpFailure {
    Port { port: u16, waited: Duration },
    Token { port: u16, waited: Duration },
    Auth {
        port: u16,
        status: u16,
        body: String,
        waited: Duration,
    },
}

impl UpFailure {
    fn remedy(&self) -> &'static str {
        match self {
            Self::Port { .. } => {
                "nothing is listening on that port: check the server task did not exit early, and that another process is not holding the port"
            }
            Self::Token { .. } => {
                "the port accepts connections but the OS credential store returned no token: the test process likely cannot read the store (Windows Credential Manager / libsecret); run --show-token once by hand to confirm the entry is readable"
            }
            Self::Auth { .. } => {
                "a token was read but the server rejected it: the stored token and the running server's token disagree (a credential-store write race between parallel tests); delete the stored entry and re-run"
            }
        }
    }
}

impl std::fmt::Display for UpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (stage, detail) = match self {
            Self::Port { port, waited } => (
                "port",
                format!("no TCP connection on 127.0.0.1:{port} within {waited:?}"),
            ),
            Self::Token { port, waited } => (
                "token",
                format!("127.0.0.1:{port} is up but show_token() returned none within {waited:?}"),
            ),
            Self::Auth {
                port,
                status,
                body,
                waited,
            } => (
                "auth",
                format!("127.0.0.1:{port} answered {status} to an authenticated request within {waited:?} ({body})"),
            ),
        };
        write!(
            f,
            "server never became usable — stuck at stage '{stage}': {detail}\n  remedy: {}",
            self.remedy()
        )
    }
}

/// Wait until the server is *usable*, in three stages:
///
/// 1. the port accepts a TCP connection;
/// 2. `show_token()` returns a token (the credential store is readable);
/// 3. that token authenticates a real request.
///
/// Stage 3 is the one that matters for the historical flake: two parallel
/// tests could both mint tokens before either stored theirs, so the port was
/// up and *a* token was readable — just not the one the server held. A
/// predicate that stops at stage 1 or 2 cannot tell that state from a healthy
/// one; waiting on the authenticated round trip means the test proceeds only
/// when the server would actually accept the token it is about to use.
async fn wait_until_up(port: u16) -> Result<ServerProbe, UpFailure> {
    let started = std::time::Instant::now();
    // Readiness waits on the slowest stage of "boot + credential store + first
    // authenticated round trip", and every stage degrades on a loaded runner:
    // parallel test binaries, a cold credential daemon, antivirus scans. A
    // too-tight budget here turns "slow boot" into a panic that reads as a
    // shutdown failure, so this is the one budget sized for load. The deadlines
    // after the signal are the opposite: they ARE the assertion, so they stay
    // tight.
    let deadline = started + Duration::from_secs(30);
    loop {
        let mut stuck = UpFailure::Port {
            port,
            waited: started.elapsed(),
        };
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            match weflow_server::config::show_token().ok().flatten() {
                Some(token) => match probe_auth(port, &token).await {
                    Ok(()) => return Ok(ServerProbe { token }),
                    Err((status, body)) => {
                        stuck = UpFailure::Auth {
                            port,
                            status,
                            body,
                            waited: started.elapsed(),
                        }
                    }
                },
                None => {
                    stuck = UpFailure::Token {
                        port,
                        waited: started.elapsed(),
                    }
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(stuck);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One authenticated request over a raw socket (no client library, same
/// approach as the SSE handshake below). `/api/v1/accounts` is the probe:
/// it is token-protected and answers 200 with an empty list when no account
/// is registered, so it tests the credential path without depending on an
/// account being bound.
async fn probe_auth(port: u16, token: &str) -> Result<(), (u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| (0, e.to_string()))?;
    // 逐行写、续行符后不留缩进：多出来的前导空格会让请求行/头部非法，
    // 服务端回 400 而不是 401——那会把「凭据不对」误报成「请求写坏了」。
    let req = format!(
        "GET /api/v1/accounts HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    sock.write_all(req.as_bytes())
        .await
        .map_err(|e| (0, e.to_string()))?;
    sock.flush().await.map_err(|e| (0, e.to_string()))?;
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err((status, text.lines().next().unwrap_or("").to_string()))
    }
}

/// The signal must actually stop the server, and it must do so well inside the
/// grace period when nothing is holding a connection open.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_signal_stops_the_server() {
    let _credential = credential_guard().await;
    let dir = tmp_dir("basic");
    let port = free_port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let cfg = test_cfg(&dir, port);
    let server = tokio::spawn(async move {
        weflow_server::serve_with_shutdown(cfg, async move {
            let _ = rx.await;
        })
        .await
    });

    // Wait until it is actually serving authenticated traffic, so the shutdown
    // races a live listener rather than an unbound socket — and so a failure
    // reports which stage stalled (port / token / auth) instead of one generic
    // "never came up" line.
    let probe = wait_until_up(port).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(!probe.token.is_empty(), "probe token must not be empty");

    let started = std::time::Instant::now();
    tx.send(()).expect("shutdown trigger delivered");
    let result = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server must stop after the shutdown signal")
        .expect("server task must not panic");
    result.expect("serve_with_shutdown returned an error");

    // With no connection held open, axum drains immediately: this must NOT
    // take the full grace period.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "idle shutdown should be prompt, took {:?}",
        started.elapsed()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An open SSE stream must not hold shutdown hostage. `with_graceful_shutdown`
/// waits for every in-flight connection, and an SSE response never ends on its
/// own — so without both the `shutdown` broadcast (which closes the stream from
/// the handler side) and the bounded grace period, Ctrl+C would hang for as
/// long as a client stayed subscribed.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_ends_a_live_sse_stream_within_the_grace_period() {
    let _credential = credential_guard().await;
    let dir = tmp_dir("sse");
    let port = free_port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let cfg = test_cfg(&dir, port);
    let server = tokio::spawn(async move {
        weflow_server::serve_with_shutdown(cfg, async move {
            let _ = rx.await;
        })
        .await
    });

    // The token is minted inside serve_with_shutdown from the credential store.
    // The predicate hands it back only after it has authenticated a real
    // request, so the handshake below cannot use a token the server will
    // reject — which is what the old inline loop could not rule out.
    let token = wait_until_up(port)
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .token;

    // Hold an SSE stream open with a raw socket: no client library, and the
    // response body is deliberately never drained to completion.
    let mut sse = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("SSE connect");
    {
        use tokio::io::AsyncWriteExt;
        let req = format!(
            "GET /api/v1/push/messages?access_token={token} HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\r\n"
        );
        sse.write_all(req.as_bytes()).await.expect("send SSE request");
        sse.flush().await.unwrap();
    }
    // Read enough to be sure the stream is established (headers + `ready`).
    {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(Duration::from_secs(5), sse.read(&mut buf))
            .await
            .expect("SSE response arrived")
            .expect("SSE read");
        let head = String::from_utf8_lossy(&buf[..n]);
        assert!(head.contains("200"), "SSE handshake: {head}");
        assert!(
            head.contains("text/event-stream"),
            "SSE content-type: {head}"
        );
    }

    let started = std::time::Instant::now();
    tx.send(()).expect("shutdown trigger delivered");
    let result = tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .expect("a live SSE stream must not block shutdown past the timeout")
        .expect("server task must not panic");
    result.expect("serve_with_shutdown returned an error");
    let elapsed = started.elapsed();

    // Must be well under SHUTDOWN_GRACE (3s), not merely under some generous
    // ceiling: landing AT the grace period means the stream never closed
    // itself and the timer force-exited instead — which is the bug this test
    // exists to catch. Verified by removing the `shutdown` broadcast: the
    // figure goes from sub-millisecond to 3.008s.
    assert!(
        elapsed < Duration::from_millis(1500),
        "the shutdown broadcast must close the SSE stream, not the grace timer; \
         took {elapsed:?} (grace period is 3s)"
    );
    println!("[shutdown] live SSE stream released in {elapsed:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
