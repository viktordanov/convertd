//! The connection bounds before any request is read (011 S4): the header
//! read timeout, the connection cap with its probe reserve, and the
//! graceful stop of `server::serve`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing;
use convertd::config::{Config, Env};
use convertd::engines::fake::{FakeBehavior, FakeOffice, FakePdf, FakeReadiness};
use convertd::rest::server::{ConnectionLimits, PROBE_CONNECTIONS, Served, serve};
use convertd::rest::state::{AppState, Engines};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// AGENT-RULES 13a: every await in these tests has a deadline.
const DEADLINE: Duration = Duration::from_secs(10);

fn state(name: &str) -> AppState {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("connections-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = dir.to_str().unwrap().to_owned();
    let config = Config::from_env(&Env::from_pairs(
        [
            ("API_KEY", "test-key"),
            ("TMP_DIR", tmp.as_str()),
            ("BUILD_INFO_PATH", "/nonexistent/build-info.json"),
            ("FONTS_PATH", "/nonexistent/fonts.json"),
        ],
        2,
    ))
    .unwrap();
    AppState::new(
        &config,
        Engines {
            office: Arc::new(FakeOffice::new(FakeBehavior::Succeed)),
            pdf: Arc::new(FakePdf::new(FakeBehavior::Succeed)),
            readiness: Arc::new(FakeReadiness::new(true)),
        },
    )
}

/// A server on a free loopback port; dropping the sender stops it.
async fn start(
    name: &str,
    limits: ConnectionLimits,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<std::io::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    // An extra route: the reserve's gate holds for any route of the app.
    let extra = Router::new().route("/extra", routing::get(|| async { "extra" }));
    let served = tokio::spawn(serve(
        listener,
        Served::with(state(name), extra),
        limits,
        async move {
            let _ = stopped.await;
        },
    ));
    (addr, stop, served)
}

/// Sends one request with `Connection: close` and returns the status line.
async fn get(addr: SocketAddr, path: &str) -> String {
    get_full(addr, path)
        .await
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Sends one request with `Connection: close` and returns the response.
async fn get_full(addr: SocketAddr, path: &str) -> String {
    let mut stream = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .expect("connect timed out")
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(DEADLINE, stream.read_to_end(&mut response))
        .await
        .expect("the response timed out")
        .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// Waits until the peer closes `stream`, and returns how long that took.
/// One deadline covers the whole wait, however much the peer sends.
async fn closed_within(stream: &mut TcpStream, limit: Duration) -> Duration {
    let started = tokio::time::Instant::now();
    let mut buf = [0u8; 256];
    let drained = timeout(limit, async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(drained.is_ok(), "still open after {limit:?}");
    started.elapsed()
}

/// Waits until the peer closes `stream` without sending a byte.
async fn closed_unanswered(stream: &mut TcpStream, limit: Duration) {
    let mut buf = [0u8; 256];
    match timeout(limit, stream.read(&mut buf)).await {
        Err(_) => panic!("still open after {limit:?}"),
        Ok(Ok(0) | Err(_)) => {}
        Ok(Ok(n)) => panic!("answered: {:?}", String::from_utf8_lossy(&buf[..n])),
    }
}

#[tokio::test]
async fn half_a_request_line_is_closed_at_the_header_timeout() {
    let limits = ConnectionLimits {
        header_read_timeout: Duration::from_secs(1),
        max_connections: 8,
    };
    let (addr, _stop, _served) = start("half", limits).await;
    let mut stream = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .unwrap()
        .unwrap();
    stream.write_all(b"GET /heal").await.unwrap();
    let took = closed_within(&mut stream, Duration::from_secs(5)).await;
    assert!(took >= Duration::from_millis(900), "closed after {took:?}");

    // A connection that sends nothing at all, and an idle keep-alive one
    // after its first request, are bounded the same way.
    let mut silent = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .unwrap()
        .unwrap();
    closed_within(&mut silent, Duration::from_secs(5)).await;
    let mut kept = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .unwrap()
        .unwrap();
    kept.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    closed_within(&mut kept, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn with_the_cap_full_the_probes_still_answer() {
    let limits = ConnectionLimits {
        header_read_timeout: Duration::from_secs(30),
        max_connections: 2,
    };
    let (addr, _stop, _served) = start("cap", limits).await;
    // Two idle connections hold the cap.
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(
            timeout(DEADLINE, TcpStream::connect(addr))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    // Let the server accept them before the next one.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(get(addr, "/healthz").await, "HTTP/1.1 200 OK");
    assert_eq!(get(addr, "/readyz").await, "HTTP/1.1 200 OK");
    assert_eq!(
        get(addr, "/extra").await,
        "HTTP/1.1 503 Service Unavailable"
    );
    // Any other route on a reserve connection is refused, with the usual
    // error body.
    let refused = get_full(addr, "/v1/capabilities").await;
    assert!(
        refused.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{refused}"
    );
    assert!(
        refused.contains(r#""code":"SERVICE_UNAVAILABLE""#) && refused.contains("connections"),
        "{refused}"
    );

    // With the reserve full too, a new connection is closed unread.
    for _ in 0..PROBE_CONNECTIONS {
        held.push(
            timeout(DEADLINE, TcpStream::connect(addr))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut over = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .unwrap()
        .unwrap();
    let _ = over
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await;
    closed_unanswered(&mut over, Duration::from_secs(5)).await;

    // The reserve's idle connections close at its 2 s header timeout, so
    // its places free again while the cap stays full.
    for reserve in held.iter_mut().skip(2) {
        let took = closed_within(reserve, Duration::from_secs(5)).await;
        assert!(
            took <= Duration::from_secs(4),
            "a reserve place held {took:?}"
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(get(addr, "/healthz").await, "HTTP/1.1 200 OK");

    // A held connection that closes frees its place.
    held.clear();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        get(addr, "/v1/capabilities").await,
        "HTTP/1.1 401 Unauthorized"
    );
}

#[tokio::test]
async fn the_stop_closes_idle_connections_and_returns() {
    let limits = ConnectionLimits {
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
    };
    let (addr, stop, served) = start("drain", limits).await;
    // A keep-alive connection that served one request and is now idle.
    let mut kept = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .unwrap()
        .unwrap();
    kept.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 512];
    let n = timeout(DEADLINE, kept.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(buf[..n].starts_with(b"HTTP/1.1 200 OK"));

    drop(stop);
    // The idle connection closes at once, well before its header timeout,
    // and serve returns.
    closed_within(&mut kept, Duration::from_secs(5)).await;
    timeout(DEADLINE, served)
        .await
        .expect("serve did not return")
        .unwrap()
        .unwrap();
    assert!(
        timeout(DEADLINE, TcpStream::connect(addr))
            .await
            .unwrap()
            .is_err(),
        "the listener still accepts"
    );
}

#[tokio::test]
async fn a_refused_upload_on_the_reserve_still_gets_its_answer() {
    // The cap is full; a convert upload in progress on a reserve
    // connection gets the 503 with a request id, and its body is drained,
    // so the client reads the answer instead of a reset.
    let limits = ConnectionLimits {
        header_read_timeout: Duration::from_secs(30),
        max_connections: 1,
    };
    let (addr, _stop, _served) = start("reserve-upload", limits).await;
    let _held = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut upload = timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .unwrap()
        .unwrap();
    let head = "POST /v1/convert HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-key\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: 16777216\r\n\r\n";
    upload.write_all(head.as_bytes()).await.unwrap();
    // Part of the body, written before the answer is read.
    let chunk = vec![b'x'; 64 << 10];
    for _ in 0..4 {
        timeout(DEADLINE, upload.write_all(&chunk))
            .await
            .unwrap()
            .unwrap();
    }
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let read = timeout(DEADLINE, async {
        loop {
            match upload.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                    if response.windows(4).any(|w| w == b"\r\n\r\n") && response.ends_with(b"}") {
                        break;
                    }
                }
            }
        }
    })
    .await;
    assert!(read.is_ok(), "no answer within {DEADLINE:?}");
    let text = String::from_utf8_lossy(&response).to_lowercase();
    assert!(text.starts_with("http/1.1 503"), "{text}");
    assert!(text.contains("x-request-id: "), "{text}");
    assert!(text.contains(r#""code":"service_unavailable""#), "{text}");
    // The rest of the 16 MiB, far more than the socket buffers hold, still
    // goes through: the body is drained, not cut, so the client's writes
    // succeed and it could read the answer.
    let mut left = (16usize << 20) - 4 * chunk.len();
    let finished = timeout(DEADLINE, async {
        while left > 0 {
            let n = left.min(chunk.len());
            upload.write_all(&chunk[..n]).await?;
            left -= n;
        }
        upload.flush().await
    })
    .await;
    assert!(
        matches!(finished, Ok(Ok(()))),
        "the upload was cut with {left} bytes left: {finished:?}"
    );
}
