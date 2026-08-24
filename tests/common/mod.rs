use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A minimal single-purpose HTTP/1.1 stub: every request gets the same
/// canned JSON body with status 200. Good enough to stand in for a
/// provider's `/models` endpoint without pulling in a real HTTP server
/// dependency — WP3's LLM client test stub will need the same shape.
/// Only `tests/gating.rs` uses this today; `#[allow(dead_code)]` because
/// each integration-test file compiles `common` as its own crate.
#[allow(dead_code)]
pub struct StubServer {
    pub base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

#[allow(dead_code)]
impl StubServer {
    pub fn start(response_body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
        let addr = listener.local_addr().expect("local_addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_clone = requests.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }

                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_length > 0 {
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body);
                }

                requests_clone
                    .lock()
                    .unwrap()
                    .push(request_line.trim().to_string());

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    /// Request lines seen so far, e.g. `"GET /models HTTP/1.1"`.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// One canned response for `RoutedStubServer`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct StubResponse {
    pub status: u16,
    pub body: String,
}

#[allow(dead_code)]
impl StubResponse {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// A stub HTTP server that answers by exact request path, cycling through a
/// per-path queue of canned responses (repeating the last one once
/// exhausted). WP3's LLM client and full-`run` tests need both a `/models`
/// route (fixed catalogue) and a `/chat/completions` route (a sequenced
/// trace of assistant turns) served by one server, which the single-response
/// `StubServer` above can't do.
#[allow(dead_code)]
pub struct RoutedStubServer {
    pub base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<String>>>,
}

#[allow(dead_code)]
impl RoutedStubServer {
    pub fn start(routes: HashMap<&'static str, Vec<StubResponse>>) -> Self {
        Self::start_with_delay(routes, Duration::ZERO)
    }

    /// Same as `start`, but sleeps `delay` before writing each response —
    /// used to widen the window for "kill the process mid-run" tests.
    pub fn start_with_delay(routes: HashMap<&'static str, Vec<StubResponse>>, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
        let addr = listener.local_addr().expect("local_addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let requests_clone = requests.clone();
        let bodies_clone = bodies.clone();
        let routes: HashMap<String, Vec<StubResponse>> =
            routes.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }

                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = Vec::new();
                if content_length > 0 {
                    body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body);
                }

                let trimmed_line = request_line.trim().to_string();
                requests_clone.lock().unwrap().push(trimmed_line.clone());
                bodies_clone
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());

                let path = trimmed_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .to_string();

                let response = match routes.get(&path) {
                    Some(responses) if !responses.is_empty() => {
                        let mut counters_guard = counters.lock().unwrap();
                        let idx = counters_guard.entry(path.clone()).or_insert(0);
                        let chosen = responses[(*idx).min(responses.len() - 1)].clone();
                        *idx += 1;
                        chosen
                    }
                    _ => StubResponse::json(404, "{}"),
                };

                if !delay.is_zero() {
                    thread::sleep(delay);
                }

                let http_response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    status_text(response.status),
                    response.body.len(),
                    response.body
                );
                let _ = stream.write_all(http_response.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            requests,
            bodies,
        }
    }

    /// Request lines seen so far, e.g. `"POST /chat/completions HTTP/1.1"`.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Request bodies seen so far, in the same order as `requests()`.
    pub fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

pub fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_codemason")
}

static NEXT_TEMP_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A fresh temp dir for use as a child process's cwd and/or
/// `CODEMASON_CACHE_DIR`, unique per call so parallel tests don't collide.
pub fn temp_dir(label: &str) -> std::path::PathBuf {
    let n = NEXT_TEMP_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "codemason-it-{label}-{}-{n}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn codemason(cwd: &std::path::Path) -> Command {
    let mut cmd = Command::new(bin_path());
    cmd.current_dir(cwd);
    cmd
}
