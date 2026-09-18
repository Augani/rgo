//! Provider-neutral, synchronous remote CAS transport.
//!
//! This crate deliberately knows nothing about Cargo, SQLite, or the wrapper.  It is used by
//! the daemon to move already-verified immutable bytes.  The client never follows redirects,
//! never prints the bearer token, and bounds every response body before it is allocated.

use std::time::Duration;

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub endpoint: String,
    pub namespace: String,
    pub token: String,
    pub timeout: Duration,
    pub max_object_size: u64,
    pub allow_insecure_loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    Hit(Vec<u8>),
    Miss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteErrorKind {
    Authentication,
    NotFound,
    Conflict,
    RateLimited,
    InvalidResponse,
    PayloadTooLarge,
    Transport,
    Unsupported,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError {
    pub kind: RemoteErrorKind,
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(status) = self.status {
            write!(f, "remote HTTP {status}: {}", self.message)
        } else {
            write!(f, "remote {:?}: {}", self.kind, self.message)
        }
    }
}

impl std::error::Error for RemoteError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutResult {
    pub status: u16,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Client {
    config: Config,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(config: Config) -> Result<Self> {
        validate_endpoint(&config.endpoint, config.allow_insecure_loopback)?;
        if config.namespace.trim().is_empty() || !valid_segment(&config.namespace) {
            bail!("remote namespace must be a non-empty path-safe segment")
        }
        if config.token.is_empty() {
            bail!("remote bearer token is empty")
        }
        if config.max_object_size == 0 {
            bail!("remote max_object_size must be greater than zero")
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(config.timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self { config, agent })
    }

    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    pub fn namespace(&self) -> &str {
        &self.config.namespace
    }

    pub fn probe(&self) -> Result<(), RemoteError> {
        // The CAS API has no provider-specific health endpoint. A read of a reserved,
        // impossible manifest key verifies transport, authentication, and namespace routing;
        // a 404 is a successful probe.
        match self.get("/manifests/__rgo_probe__") {
            Ok(Fetch::Hit(_)) | Ok(Fetch::Miss) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn get_object(&self, digest: &str) -> Result<Fetch, RemoteError> {
        match self.get(&format!("/objects/{}", safe_segment(digest)))? {
            Fetch::Miss => Ok(Fetch::Miss),
            Fetch::Hit(bytes) if blake3::hash(&bytes).to_hex().to_string() == digest => {
                Ok(Fetch::Hit(bytes))
            }
            Fetch::Hit(_) => Err(RemoteError {
                kind: RemoteErrorKind::InvalidResponse,
                status: Some(200),
                message: "remote object digest mismatch".into(),
            }),
        }
    }

    pub fn get_manifest(&self, key: &str) -> Result<Fetch, RemoteError> {
        let (status, body) = self.request(
            "GET",
            &format!("/manifests/{}", safe_segment(key)),
            None,
            self.config.max_object_size.min(16 * 1024 * 1024),
        )?;
        match status {
            200 => Ok(Fetch::Hit(body)),
            404 => Ok(Fetch::Miss),
            _ => Err(status_error(status)),
        }
    }

    pub fn put_object(&self, digest: &str, bytes: &[u8]) -> Result<PutResult, RemoteError> {
        let result = self.put(&format!("/objects/{}", safe_segment(digest)), bytes)?;
        if result.status == 409 {
            match self.get_object(digest)? {
                Fetch::Hit(remote)
                    if remote == bytes && blake3::hash(&remote).to_hex().to_string() == digest =>
                {
                    return Ok(result);
                }
                _ => {
                    return Err(RemoteError {
                        kind: RemoteErrorKind::Conflict,
                        status: Some(409),
                        message: "remote object conflict failed digest verification".into(),
                    });
                }
            }
        }
        Ok(result)
    }

    pub fn put_manifest(&self, key: &str, bytes: &[u8]) -> Result<PutResult, RemoteError> {
        let result = self.put(&format!("/manifests/{}", safe_segment(key)), bytes)?;
        if result.status == 409 {
            match self.get_manifest(key)? {
                Fetch::Hit(remote) if remote == bytes => return Ok(result),
                _ => {
                    return Err(RemoteError {
                        kind: RemoteErrorKind::Conflict,
                        status: Some(409),
                        message: "remote manifest conflict failed verification".into(),
                    });
                }
            }
        }
        Ok(result)
    }

    fn get(&self, path: &str) -> Result<Fetch, RemoteError> {
        let (status, body) = self.request("GET", path, None, self.config.max_object_size)?;
        match status {
            200 => Ok(Fetch::Hit(body)),
            404 => Ok(Fetch::Miss),
            _ => Err(status_error(status)),
        }
    }

    fn put(&self, path: &str, body: &[u8]) -> Result<PutResult, RemoteError> {
        if body.len() as u64 > self.config.max_object_size {
            return Err(RemoteError {
                kind: RemoteErrorKind::PayloadTooLarge,
                status: None,
                message: "request body exceeds configured remote limit".into(),
            });
        }
        let (status, _) = self.request("PUT", path, Some(body), 64 * 1024)?;
        match status {
            200..=299 | 409 => Ok(PutResult {
                status,
                bytes: body.len() as u64,
            }),
            _ => Err(status_error(status)),
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        limit: u64,
    ) -> Result<(u16, Vec<u8>), RemoteError> {
        let url = format!(
            "{}/v1/{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            safe_segment(&self.config.namespace),
            path.trim_start_matches('/')
        );
        let response = match method {
            "GET" => self
                .agent
                .get(&url)
                .header("authorization", format!("Bearer {}", self.config.token))
                .call(),
            "PUT" => self
                .agent
                .put(&url)
                .content_type("application/octet-stream")
                .header("authorization", format!("Bearer {}", self.config.token))
                .send(body.unwrap_or_default()),
            _ => {
                return Err(RemoteError {
                    kind: RemoteErrorKind::Unsupported,
                    status: None,
                    message: "unsupported remote method".into(),
                });
            }
        }
        .map_err(|error| RemoteError {
            kind: RemoteErrorKind::Transport,
            status: None,
            message: error.to_string(),
        })?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| RemoteError {
                kind: RemoteErrorKind::PayloadTooLarge,
                status: Some(status),
                message: error.to_string(),
            })?;
        Ok((status, bytes))
    }
}

fn status_error(status: u16) -> RemoteError {
    let kind = match status {
        401 | 403 => RemoteErrorKind::Authentication,
        404 => RemoteErrorKind::NotFound,
        409 => RemoteErrorKind::Conflict,
        429 => RemoteErrorKind::RateLimited,
        400 | 422 => RemoteErrorKind::InvalidResponse,
        501 => RemoteErrorKind::Unsupported,
        _ => RemoteErrorKind::Other,
    };
    RemoteError {
        kind,
        status: Some(status),
        message: "remote request failed".into(),
    }
}

fn validate_endpoint(endpoint: &str, allow_insecure_loopback: bool) -> Result<()> {
    let Some((scheme, authority)) = endpoint.split_once("://") else {
        bail!("remote endpoint must include https://")
    };
    if authority.contains('@') || endpoint.contains('?') || endpoint.contains('#') {
        bail!("remote endpoint must not contain credentials or query fragments")
    }
    if scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if !scheme.eq_ignore_ascii_case("http") || !allow_insecure_loopback {
        bail!("remote endpoint must use HTTPS")
    }
    let authority = authority.split('/').next().unwrap_or_default();
    let host = if let Some(host) = authority.strip_prefix('[') {
        host.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        Ok(())
    } else {
        bail!("insecure remote endpoints are restricted to loopback")
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn safe_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn rejects_non_loopback_http() {
        let config = Config {
            endpoint: "http://example.test".into(),
            namespace: "test".into(),
            token: "secret".into(),
            timeout: Duration::from_secs(1),
            max_object_size: 1024,
            allow_insecure_loopback: true,
        };
        assert!(Client::new(config).is_err());
    }

    #[test]
    fn accepts_explicit_loopback_test_endpoint() {
        let config = Config {
            endpoint: "http://127.0.0.1:1234".into(),
            namespace: "test".into(),
            token: "secret".into(),
            timeout: Duration::from_secs(1),
            max_object_size: 1024,
            allow_insecure_loopback: true,
        };
        assert!(Client::new(config).is_ok());
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(safe_segment("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn authenticated_get_and_put_are_bounded_and_provider_neutral() {
        let mut index = 0usize;
        let (endpoint, server) = serve_n(3, move |_| {
            let current = index;
            index += 1;
            if current == 1 {
                (409, Vec::new())
            } else {
                (200, b"object".to_vec())
            }
        });
        let client = Client::new(Config {
            endpoint,
            namespace: "test".into(),
            token: "test-token".into(),
            timeout: Duration::from_secs(2),
            max_object_size: 1024,
            allow_insecure_loopback: true,
        })
        .unwrap();
        let digest = blake3::hash(b"object").to_hex().to_string();
        assert_eq!(
            client.get_object(&digest).unwrap(),
            Fetch::Hit(b"object".to_vec())
        );
        assert_eq!(client.put_object(&digest, b"object").unwrap().status, 409);
        let recorded = server.join().unwrap();
        assert!(
            recorded
                .iter()
                .all(|r| r.headers.contains("authorization: Bearer test-token"))
        );
    }

    #[test]
    fn maps_not_found_to_a_miss() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let client = Client::new(Config {
            endpoint: format!("http://{address}"),
            namespace: "test".into(),
            token: "token".into(),
            timeout: Duration::from_secs(2),
            max_object_size: 1024,
            allow_insecure_loopback: true,
        })
        .unwrap();
        assert_eq!(client.get_manifest("key").unwrap(), Fetch::Miss);
        server.join().unwrap();
    }

    // ---- Shared fixtures for fault and interop coverage ----------------------

    struct Recorded {
        method: String,
        path: String,
        headers: String,
        body: Vec<u8>,
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Recorded {
        let mut buf = Vec::new();
        let mut tmp = [0_u8; 8192];
        let headers_end = loop {
            let count = stream.read(&mut tmp).unwrap();
            if count == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&tmp[..count]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..headers_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = buf[headers_end..].to_vec();
        while body.len() < content_length {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(count) => body.extend_from_slice(&tmp[..count]),
            }
        }
        body.truncate(content_length);
        let mut request_line = headers
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace();
        Recorded {
            method: request_line.next().unwrap_or_default().into(),
            path: request_line.next().unwrap_or_default().into(),
            headers,
            body,
        }
    }

    fn write_response(stream: &mut std::net::TcpStream, status: u16, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 {status} R\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    }

    /// Serve `count` canned requests; returns the endpoint and a handle to the
    /// recorded requests for assertions.
    fn serve_n(
        count: usize,
        mut handler: impl FnMut(&Recorded) -> (u16, Vec<u8>) + Send + 'static,
    ) -> (String, thread::JoinHandle<Vec<Recorded>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut recorded = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                let (status, body) = handler(&request);
                write_response(&mut stream, status, &body);
                recorded.push(request);
            }
            recorded
        });
        (format!("http://{address}"), handle)
    }

    fn test_config(endpoint: &str) -> Config {
        Config {
            endpoint: endpoint.into(),
            namespace: "test".into(),
            token: "test-token".into(),
            timeout: Duration::from_secs(2),
            max_object_size: 1024,
            allow_insecure_loopback: true,
        }
    }

    /// A second, independent implementation of the remote-CAS protocol: a
    /// stateful in-memory CAS that returns 201 for new objects, 409 for
    /// duplicates, and real GETs — exercising the client against semantics
    /// rather than canned responses.
    fn serve_cas(expected_requests: usize) -> (String, thread::JoinHandle<Vec<Recorded>>) {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut recorded = Vec::new();
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                let mut store = store.lock().unwrap();
                let (status, body) = match request.method.as_str() {
                    "GET" => match store.get(&request.path) {
                        Some(bytes) => (200, bytes.clone()),
                        None => (404, Vec::new()),
                    },
                    "PUT" => {
                        if store.contains_key(&request.path) {
                            (409, Vec::new())
                        } else {
                            store.insert(request.path.clone(), request.body.clone());
                            (201, Vec::new())
                        }
                    }
                    _ => (405, Vec::new()),
                };
                drop(store);
                write_response(&mut stream, status, &body);
                recorded.push(request);
            }
            recorded
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn oversized_and_truncated_responses_fail_closed() {
        // Body beyond max_object_size: bounded reader must error, never return a Hit.
        let (endpoint, server) = serve_n(1, |_| (200, vec![b'x'; 2048]));
        let client = Client::new(test_config(&endpoint)).unwrap();
        let digest = blake3::hash(b"whatever").to_hex().to_string();
        assert!(client.get_object(&digest).is_err());
        server.join().unwrap();

        // Content-Length lies (claims more than sent): must error, not return a
        // partial Hit.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n")
                .unwrap();
            stream.write_all(b"short").unwrap();
        });
        let client = Client::new(test_config(&format!("http://{address}"))).unwrap();
        let digest = blake3::hash(b"short").to_hex().to_string();
        assert!(client.get_object(&digest).is_err());
        server.join().unwrap();
    }

    #[test]
    fn digest_mismatch_auth_and_rate_limit_fail_closed() {
        // Server returns a 200 with bytes that don't match the requested digest.
        let (endpoint, server) = serve_n(1, |_| (200, b"forged".to_vec()));
        let client = Client::new(test_config(&endpoint)).unwrap();
        let digest = blake3::hash(b"genuine").to_hex().to_string();
        let error = client.get_object(&digest).unwrap_err();
        assert_eq!(error.kind, RemoteErrorKind::InvalidResponse);
        server.join().unwrap();

        let (endpoint, server) = serve_n(2, |_| (401, Vec::new()));
        let client = Client::new(test_config(&endpoint)).unwrap();
        assert_eq!(
            client.get_manifest("k").unwrap_err().kind,
            RemoteErrorKind::Authentication
        );
        assert_eq!(
            client.put_manifest("k", b"m").unwrap_err().kind,
            RemoteErrorKind::Authentication
        );
        server.join().unwrap();

        let (endpoint, server) = serve_n(1, |_| (429, Vec::new()));
        let client = Client::new(test_config(&endpoint)).unwrap();
        assert_eq!(
            client.get_manifest("k").unwrap_err().kind,
            RemoteErrorKind::RateLimited
        );
        server.join().unwrap();
    }

    #[test]
    fn offline_timeout_and_tls_failures_are_transport_errors() {
        // Nothing listening: connection refused is a bounded Transport error.
        let client = Client::new(test_config("http://127.0.0.1:9")).unwrap();
        assert_eq!(
            client.get_manifest("k").unwrap_err().kind,
            RemoteErrorKind::Transport
        );

        // A server that accepts but never responds must hit the global timeout.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stalled = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(10));
            drop(stream);
        });
        let client = Client::new(test_config(&format!("http://{address}"))).unwrap();
        let started = std::time::Instant::now();
        let error = client.get_manifest("k").unwrap_err();
        assert_eq!(error.kind, RemoteErrorKind::Transport);
        assert!(started.elapsed() < Duration::from_secs(8));
        drop(stalled);

        // HTTPS against a plaintext endpoint must fail at the transport layer —
        // plaintext can never masquerade as a remote CAS.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 512];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let client = Client::new(Config {
            endpoint: format!("https://{address}"),
            ..test_config("")
        })
        .unwrap();
        assert_eq!(
            client.get_manifest("k").unwrap_err().kind,
            RemoteErrorKind::Transport
        );
        server.join().unwrap();
    }

    #[test]
    fn second_implementation_interop_and_namespace_isolation() {
        // Full object+manifest round trip against the independent CAS fixture:
        // PUT -> 201, duplicate PUT -> 409 verified by GET, GET hits verified.
        // Requests: put, put+get(409-verify), get, put, put+get(409-verify), get = 8.
        let (endpoint, server) = serve_cas(8);
        let client = Client::new(test_config(&endpoint)).unwrap();
        let bytes = b"interoperable bytes".to_vec();
        let digest = blake3::hash(&bytes).to_hex().to_string();
        assert_eq!(client.put_object(&digest, &bytes).unwrap().status, 201);
        assert_eq!(client.put_object(&digest, &bytes).unwrap().status, 409);
        assert_eq!(
            client.get_object(&digest).unwrap(),
            Fetch::Hit(bytes.clone())
        );
        assert_eq!(client.put_manifest("key-1", b"{}").unwrap().status, 201);
        assert_eq!(client.put_manifest("key-1", b"{}").unwrap().status, 409);
        assert_eq!(
            client.get_manifest("key-1").unwrap(),
            Fetch::Hit(b"{}".to_vec())
        );
        let recorded = server.join().unwrap();
        assert!(recorded.iter().all(|r| r.path.starts_with("/v1/test/")));
        assert!(
            recorded
                .iter()
                .all(|r| r.headers.contains("authorization: Bearer test-token"))
        );

        // Two namespaces over the same endpoint never share paths.
        let (endpoint, server) = serve_cas(2);
        let mut ns_a = test_config(&endpoint);
        ns_a.namespace = "ns-a".into();
        let mut ns_b = test_config(&endpoint);
        ns_b.namespace = "ns-b".into();
        let client_a = Client::new(ns_a).unwrap();
        let client_b = Client::new(ns_b).unwrap();
        client_a.put_manifest("k", b"a").unwrap();
        client_b.put_manifest("k", b"b").unwrap();
        let recorded = server.join().unwrap();
        assert_eq!(recorded[0].path, "/v1/ns-a/manifests/k");
        assert_eq!(recorded[1].path, "/v1/ns-b/manifests/k");
    }

    #[test]
    fn token_never_leaks_into_urls_errors_or_request_lines() {
        let (endpoint, server) = serve_n(1, |_| (500, Vec::new()));
        let client = Client::new(Config {
            token: "super-secret-token".into(),
            ..test_config(&endpoint)
        })
        .unwrap();
        let error = client.get_manifest("k").unwrap_err();
        let shown = format!("{error}");
        assert!(!shown.contains("super-secret-token"), "{shown}");
        let recorded = server.join().unwrap();
        assert!(!recorded[0].path.contains("super-secret-token"));
        assert!(!recorded[0].path.contains("super%2Dsecret"));
        assert!(
            recorded[0]
                .headers
                .contains("authorization: Bearer super-secret-token")
        );
    }
}
