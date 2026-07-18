#![cfg(test)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

static MOCK_HTTP_TEST_LOCK: Mutex<()> = Mutex::new(());

pub fn with_mock_http_test_lock<R>(run: impl FnOnce() -> R) -> R {
    let _guard = MOCK_HTTP_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    run()
}

#[derive(Clone, Copy, Debug)]
pub enum MockEndpoint {
    Usage,
    Refresh,
}

pub struct MockHttpServer {
    pub base_url: String,
    usage_responses: Arc<Mutex<Vec<(u16, String)>>>,
    refresh_responses: Arc<Mutex<Vec<(u16, String)>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockHttpServer {
    pub fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("mock server nonblocking");
        let addr = listener.local_addr().expect("mock server addr");
        let usage_responses = Arc::new(Mutex::new(Vec::new()));
        let refresh_responses = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let usage_for_thread = Arc::clone(&usage_responses);
        let refresh_for_thread = Arc::clone(&refresh_responses);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            ready_tx.send(()).ok();
            serve(
                listener,
                usage_for_thread,
                refresh_for_thread,
                shutdown_for_thread,
            );
        });
        ready_rx
            .recv()
            .expect("mock server thread should start promptly");
        Self {
            base_url: format!("http://{addr}"),
            usage_responses,
            refresh_responses,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn enqueue(&self, endpoint: MockEndpoint, status: u16, body: impl Into<String>) {
        let body = body.into();
        match endpoint {
            MockEndpoint::Usage => self
                .usage_responses
                .lock()
                .expect("mock usage responses lock")
                .push((status, body)),
            MockEndpoint::Refresh => self
                .refresh_responses
                .lock()
                .expect("mock refresh responses lock")
                .push((status, body)),
        }
    }

    pub fn usage_url(&self) -> String {
        format!("{}/backend-api/wham/usage", self.base_url)
    }

    pub fn refresh_url(&self) -> String {
        format!("{}/oauth/token", self.base_url)
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    listener: TcpListener,
    usage_responses: Arc<Mutex<Vec<(u16, String)>>>,
    refresh_responses: Arc<Mutex<Vec<(u16, String)>>>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                handle_connection(stream, &usage_responses, &refresh_responses);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    usage_responses: &Arc<Mutex<Vec<(u16, String)>>>,
    refresh_responses: &Arc<Mutex<Vec<(u16, String)>>>,
) {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .expect("mock server stream clone for read"),
    );
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut content_length = 0usize;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("transfer-encoding")
            && value.trim().eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
    } else if chunked && read_chunked_body(&mut reader).is_err() {
        return;
    }

    let responses = if request_line.contains("/oauth/token") {
        refresh_responses
    } else {
        usage_responses
    };
    let (status, body) = {
        let mut guard = responses.lock().expect("mock responses lock");
        guard
            .first()
            .cloned()
            .map(|_| guard.remove(0))
            .unwrap_or((404, r#"{"error":"no mock response"}"#.to_owned()))
    };
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn read_chunked_body(reader: &mut BufReader<TcpStream>) -> std::io::Result<()> {
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size_hex = size_line
            .trim()
            .split_once(';')
            .map(|(size, _)| size)
            .unwrap_or_else(|| size_line.trim());
        let size = usize::from_str_radix(size_hex, 16).unwrap_or(0);
        if size == 0 {
            let mut trailer = String::new();
            reader.read_line(&mut trailer)?;
            return Ok(());
        }
        let mut chunk = vec![0u8; size + 2];
        reader.read_exact(&mut chunk)?;
    }
}
