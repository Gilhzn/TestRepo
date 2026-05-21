//! Remote and tiered content-addressable storage backends.
//!
//! Provides HTTP-based CAS for cloud storage (S3, GCS, MinIO, etc.) via presigned URLs,
//! and a tiered composer for write-through with read fallback.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::storage::Cas;
use std::io::Write;

const ZSTD_LEVEL: i32 = 3;

/// HTTP-based content store for any server supporting PUT/GET/HEAD at `<base>/<prefix>/<rest>`.
/// Stores zstd-compressed bytes; verifies integrity on read via re-hashing.
pub struct HttpCas {
    base: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
}

impl HttpCas {
    /// Create a new HTTP CAS without authentication.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;
        Ok(Self {
            base: base_url.into(),
            token: None,
            client,
        })
    }

    /// Create a new HTTP CAS with Bearer token authentication.
    pub fn with_token(base_url: impl Into<String>, bearer: impl Into<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;
        Ok(Self {
            base: base_url.into(),
            token: Some(bearer.into()),
            client,
        })
    }

    fn url_for(&self, hash: &Hash) -> String {
        let hex = hash.to_hex();
        format!("{}/{}/{}", self.base, &hex[..2], &hex[2..])
    }

    fn authed(
        &self,
        req: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        if let Some(token) = &self.token {
            req.header("Authorization", format!("Bearer {}", token))
        } else {
            req
        }
    }
}

impl Cas for HttpCas {
    fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);

        if self.has(&hash)? {
            return Ok(hash);
        }

        let url = self.url_for(&hash);

        let mut compressed = Vec::new();
        {
            let mut enc = zstd::Encoder::new(&mut compressed, ZSTD_LEVEL)
                .map_err(|e| Error::RemoteStorage(e.to_string()))?;
            enc.write_all(bytes)
                .map_err(|e| Error::RemoteStorage(e.to_string()))?;
            enc.finish()
                .map_err(|e| Error::RemoteStorage(e.to_string()))?;
        }

        let req = self.client.put(&url).body(compressed);
        let req = self.authed(req);
        let resp = req
            .send()
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(Error::RemoteStorage(format!(
                "PUT failed with status {}",
                resp.status()
            )));
        }

        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        let url = self.url_for(hash);

        let req = self.client.get(&url);
        let req = self.authed(req);
        let resp = req
            .send()
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;

        if resp.status().as_u16() == 404 {
            return Err(Error::RemoteStorage(format!("blob not found: {}", hash)));
        }

        if !resp.status().is_success() {
            return Err(Error::RemoteStorage(format!(
                "GET failed with status {}",
                resp.status()
            )));
        }

        let compressed = resp
            .bytes()
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;

        let mut decompressed = Vec::new();
        let mut dec = zstd::Decoder::new(compressed.as_ref())
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;
        std::io::Read::read_to_end(&mut dec, &mut decompressed)
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;

        let actual = Hash::of(&decompressed);
        if actual != *hash {
            return Err(Error::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }

        Ok(decompressed)
    }

    fn has(&self, hash: &Hash) -> Result<bool> {
        let url = self.url_for(hash);

        let req = self.client.head(&url);
        let req = self.authed(req);
        let resp = req
            .send()
            .map_err(|e| Error::RemoteStorage(e.to_string()))?;

        match resp.status().as_u16() {
            200..=299 => Ok(true),
            404 => Ok(false),
            s => Err(Error::RemoteStorage(format!("HEAD failed with status {}", s))),
        }
    }
}

/// Tiered composer: write-through to both L and R; read from L first, then R (cache miss).
pub struct TieredCas<L, R> {
    pub local: L,
    pub remote: R,
}

impl<L, R> TieredCas<L, R> {
    pub fn new(local: L, remote: R) -> Self {
        Self { local, remote }
    }
}

impl<L: Cas, R: Cas> Cas for TieredCas<L, R> {
    fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let h = self.local.put(bytes)?;
        // Best-effort remote write; if remote fails we still return Ok so disconnected
        // operation continues, but log the failure.
        if let Err(e) = self.remote.put(bytes) {
            eprintln!("warning: tiered remote write failed (local succeeded): {}", e);
        }
        Ok(h)
    }

    fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        match self.local.get(hash) {
            Ok(v) => Ok(v),
            Err(_) => {
                let bytes = self.remote.get(hash)?;
                // Populate local cache best-effort.
                if let Err(e) = self.local.put(&bytes) {
                    eprintln!(
                        "warning: tiered cache population failed (remote succeeded): {}",
                        e
                    );
                }
                Ok(bytes)
            }
        }
    }

    fn has(&self, hash: &Hash) -> Result<bool> {
        if self.local.has(hash)? {
            return Ok(true);
        }
        self.remote.has(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Minimal in-process test HTTP server.
    struct TestServer {
        storage: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        listener: std::net::TcpListener,
        require_token: bool,
        expected_token: String,
    }

    impl TestServer {
        fn new() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("failed to bind test server");
            Self {
                storage: Arc::new(Mutex::new(HashMap::new())),
                listener,
                require_token: false,
                expected_token: "secret-token".to_string(),
            }
        }

        fn with_token(token: &str) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("failed to bind test server");
            Self {
                storage: Arc::new(Mutex::new(HashMap::new())),
                listener,
                require_token: true,
                expected_token: token.to_string(),
            }
        }

        fn addr(&self) -> String {
            self.listener.local_addr().unwrap().to_string()
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr())
        }

        fn start(self) -> TestServerHandle {
            let storage = Arc::clone(&self.storage);
            let listener = self.listener.try_clone().unwrap();
            let require_token = self.require_token;
            let expected_token = self.expected_token.clone();
            let base_url = self.base_url();

            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if let Ok(stream) = stream {
                        let storage = Arc::clone(&storage);
                        let expected_token = expected_token.clone();
                        std::thread::spawn(move || {
                            handle_request(stream, &storage, require_token, &expected_token)
                        });
                    }
                }
            });

            TestServerHandle {
                storage: Arc::clone(&self.storage),
                base_url,
                _thread: handle,
            }
        }
    }

    struct TestServerHandle {
        storage: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        base_url: String,
        _thread: std::thread::JoinHandle<()>,
    }

    fn handle_request(
        mut stream: std::net::TcpStream,
        storage: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
        require_token: bool,
        expected_token: &str,
    ) {
        use std::io::{Read, Write};

        let mut buf = vec![0u8; 4096];
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => return,
        };

        let request = String::from_utf8_lossy(&buf[..n]);
        let lines: Vec<&str> = request.lines().collect();
        if lines.is_empty() {
            return;
        }

        let first_line = lines[0];
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }

        let method = parts[0];
        let path = parts[1];

        // Check authorization if required. Check for Bearer token in Authorization header.
        let mut has_valid_token = !require_token;
        if require_token {
            for line in &lines[1..] {
                if line.to_lowercase().starts_with("authorization:") {
                    if let Some(rest) = line.split(':').nth(1) {
                        let rest = rest.trim();
                        if rest.starts_with("Bearer ") {
                            let token = rest.strip_prefix("Bearer ").unwrap().trim();
                            if token == expected_token {
                                has_valid_token = true;
                            }
                        }
                    }
                    break;
                }
            }
        }

        if !has_valid_token {
            let response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            return;
        }

        match method {
            "PUT" => {
                if let Some(body_start) = request.find("\r\n\r\n") {
                    let body = &buf[body_start + 4..n];
                    let mut storage = storage.lock().unwrap();
                    storage.insert(path.to_string(), body.to_vec());
                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                } else {
                    let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                }
            }
            "GET" => {
                let storage = storage.lock().unwrap();
                if let Some(data) = storage.get(path) {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        data.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(data);
                } else {
                    let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                }
            }
            "HEAD" => {
                let storage = storage.lock().unwrap();
                if storage.contains_key(path) {
                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                } else {
                    let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                }
            }
            _ => {
                let response = "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        }
    }

    #[test]
    fn http_cas_put_then_get_round_trip() {
        let server = TestServer::new().start();
        let cas = HttpCas::new(&server.base_url).unwrap();

        let data = b"test payload for http cas";
        let h = cas.put(data).unwrap();
        assert_eq!(cas.get(&h).unwrap(), data);
    }

    #[test]
    fn http_cas_has_reports_true_after_put() {
        let server = TestServer::new().start();
        let cas = HttpCas::new(&server.base_url).unwrap();

        let data = b"payload for has test";
        let h = cas.put(data).unwrap();
        assert!(cas.has(&h).unwrap());
    }

    #[test]
    fn http_cas_has_reports_false_for_unknown() {
        let server = TestServer::new().start();
        let cas = HttpCas::new(&server.base_url).unwrap();

        let phantom = Hash::of(b"never uploaded");
        assert!(!cas.has(&phantom).unwrap());
    }

    #[test]
    fn http_cas_detects_corruption() {
        let server = TestServer::new().start();
        let cas = HttpCas::new(&server.base_url).unwrap();

        let data = b"original data";
        let h = cas.put(data).unwrap();

        // Manually tamper with storage
        {
            let mut storage = server.storage.lock().unwrap();
            let hex = h.to_hex();
            let key = format!("/{}/{}", &hex[..2], &hex[2..]);
            // Store corrupted compressed data
            let mut corrupted = Vec::new();
            {
                let mut enc = zstd::Encoder::new(&mut corrupted, ZSTD_LEVEL).unwrap();
                enc.write_all(b"tampered bytes!").unwrap();
                enc.finish().unwrap();
            }
            storage.insert(key, corrupted);
        }

        match cas.get(&h) {
            Err(Error::HashMismatch { .. }) => {}
            other => panic!("expected HashMismatch, got {:?}", other),
        }
    }

    #[test]
    fn http_cas_with_bearer_token() {
        let server = TestServer::with_token("my-secret-token").start();
        let cas = HttpCas::with_token(&server.base_url, "my-secret-token").unwrap();

        let data = b"token-protected data";
        let h = cas.put(data).unwrap();
        assert_eq!(cas.get(&h).unwrap(), data);
    }

    #[test]
    fn http_cas_bearer_token_wrong_fails() {
        let server = TestServer::with_token("correct-token").start();
        let cas = HttpCas::with_token(&server.base_url, "wrong-token").unwrap();

        let data = b"protected data";
        let result = cas.put(data);
        assert!(result.is_err());
    }

    #[test]
    fn tiered_cas_writes_to_both_layers() {
        let dir1 = tempfile::TempDir::new().unwrap();
        let dir2 = tempfile::TempDir::new().unwrap();
        let local = crate::storage::FsCas::open(dir1.path()).unwrap();
        let remote = crate::storage::FsCas::open(dir2.path()).unwrap();
        let tiered = TieredCas::new(local, remote);

        let data = b"data written to both layers";
        let h = tiered.put(data).unwrap();

        // Both should have the blob
        assert!(tiered.local.has(&h).unwrap());
        assert!(tiered.remote.has(&h).unwrap());
        assert_eq!(tiered.local.get(&h).unwrap(), data);
        assert_eq!(tiered.remote.get(&h).unwrap(), data);
    }

    #[test]
    fn tiered_cas_read_falls_back_to_remote_on_local_miss() {
        let dir1 = tempfile::TempDir::new().unwrap();
        let dir2 = tempfile::TempDir::new().unwrap();
        let local = crate::storage::FsCas::open(dir1.path()).unwrap();
        let remote = crate::storage::FsCas::open(dir2.path()).unwrap();

        let data = b"remote-only data initially";
        let h = remote.put(data).unwrap();

        let tiered = TieredCas::new(local, remote);

        // First read from tiered (local miss, remote hit, cache populates local)
        assert_eq!(tiered.get(&h).unwrap(), data);

        // Second read should find it in local
        assert!(tiered.local.has(&h).unwrap());
    }

    #[test]
    fn tiered_cas_has_checks_local_first() {
        let dir1 = tempfile::TempDir::new().unwrap();
        let dir2 = tempfile::TempDir::new().unwrap();
        let local = crate::storage::FsCas::open(dir1.path()).unwrap();
        let remote = crate::storage::FsCas::open(dir2.path()).unwrap();

        let data = b"local-only data";
        let h = local.put(data).unwrap();

        let tiered = TieredCas::new(local, remote);

        // has() should return true via local without querying remote
        assert!(tiered.has(&h).unwrap());
    }
}
