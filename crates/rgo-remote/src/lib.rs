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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8_lossy(&request);
                assert!(request_text.contains("authorization: Bearer test-token"));
                let (status, body) = if index == 0 || index == 2 {
                    ("200 OK", b"object".as_slice())
                } else {
                    ("409 Conflict", b"".as_slice())
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let client = Client::new(Config {
            endpoint: format!("http://{address}"),
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
        server.join().unwrap();
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
}
