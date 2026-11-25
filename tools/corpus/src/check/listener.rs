//! The canary listener: a TCP socket on the loopback address that the
//! canary files point to (plan 005 §11). It counts every connection and
//! keeps the first request line, then answers 404 and closes. Any
//! connection fails the case that was running.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct Listener {
    addr: SocketAddr,
    count: Arc<AtomicU64>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Listener {
    /// Binds `addr` and accepts on a thread of its own. A failed bind is an
    /// error: without the listener, zero connections proves nothing.
    pub fn start(addr: &str) -> Result<Self, String> {
        let socket = TcpListener::bind(addr)
            .map_err(|err| format!("failed to bind the canary listener on {addr}: {err}"))?;
        let addr = socket
            .local_addr()
            .map_err(|err| format!("failed to read the listener address: {err}"))?;
        let count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (thread_count, thread_seen) = (Arc::clone(&count), Arc::clone(&seen));
        thread::Builder::new()
            .name("canary-listener".into())
            .spawn(move || {
                for stream in socket.incoming().flatten() {
                    thread_count.fetch_add(1, Ordering::SeqCst);
                    let line = first_line(stream);
                    if let Ok(mut seen) = thread_seen.lock() {
                        seen.push(line);
                    }
                }
            })
            .map_err(|err| format!("failed to start the listener thread: {err}"))?;
        Ok(Self { addr, count, seen })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Connections since the start.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    /// The first request line of each connection, in order.
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Reads up to 512 bytes for one second, answers 404 and returns the first line.
fn first_line(mut stream: TcpStream) -> String {
    let peer = stream
        .peer_addr()
        .map(|p| p.to_string())
        .unwrap_or_else(|_| "unknown peer".into());
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).unwrap_or(0);
    let _ = stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let text = String::from_utf8_lossy(&buf[..n]);
    format!("{peer}: {}", text.lines().next().unwrap_or("").trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_connection_is_counted_and_its_first_line_kept() {
        let listener = Listener::start("127.0.0.1:0").expect("bind");
        let mut client = TcpStream::connect(listener.addr()).expect("connect");
        client
            .write_all(b"GET /odt-basic HTTP/1.1\r\n\r\n")
            .expect("write");
        let mut answer = String::new();
        let _ = client.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
        let until = Instant::now() + Duration::from_secs(5);
        while listener.seen().is_empty() && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(listener.count(), 1);
        assert!(
            listener.seen()[0].ends_with("GET /odt-basic HTTP/1.1"),
            "{:?}",
            listener.seen()
        );
    }

    #[test]
    fn a_taken_address_is_an_error() {
        let first = Listener::start("127.0.0.1:0").expect("bind");
        let second = Listener::start(&first.addr().to_string());
        assert!(second.is_err());
    }
}
