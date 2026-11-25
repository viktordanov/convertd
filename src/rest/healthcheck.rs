//! `convertd healthcheck`: `GET /readyz` on `127.0.0.1:$PORT` without curl
//! and without an HTTP client crate.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// One deadline across connect, write and read.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// Why the service is not ready, or could not be asked.
#[derive(Debug, thiserror::Error)]
pub enum HealthcheckError {
    #[error("failed to connect to {addr}: {source}")]
    Connect { addr: SocketAddr, source: io::Error },
    #[error("failed to send the request: {0}")]
    Send(io::Error),
    #[error("failed to read the response: {0}")]
    Read(io::Error),
    #[error("failed to get an answer within {0:?}")]
    Timeout(Duration),
    #[error("failed readiness: the status line is {0:?}")]
    NotReady(String),
}

/// Returns `Ok` only when `/readyz` answers 200 within `timeout`.
pub fn check(port: u16, timeout: Duration) -> Result<(), HealthcheckError> {
    let deadline = Instant::now() + timeout;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(HealthcheckError::Timeout(timeout))
    };

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|source| HealthcheckError::Connect { addr, source })?;

    let request =
        format!("GET /readyz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .set_write_timeout(Some(remaining()?))
        .map_err(HealthcheckError::Send)?;
    stream
        .write_all(request.as_bytes())
        .map_err(HealthcheckError::Send)?;

    // Only the status line matters; read until its end, at most 256 bytes.
    let mut buf = [0u8; 256];
    let mut len = 0;
    while len < buf.len() && !buf[..len].contains(&b'\n') {
        stream
            .set_read_timeout(Some(remaining()?))
            .map_err(HealthcheckError::Read)?;
        match stream.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(HealthcheckError::Read(err)),
        }
    }

    let line = buf[..len].split(|&b| b == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line).trim_end().to_owned();
    let mut parts = line.split(' ');
    match (parts.next(), parts.next()) {
        (Some(version), Some("200")) if version.starts_with("HTTP/1.") => Ok(()),
        _ => Err(HealthcheckError::NotReady(line)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use super::{HealthcheckError, check};

    /// A one-shot server that answers with `response`, or never if `None`.
    fn server(response: Option<&'static str>) -> (u16, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 512];
            let n = stream.read(&mut request).unwrap();
            match response {
                Some(response) => stream.write_all(response.as_bytes()).unwrap(),
                None => thread::sleep(Duration::from_millis(500)),
            }
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        (port, handle)
    }

    #[test]
    fn ready_on_200() {
        let (port, handle) = server(Some("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n"));
        check(port, Duration::from_secs(2)).unwrap();
        let request = handle.join().unwrap();
        assert!(request.starts_with("GET /readyz HTTP/1.1\r\n"), "{request}");
    }

    #[test]
    fn not_ready_on_503() {
        let (port, handle) = server(Some("HTTP/1.1 503 Service Unavailable\r\n\r\n"));
        let err = check(port, Duration::from_secs(2)).unwrap_err();
        assert!(matches!(err, HealthcheckError::NotReady(_)), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn not_ready_on_404() {
        let (port, handle) = server(Some("HTTP/1.1 404 Not Found\r\n\r\n"));
        assert!(check(port, Duration::from_secs(2)).is_err());
        handle.join().unwrap();
    }

    #[test]
    fn gives_up_at_the_deadline() {
        let (port, handle) = server(None);
        let err = check(port, Duration::from_millis(100)).unwrap_err();
        assert!(
            matches!(
                err,
                HealthcheckError::Read(_) | HealthcheckError::Timeout(_)
            ),
            "{err}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn fails_when_nothing_listens() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let err = check(port, Duration::from_secs(2)).unwrap_err();
        assert!(matches!(err, HealthcheckError::Connect { .. }), "{err}");
    }
}
