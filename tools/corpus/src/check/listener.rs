//! The canary listener: a TCP socket on the loopback address that the
//! canary files point to. It counts every connection and
//! keeps the first request line, then answers 404 and closes. Any
//! connection fails the case that was running.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

    /// Waits until the listener has counted `n` connections in all, or
    /// until `timeout`. Returns the count it saw last.
    pub fn wait_for(&self, n: u64, timeout: Duration) -> u64 {
        let until = Instant::now() + timeout;
        loop {
            let count = self.count();
            if count >= n || Instant::now() >= until {
                return count;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The listener control: connects to the listener from this process
    /// and requires that it counts exactly that one connection. Run it
    /// before any case, in the network namespace of the service, so that
    /// a count of 0 later means that nothing connected, not that the
    /// listener could not see a connection.
    pub fn control(&self, timeout: Duration) -> Result<String, String> {
        let before = self.count();
        let mut client = TcpStream::connect_timeout(&self.addr, timeout)
            .map_err(|err| format!("failed to connect to the listener on {}: {err}", self.addr))?;
        let _ = client.set_read_timeout(Some(timeout));
        client
            .write_all(CONTROL_REQUEST)
            .map_err(|err| format!("failed to write to the listener: {err}"))?;
        let mut answer = Vec::new();
        let _ = client.read_to_end(&mut answer);
        let count = self.wait_for(before + 1, timeout);
        // The line is stored after the answer, so give it the same time.
        let until = Instant::now() + timeout;
        while self.seen().len() < count as usize && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        if count != before + 1 {
            return Err(format!(
                "the listener control saw {} connection(s), expected 1",
                count - before
            ));
        }
        Ok(self.seen().last().cloned().unwrap_or_default())
    }
}

/// What the control connection sends, so that its line is easy to tell apart.
const CONTROL_REQUEST: &[u8] = b"GET /corpus-bench-listener-control HTTP/1.1\r\n\r\n";

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
    fn the_control_counts_its_own_connection() {
        let listener = Listener::start("127.0.0.1:0").expect("bind");
        let line = listener.control(Duration::from_secs(5)).expect("control");
        assert!(
            line.ends_with("GET /corpus-bench-listener-control HTTP/1.1"),
            "{line}"
        );
        assert_eq!(listener.count(), 1);
    }

    #[test]
    fn wait_for_gives_up_at_the_timeout() {
        let listener = Listener::start("127.0.0.1:0").expect("bind");
        assert_eq!(listener.wait_for(1, Duration::from_millis(50)), 0);
    }

    #[test]
    fn a_taken_address_is_an_error() {
        let first = Listener::start("127.0.0.1:0").expect("bind");
        let second = Listener::start(&first.addr().to_string());
        assert!(second.is_err());
    }
}
