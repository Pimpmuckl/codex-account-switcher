#![cfg(test)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct MockHttpServer {
    pub base_url: String,
    responses: Arc<Mutex<Vec<(u16, String)>>>,
    expected: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl MockHttpServer {
    pub fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("mock server nonblocking");
        let addr = listener.local_addr().expect("mock server addr");
        let responses = Arc::new(Mutex::new(Vec::new()));
        let expected = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let handle = thread::spawn({
            let responses = Arc::clone(&responses);
            let expected = Arc::clone(&expected);
            let served = Arc::clone(&served);
            move || serve(listener, responses, expected, served)
        });
        Self {
            base_url: format!("http://{addr}"),
            responses,
            expected,
            served,
            handle: Some(handle),
        }
    }

    pub fn enqueue(&self, status: u16, body: impl Into<String>) {
        self.responses
            .lock()
            .expect("mock responses lock")
            .push((status, body.into()));
        self.expected.fetch_add(1, Ordering::SeqCst);
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
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    listener: TcpListener,
    responses: Arc<Mutex<Vec<(u16, String)>>>,
    expected: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
) {
    let started = std::time::Instant::now();
    let max_runtime = Duration::from_secs(3);
    let idle_shutdown = Duration::from_millis(150);
    let mut idle_since: Option<std::time::Instant> = None;

    while started.elapsed() < max_runtime {
        let served_count = served.load(Ordering::SeqCst);
        let expected_count = expected.load(Ordering::SeqCst);
        if served_count >= expected_count && expected_count > 0 {
            idle_since.get_or_insert_with(std::time::Instant::now);
            if idle_since.is_some_and(|since| since.elapsed() >= idle_shutdown) {
                break;
            }
        } else {
            idle_since = None;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                idle_since = None;
                handle_connection(stream, &responses);
                served.fetch_add(1, Ordering::SeqCst);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
}

fn handle_connection(mut stream: TcpStream, responses: &Arc<Mutex<Vec<(u16, String)>>>) {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .expect("mock server stream clone for read"),
    );
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" {
            break;
        }
    }

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
