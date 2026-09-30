use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    time::{Duration, Instant},
};

fn fixture(proxy: &str, endpoint: &str, adapter: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(dir.path())
        .arg("init")
        .output()
        .unwrap()
        .status
        .success());
    fs::write(dir.path().join("memory/config.json"), json!({
        "agent": {
            "proxy":{"enabled":true,"url":proxy},
            "providers":{"test":{"adapter":adapter,"endpoint":endpoint,"allow_remote_content":true}},
            "profiles":{"probe":{"provider":"test","model":"test-model"}}
        },
        "memory":{"timeout_seconds":3,"agent_retries":{"max_attempts":1}}
    }).to_string()).unwrap();
    dir
}

fn run(dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(dir)
        .arg("-test_providers")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .output()
        .unwrap()
}

fn serve(listener: TcpListener, response: String) -> std::thread::JoinHandle<String> {
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "proxy never received a request"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() < 32_768);
        }
        let header = String::from_utf8(header).unwrap();
        if let Some(length) = header.lines().find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        }) {
            assert!(length < 100_000);
            stream.read_exact(&mut vec![0; length]).unwrap();
        }
        stream.write_all(response.as_bytes()).unwrap();
        header
    })
}

#[test]
fn cli_http_request_uses_configured_proxy_even_with_no_proxy_star() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    origin.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        origin.local_addr().unwrap()
    );
    let dir = fixture(
        &format!("http://{}", proxy.local_addr().unwrap()),
        &endpoint,
        "openai-compatible",
    );
    let body =
        json!({"choices":[{"message":{"content":"{\"status\":\"ok\"}"},"finish_reason":"stop"}]})
            .to_string();
    let worker = serve(
        proxy,
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    let output = run(dir.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(worker
        .join()
        .unwrap()
        .starts_with(&format!("POST {endpoint} HTTP/1.1")));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["checks"][0]["status"], "ok");
    assert!(matches!(origin.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn https_uses_connect_and_proxy_refusal_is_not_success() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = fixture(
        &format!("http://{}", proxy.local_addr().unwrap()),
        "https://unresolvable.invalid/v1/chat/completions",
        "openai-compatible",
    );
    let worker = serve(
        proxy,
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    );
    let output = run(dir.path());
    assert!(!output.status.success());
    assert!(worker
        .join()
        .unwrap()
        .starts_with("CONNECT unresolvable.invalid:443 HTTP/1.1"));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["checks"][0]["status"], "error");
}

#[test]
fn cli_adapter_is_rejected_before_starting_when_proxy_is_enabled() {
    let dir = fixture("http://localhost:10809", "http://localhost/", "codex");
    let output = run(dir.path());
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["checks"][0]["error"]
        .as_str()
        .unwrap()
        .contains("cannot guarantee proxy routing"));
}

#[test]
fn cli_http_retries_interrupted_body_and_preserves_generation_header() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = fixture(
        &format!("http://{}", proxy.local_addr().unwrap()),
        "http://127.0.0.1:9/v1/chat/completions",
        "openai-compatible",
    );
    let config_path = dir.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["agent_retries"] =
        json!({"max_attempts":2,"attempt_timeout_seconds":2,"backoff_ms":0});
    config["memory"]["agent_logs"] = json!({"enabled":true});
    fs::write(config_path, config.to_string()).unwrap();
    let worker = std::thread::spawn(move || {
        serve(proxy.try_clone().unwrap(), "HTTP/1.1 200 OK\r\nx-generation-id: gen-interrupted-test\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{".into()).join().unwrap();
        let body = json!({"choices":[{"message":{"content":"{\"status\":\"ok\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":5}}).to_string();
        serve(
            proxy,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
        .join()
        .unwrap();
    });
    let output = run(dir.path());
    assert!(
        worker.join().is_ok(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = fs::read_dir(dir.path().join("memory/runtime/agent-logs"))
        .unwrap()
        .flat_map(|entry| {
            fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"] == "attempt_started")
            .count(),
        2
    );
    let headers = rows
        .iter()
        .find(|row| row["event"] == "http_headers" && row["data"]["attempt"] == 1)
        .unwrap();
    assert_eq!(
        headers["data"]["headers"]["x-generation-id"],
        "gen-interrupted-test"
    );
    assert!(rows
        .iter()
        .any(|row| row["event"] == "http_body_error" && row["data"]["attempt"] == 1));
}

#[test]
fn cli_http_reset_retries_are_bounded_and_bad_request_is_not_retried() {
    for reset in [true, false] {
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let dir = fixture(
            &format!("http://{}", proxy.local_addr().unwrap()),
            "http://127.0.0.1:9/v1/chat/completions",
            "openai-compatible",
        );
        let config_path = dir.path().join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["memory"]["agent_retries"] =
            json!({"max_attempts":2,"attempt_timeout_seconds":2,"backoff_ms":0});
        config["memory"]["agent_logs"] = json!({"enabled":true});
        fs::write(config_path, config.to_string()).unwrap();
        let worker = std::thread::spawn(move || {
            if !reset {
                serve(
                    proxy,
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .into(),
                )
                .join()
                .unwrap();
                return;
            }
            proxy.set_nonblocking(true).unwrap();
            for _ in 0..2 {
                let start = Instant::now();
                let mut stream = loop {
                    if let Ok((stream, _)) = proxy.accept() {
                        break stream;
                    }
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "reset retry did not arrive"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                // Closing with unread POST bytes causes reset rather than clean EOF.
                stream.read_exact(&mut [0; 1]).unwrap();
            }
        });
        let output = run(dir.path());
        assert!(
            worker.join().is_ok(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success());
        let rows: Vec<Value> = fs::read_dir(dir.path().join("memory/runtime/agent-logs"))
            .unwrap()
            .flat_map(|entry| {
                fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            rows.iter()
                .filter(|row| row["event"] == "attempt_started")
                .count(),
            if reset { 2 } else { 1 },
            "reset={reset}: {rows:?}"
        );
    }
}
