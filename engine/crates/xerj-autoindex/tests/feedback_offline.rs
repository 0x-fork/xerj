//! `xerj feedback --submit` against a local mock, with no GitHub token.
//!
//! The preview (`--submit` without `--yes`) must not open a connection. The
//! send (`--submit --yes`) POSTs the JSON body and must not attach a GitHub
//! token or an `Authorization` header — the admin key belongs to the local
//! node, not to this endpoint.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::time::Duration;

use xerj_autoindex::feedback::{parse, run, Cfg};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVar {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVar {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn filled(endpoint: &str, yes: bool) -> Cfg {
    let mut cfg = parse(vec![
        "--submit".into(),
        "--no-autofill".into(),
        "--agent".into(),
        "Test Agent".into(),
        "--used-for".into(),
        "a local mock, no GitHub".into(),
        "--verdict".into(),
        "offline submit works".into(),
    ])
    .expect("parse")
    .expect("cfg");
    if yes {
        cfg.yes = true;
    }
    cfg.endpoint = endpoint.to_string();
    cfg
}

fn read_one_http(listener: TcpListener) -> String {
    let (mut sock, _) = listener.accept().expect("accept");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match sock.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..header_end]);
                    let length = headers.lines().find_map(|line| {
                        let (name, rest) = line.split_once(':')?;
                        if !name.eq_ignore_ascii_case("content-length") {
                            return None;
                        }
                        rest.trim().parse::<usize>().ok()
                    });
                    let body_at = header_end + 4;
                    if let Some(length) = length {
                        while buf.len() < body_at + length {
                            match sock.read(&mut tmp) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        }
                    }
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let response_body = r#"{"ok":true,"stored":true}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len()
    );
    let _ = sock.write_all(response.as_bytes());
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn submit_without_yes_does_not_connect_and_needs_no_github_token() {
    let _lock = ENV_LOCK.lock().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/api/field-report");
    let _gh = EnvVar::set("GH_TOKEN", "ghp_should_not_appear");
    let _github = EnvVar::set("GITHUB_TOKEN", "ghp_should_not_appear");
    let code = run(&filled(&endpoint, false));
    assert_eq!(code, 0);
    match listener.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("--submit without --yes must not connect: {other:?}"),
    }
}

#[test]
fn submit_yes_posts_json_and_sends_no_github_token() {
    let _lock = ENV_LOCK.lock().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/api/field-report");
    let token = "ghp_should_not_appear";
    let _gh = EnvVar::set("GH_TOKEN", token);
    let _github = EnvVar::set("GITHUB_TOKEN", token);
    let server = std::thread::spawn(move || read_one_http(listener));
    let code = run(&filled(&endpoint, true));
    let request = server.join().expect("server thread");
    assert_eq!(code, 0, "request was:\n{request}");
    let header = request
        .split("\r\n\r\n")
        .next()
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        header.starts_with("post /api/field-report http/1.1"),
        "{header}"
    );
    assert!(
        header.contains("content-type: application/json"),
        "{header}"
    );
    assert!(header.contains("user-agent: xerj-feedback"), "{header}");
    assert!(
        !header.contains("authorization"),
        "the node key must not be sent:\n{header}"
    );
    assert!(!request.contains(token), "{request}");
    assert!(request.contains("\"agent\":\"Test Agent\""), "{request}");
    assert!(
        request.contains("\"verdict\":\"offline submit works\""),
        "{request}"
    );
    assert!(
        request.contains("\"used_for\":\"a local mock, no GitHub\""),
        "{request}"
    );
}

#[test]
fn submit_yes_refuses_placeholders_without_connecting() {
    let _lock = ENV_LOCK.lock().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut cfg = parse(vec!["--submit".into(), "--no-autofill".into()])
        .unwrap()
        .unwrap();
    cfg.yes = true;
    cfg.endpoint = format!("http://127.0.0.1:{port}/api/field-report");
    let code = run(&cfg);
    assert_eq!(code, 2);
    match listener.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("a placeholder report must not be posted: {other:?}"),
    }
}
