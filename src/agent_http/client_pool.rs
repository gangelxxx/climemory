//! Transport-only reuse. Credentials and total deadlines belong to requests.
use crate::config::ProxyConfig;
use crate::util::{AppError, Result};
use reqwest::blocking::Client;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const CAPACITY: usize = 8;

#[derive(PartialEq, Eq)]
struct Key {
    endpoint: String,
    proxy: Option<String>,
    connect_timeout: Duration,
}

#[derive(Default)]
struct Pool(VecDeque<(Key, Client)>);

impl Pool {
    fn get(&mut self, key: Key, proxy: &ProxyConfig) -> Result<Client> {
        if let Some((_, client)) = self.0.iter().find(|(existing, _)| existing == &key) {
            return Ok(client.clone());
        }
        let builder = Client::builder()
            .connect_timeout(key.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        let client = proxy
            .apply(builder)?
            .build()
            .map_err(|_| AppError::new("cannot initialize HTTP agent client"))?;
        if self.0.len() == CAPACITY {
            self.0.pop_front();
        }
        self.0.push_back((key, client.clone()));
        Ok(client)
    }
}

pub(super) fn get(
    endpoint: &str,
    proxy: &ProxyConfig,
    connect_timeout: Duration,
) -> Result<Client> {
    proxy.validate()?;
    let key = Key {
        endpoint: endpoint.to_owned(),
        proxy: proxy.enabled.then(|| proxy.url.clone()),
        connect_timeout,
    };
    static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Pool::default()))
        .lock()
        .map_err(|_| AppError::new("HTTP client pool unavailable"))?
        .get(key, proxy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn serve(listener: TcpListener, requests: usize) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut received = Vec::new();
            for _ in 0..requests {
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                received.push(headers);
                reader
                    .get_mut()
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .unwrap();
            }
            received
        })
    }

    #[test]
    fn repeated_requests_reuse_connection_without_reusing_authorization() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = serve(listener, 3);
        for key in [Some("first-test-key"), Some("second-test-key"), None] {
            let client = get(&endpoint, &ProxyConfig::default(), Duration::from_secs(1)).unwrap();
            let mut request = client.get(&endpoint).timeout(Duration::from_secs(2));
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            assert_eq!(request.send().unwrap().text().unwrap(), "ok");
        }
        let requests = server.join().unwrap();
        assert!(requests[0].contains("Bearer first-test-key"));
        assert!(requests[1].contains("Bearer second-test-key"));
        assert!(!requests[1].contains("first-test-key"));
        assert!(!requests[2].to_lowercase().contains("authorization:"));
    }

    #[test]
    fn proxy_transport_is_isolated_and_request_deadline_is_not_cached() {
        let direct = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", direct.local_addr().unwrap());
        let direct_server = serve(direct, 1);
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_config = ProxyConfig {
            enabled: true,
            url: format!("http://{}", proxy.local_addr().unwrap()),
        };
        let proxy_server = serve(proxy, 1);
        for config in [ProxyConfig::default(), proxy_config] {
            let client = get(&endpoint, &config, Duration::from_secs(1)).unwrap();
            assert_eq!(
                client
                    .get(&endpoint)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .unwrap()
                    .text()
                    .unwrap(),
                "ok"
            );
        }
        assert!(direct_server.join().unwrap()[0].starts_with("GET / HTTP/1.1"));
        assert!(proxy_server.join().unwrap()[0].starts_with(&format!("GET {endpoint}/ HTTP/1.1")));

        let slow = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", slow.local_addr().unwrap());
        let (finish, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_stream, _) = slow.accept().unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        let client = get(&endpoint, &ProxyConfig::default(), Duration::from_secs(1)).unwrap();
        let error = client
            .get(endpoint)
            .timeout(Duration::from_millis(50))
            .send()
            .unwrap_err();
        assert!(error.is_timeout());
        let _ = finish.send(());
        server.join().unwrap();
    }

    #[test]
    fn cache_size_is_bounded() {
        let mut pool = Pool::default();
        for i in 0..CAPACITY + 2 {
            pool.get(
                Key {
                    endpoint: format!("http://127.0.0.1:{i}"),
                    proxy: None,
                    connect_timeout: Duration::from_secs(1),
                },
                &ProxyConfig::default(),
            )
            .unwrap();
        }
        assert_eq!(pool.0.len(), CAPACITY);
        assert_eq!(pool.0.front().unwrap().0.endpoint, "http://127.0.0.1:2");
    }
}
