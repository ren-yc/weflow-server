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

/// The signal must actually stop the server, and it must do so well inside the
/// grace period when nothing is holding a connection open.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_signal_stops_the_server() {
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

    // Wait until it is actually accepting, so the shutdown races a live
    // listener rather than an unbound socket.
    let mut connected = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "server never came up on port {port}");

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

    // The token is minted inside serve_with_shutdown from the credential
    // store, so read it the same way a client would be told to.
    let mut token = None;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            token = weflow_server::config::show_token().ok().flatten();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let token = token.expect("server up and token readable");

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
