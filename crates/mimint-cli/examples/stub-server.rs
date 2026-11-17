//! Canned stub OpenAI-compatible server for manual CLI runs (std-only).
//!
//! Listens only on 127.0.0.1 and never calls out. The first chat request
//! gets a canned `notebook_add` tool call; later requests get canned final
//! text. Start a fresh stub per manual run: the tool-call script runs once.
//!
//! Usage: `stub-server [port]` (default 18099, or env `STUB_PORT`).

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

fn stub_response(call: usize) -> String {
    if call == 1 {
        r#"{"id":"chatcmpl-stub","object":"chat.completion","created":0,"model":"stub","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_stub_1","type":"function","function":{"name":"notebook_add","arguments":"{\"title\":\"Stub note\",\"body\":\"Written by stub e2e\"}"}}]},"finish_reason":"tool_calls"}]}"#.to_string()
    } else {
        r#"{"id":"chatcmpl-stub","object":"chat.completion","created":0,"model":"stub","choices":[{"index":0,"message":{"role":"assistant","content":"Stub done: note saved."},"finish_reason":"stop"}]}"#.to_string()
    }
}

fn handle(mut stream: std::net::TcpStream, call: usize) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut content_length = 0usize;
    let mut expect_continue = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break;
                }
                if let Some((key, value)) = trimmed.split_once(':') {
                    if key.trim().eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    } else if key.trim().eq_ignore_ascii_case("expect")
                        && value.trim().eq_ignore_ascii_case("100-continue")
                    {
                        expect_continue = true;
                    }
                }
            }
        }
    }
    if expect_continue {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    eprintln!("stub call {call}: {}", request_line.trim());
    let payload = stub_response(call);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn main() {
    let port_arg = env::args().nth(1).or_else(|| env::var("STUB_PORT").ok());
    let port: u16 = match port_arg {
        None => 18099,
        Some(text) => match text.parse() {
            Ok(port) => port,
            Err(_) => {
                eprintln!("stub: invalid port: {text}");
                std::process::exit(1);
            }
        },
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("stub: cannot bind 127.0.0.1:{port}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("stub listening on 127.0.0.1:{port}");
    let counter = AtomicUsize::new(0);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
                handle(stream, call);
            }
            Err(err) => eprintln!("stub accept error: {err}"),
        }
    }
}
