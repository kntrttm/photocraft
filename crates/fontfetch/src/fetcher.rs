//! The blocking HTTPS fetcher behind `type.fonts.install`: ureq with rustls, timeouts, a hard
//! body cap, cancellation and a generic `PhotoCraft/<version>` User-Agent (never anything
//! personal).
//!
//! The request runs on a helper thread and the caller polls [`JobCtx::cancelled`] every few
//! milliseconds, so Cancel takes effect even while a read is stalled (the helper then ends at its
//! timeout or its next chunk, discarding what it read).

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use photocraft_engine::font_download_cmds::FontFetcher;
use photocraft_engine::jobs::JobCtx;
use photocraft_engine::{EngineError, Result};

/// How long a request may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// Establishing the connection (DNS, TCP, TLS).
    pub connect: Duration,
    /// The whole request including the body.
    pub overall: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        // 5 minutes moves the largest accepted font (64 MiB) over a 2 Mbit/s line.
        Timeouts { connect: Duration::from_secs(15), overall: Duration::from_secs(300) }
    }
}

/// `ureq`-based [`FontFetcher`]. HTTPS only.
#[derive(Clone)]
pub struct UreqFetcher {
    agent: ureq::Agent,
}

/// The User-Agent every request sends.
pub fn user_agent() -> String {
    format!("PhotoCraft/{}", env!("CARGO_PKG_VERSION"))
}

impl Default for UreqFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqFetcher {
    pub fn new() -> Self {
        Self::with_timeouts(Timeouts::default())
    }

    pub fn with_timeouts(t: Timeouts) -> Self {
        Self::build(t, true)
    }

    fn build(t: Timeouts, https_only: bool) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(https_only)
            .user_agent(user_agent())
            .timeout_connect(Some(t.connect))
            .timeout_global(Some(t.overall))
            .max_redirects(3)
            // Non-2xx statuses are errors.
            .http_status_as_error(true)
            .build();
        UreqFetcher { agent: ureq::Agent::new_with_config(config) }
    }

    /// Plain `http://` allowed, for the loopback test servers only. Production is HTTPS only.
    #[cfg(test)]
    pub(crate) fn insecure_for_tests(t: Timeouts) -> Self {
        Self::build(t, false)
    }
}

fn describe(e: &ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code) => format!("HTTP {code}"),
        ureq::Error::Timeout(_) => "timed out".to_string(),
        other => other.to_string(),
    }
}

fn too_large(max: u64) -> EngineError {
    EngineError::Other(format!("the download is larger than the {max} bytes allowed"))
}

/// One download, on the helper thread: stops reading when `stop` is set.
fn download(agent: &ureq::Agent, url: &str, max: u64, stop: &AtomicBool) -> Result<Vec<u8>> {
    let mut resp = agent.get(url).call().map_err(|e| EngineError::Other(describe(&e)))?;
    if resp.body().content_length().is_some_and(|n| n > max) {
        return Err(too_large(max));
    }
    // Read at most max + 1 bytes: one byte over the cap is an error, not a truncation.
    let mut reader = resp.body_mut().with_config().limit(max.saturating_add(1)).reader();
    let mut out: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(EngineError::Cancelled);
        }
        let n = reader.read(&mut buf).map_err(|e| EngineError::Other(format!("reading the response failed: {e}")))?;
        if n == 0 {
            return Ok(out);
        }
        if (out.len() as u64).saturating_add(n as u64) > max {
            return Err(too_large(max));
        }
        out.extend_from_slice(buf.get(..n).unwrap_or_default());
    }
}

impl FontFetcher for UreqFetcher {
    fn get(&self, url: &str, max_bytes: u64, ctx: &JobCtx) -> Result<Vec<u8>> {
        ctx.check()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (agent, owned_url, flag) = (self.agent.clone(), url.to_string(), stop.clone());
        std::thread::Builder::new()
            .name("photocraft-font-fetch".into())
            .spawn(move || {
                // The receiver is gone after a cancel: nobody wants the result.
                let _ = tx.send(download(&agent, &owned_url, max_bytes, &flag));
            })
            .map_err(|e| EngineError::Other(format!("could not start the download: {e}")))?;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(r) => return r,
                Err(RecvTimeoutError::Timeout) => {
                    if ctx.cancelled() {
                        stop.store(true, Ordering::Relaxed);
                        return Err(EngineError::Cancelled);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(EngineError::Other("the download stopped unexpectedly".into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    use super::*;

    /// How the stub answers a request for `path`.
    type Handler = dyn Fn(&mut TcpStream, &str, &str) + Send + Sync + 'static;

    /// A one-thread-per-connection HTTP/1.1 server on 127.0.0.1 (plain http: tests only).
    fn serve(handler: impl Fn(&mut TcpStream, &str, &str) + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handler: Arc<Handler> = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let h = handler.clone();
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match std::io::Read::read(&mut stream, &mut b) {
                            Ok(1) => head.push(b[0]),
                            _ => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                    h(&mut stream, &path, &text);
                });
            }
        });
        format!("http://{addr}")
    }

    fn ok(s: &mut TcpStream, body: &[u8]) {
        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = s.write_all(body);
    }

    fn fast() -> UreqFetcher {
        UreqFetcher::insecure_for_tests(Timeouts { connect: Duration::from_secs(2), overall: Duration::from_secs(10) })
    }

    #[test]
    fn fetches_a_body_and_sends_the_generic_user_agent() {
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen2 = seen.clone();
        let base = serve(move |s, _, head| {
            *seen2.lock().unwrap() = head.to_string();
            ok(s, b"hello font");
        });
        let body = fast().get(&format!("{base}/a.ttf"), 1000, &JobCtx::new()).unwrap();
        assert_eq!(body, b"hello font");
        let head = seen.lock().unwrap().to_lowercase();
        assert!(head.contains(&format!("user-agent: photocraft/{}", env!("CARGO_PKG_VERSION"))), "{head}");
        assert!(!head.contains("cookie") && !head.contains("authorization"));
    }

    #[test]
    fn the_cap_is_enforced_with_and_without_a_content_length() {
        let base = serve(|s, path, _| match path {
            "/exact" => ok(s, &[1u8; 100]),
            "/over" => ok(s, &[1u8; 101]),
            // No Content-Length: the body ends when the server closes, and is far larger than the cap.
            _ => {
                let _ = write!(s, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
                let chunk = [7u8; 4096];
                for _ in 0..1024 {
                    if s.write_all(&chunk).is_err() {
                        break;
                    }
                }
            }
        });
        let f = fast();
        let ctx = JobCtx::new();
        assert_eq!(f.get(&format!("{base}/exact"), 100, &ctx).unwrap().len(), 100);
        let e = f.get(&format!("{base}/over"), 100, &ctx).unwrap_err().to_string();
        assert!(e.contains("larger than"), "{e}");
        let e = f.get(&format!("{base}/stream"), 10_000, &ctx).unwrap_err().to_string();
        assert!(e.contains("larger than") || e.contains("limit"), "{e}");
    }

    #[test]
    fn non_2xx_is_an_error() {
        let base = serve(|s, path, _| {
            let status = if path == "/missing" { "404 Not Found" } else { "500 Internal Server Error" };
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops");
        });
        let ctx = JobCtx::new();
        let e = fast().get(&format!("{base}/missing"), 1000, &ctx).unwrap_err().to_string();
        assert!(e.contains("404"), "{e}");
        let e = fast().get(&format!("{base}/other"), 1000, &ctx).unwrap_err().to_string();
        assert!(e.contains("500"), "{e}");
    }

    #[test]
    fn a_stalled_server_times_out() {
        let base = serve(|s, _, _| {
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc");
            let _ = s.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let f = UreqFetcher::insecure_for_tests(Timeouts { connect: Duration::from_secs(1), overall: Duration::from_millis(400) });
        let t = Instant::now();
        let e = f.get(&format!("{base}/x"), 1000, &JobCtx::new()).unwrap_err().to_string();
        assert!(t.elapsed() < Duration::from_secs(8), "took {:?}", t.elapsed());
        assert!(e.contains("timed out") || e.contains("reading the response failed"), "{e}");
    }

    #[test]
    fn cancel_stops_a_stalled_download_quickly() {
        let base = serve(|s, _, _| {
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc");
            let _ = s.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let ctx = JobCtx::new();
        let c2 = ctx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            c2.cancel();
        });
        let t = Instant::now();
        let r = fast().get(&format!("{base}/x"), 1000, &ctx);
        assert!(matches!(r, Err(EngineError::Cancelled)), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
        // Already cancelled: no request is made at all.
        assert!(matches!(fast().get("http://127.0.0.1:1/x", 10, &ctx), Err(EngineError::Cancelled)));
    }

    #[test]
    fn connection_failures_are_errors() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let r = fast().get(&format!("http://127.0.0.1:{port}/x"), 10, &JobCtx::new());
        assert!(matches!(r, Err(EngineError::Other(_))), "{r:?}");
        assert!(UreqFetcher::new().get("not a url", 10, &JobCtx::new()).is_err());
    }

    #[test]
    fn production_is_https_only() {
        let base = serve(|s, _, _| ok(s, b"x"));
        let e = UreqFetcher::new().get(&format!("{base}/x"), 10, &JobCtx::new()).unwrap_err().to_string();
        assert!(e.to_lowercase().contains("https"), "{e}");
        assert_eq!(user_agent(), format!("PhotoCraft/{}", env!("CARGO_PKG_VERSION")));
    }
}
