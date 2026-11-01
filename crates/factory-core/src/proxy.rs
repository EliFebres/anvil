//! The egress proxy: an HTTP `CONNECT` proxy on a unix socket that lets the
//! container reach the allowlisted hosts and nothing else.
//!
//! The container runs with `--network none`; `socat` inside it bridges
//! `localhost:3128` to the socket, and `HTTPS_PROXY` points there. Every
//! refused host is remembered, so a missing allowlist entry shows up in the
//! ticket's notes instead of as a mystery build failure.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Hosts the proxy connects to.
///
/// An entry is `host` (port 443 only) or `host:port`. Matching is exact and
/// case-insensitive on the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: BTreeSet<String>,
}

impl Allowlist {
    /// Builds the list from hosts and `host:port` entries.
    ///
    /// # Arguments
    ///
    /// * `entries` - The entries; empty ones are dropped.
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>>(entries: I) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|e| e.as_ref().trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
        }
    }

    /// Whether a `CONNECT host:port` may go through.
    ///
    /// # Arguments
    ///
    /// * `host` - The host name or address.
    /// * `port` - The port.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = host.to_ascii_lowercase();
        self.entries.contains(&format!("{host}:{port}"))
            || (port == 443 && self.entries.contains(&host))
    }

    /// The entries, sorted.
    pub fn entries(&self) -> Vec<String> {
        self.entries.iter().cloned().collect()
    }
}

/// The hosts refused so far, shared with whoever writes the notes.
pub type Denied = Arc<Mutex<BTreeSet<String>>>;

/// Parses the first line of a `CONNECT` request.
///
/// # Arguments
///
/// * `request` - The request head, up to the blank line.
///
/// # Returns
///
/// `(host, port)`, or `None` for anything but `CONNECT host:port HTTP/1.x`.
pub fn parse_connect(request: &str) -> Option<(String, u16)> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "CONNECT" {
        return None;
    }
    let target = parts.next()?;
    let (host, port) = target.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.trim_matches(['[', ']']).to_owned(), port))
}

/// Records a refusal.
pub fn record_denied(denied: &Denied, host: &str, port: u16) {
    denied
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(format!("{host}:{port}"));
}

#[cfg(unix)]
pub use unix::{ProxyHandle, start};

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UnixListener, UnixStream};
    use tokio::task::JoinHandle;

    use super::{Allowlist, Denied, parse_connect, record_denied};
    use crate::error::{FactoryError, Result};

    /// Largest request head accepted.
    const MAX_HEAD: usize = 8 * 1024;

    /// A running proxy; dropping it stops the listener and removes the socket.
    #[derive(Debug)]
    pub struct ProxyHandle {
        task: JoinHandle<()>,
        path: PathBuf,
    }

    impl Drop for ProxyHandle {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Starts the proxy on a unix socket.
    ///
    /// # Arguments
    ///
    /// * `path` - The socket path; a stale file there is removed.
    /// * `allowlist` - What may be reached.
    /// * `denied` - Where refusals are recorded.
    ///
    /// # Returns
    ///
    /// The handle.
    ///
    /// # Errors
    ///
    /// When the socket cannot be bound.
    pub fn start(path: &Path, allowlist: Allowlist, denied: Denied) -> Result<ProxyHandle> {
        let _ = std::fs::remove_file(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|source| FactoryError::io("creating", dir, source))?;
        }
        let listener = UnixListener::bind(path).map_err(|error| {
            FactoryError::Sandbox(format!("binding {}: {error}", path.display()))
        })?;
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let allowlist = allowlist.clone();
                        let denied = denied.clone();
                        tokio::spawn(async move {
                            if let Err(error) = serve(stream, &allowlist, &denied).await {
                                tracing::debug!(%error, "proxy connection ended");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "proxy accept failed");
                        break;
                    }
                }
            }
        });
        Ok(ProxyHandle {
            task,
            path: path.to_path_buf(),
        })
    }

    /// Handles one client: reads the `CONNECT`, tunnels or refuses.
    async fn serve(
        mut client: UnixStream,
        allowlist: &Allowlist,
        denied: &Denied,
    ) -> std::io::Result<()> {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && head.len() < MAX_HEAD {
            if client.read(&mut byte).await? == 0 {
                return Ok(());
            }
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head);
        let Some((host, port)) = parse_connect(&text) else {
            client
                .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n")
                .await?;
            return Ok(());
        };
        if !allowlist.allows(&host, port) {
            tracing::warn!(host, port, "egress refused by the allowlist");
            record_denied(denied, &host, port);
            client
                .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
                .await?;
            return Ok(());
        }
        let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(host, port, %error, "egress connect failed");
                client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                    .await?;
                return Ok(());
            }
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        Ok(())
    }
}

#[cfg(not(unix))]
pub use other::{ProxyHandle, start};

#[cfg(not(unix))]
mod other {
    use std::path::Path;

    use super::{Allowlist, Denied};
    use crate::error::{FactoryError, Result};

    /// Never constructed off unix.
    #[derive(Debug)]
    pub struct ProxyHandle;

    /// Refuses: containers and their proxy run on Linux only.
    ///
    /// # Errors
    ///
    /// Always [`FactoryError::Sandbox`].
    pub fn start(_path: &Path, _allowlist: Allowlist, _denied: Denied) -> Result<ProxyHandle> {
        Err(FactoryError::Sandbox(
            "the egress proxy needs a unix socket; containers run on Linux only (set [worker] runtime = \"none\" here)".to_owned(),
        ))
    }
}

/// Whether `path` is where a proxy socket would live; a helper for the notes.
pub fn describe(path: &Path) -> String {
    format!("proxy socket {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_hosts_on_443_and_host_port_entries() {
        let list = Allowlist::new(["github.com", "Index.crates.io", "127.0.0.1:9000", ""]);
        assert!(list.allows("github.com", 443));
        assert!(list.allows("GITHUB.COM", 443));
        assert!(!list.allows("github.com", 80));
        assert!(!list.allows("api.github.com", 443));
        assert!(list.allows("index.crates.io", 443));
        assert!(list.allows("127.0.0.1", 9000));
        assert!(!list.allows("127.0.0.1", 9001));
        assert_eq!(list.entries().len(), 3);
    }

    #[test]
    fn connect_lines_parse_and_others_do_not() {
        assert_eq!(
            parse_connect("CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("api.anthropic.com".to_owned(), 443))
        );
        assert_eq!(
            parse_connect("CONNECT [::1]:8443 HTTP/1.1\r\n\r\n"),
            Some(("::1".to_owned(), 8443))
        );
        assert_eq!(parse_connect("GET http://x/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_connect("CONNECT nohost HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_connect(""), None);
    }

    #[test]
    fn refusals_are_recorded_once_per_host_and_port() {
        let denied: Denied = Default::default();
        record_denied(&denied, "evil.example", 443);
        record_denied(&denied, "evil.example", 443);
        record_denied(&denied, "evil.example", 80);
        let set = denied.lock().expect("lock");
        assert_eq!(set.len(), 2);
        assert!(set.contains("evil.example:443"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tunnels_allowed_hosts_and_refuses_the_rest() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, UnixStream};

        let echo = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = echo.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    if let Ok(n) = stream.read(&mut buf).await {
                        let _ = stream.write_all(&buf[..n]).await;
                    }
                });
            }
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("p.sock");
        let denied: Denied = Default::default();
        let _proxy = start(
            &socket,
            Allowlist::new([format!("127.0.0.1:{port}")]),
            denied.clone(),
        )
        .expect("starts");

        let mut client = UnixStream::connect(&socket).await.expect("connect");
        client
            .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut reply = [0u8; 39];
        client.read_exact(&mut reply).await.expect("read");
        assert!(
            reply.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&reply)
        );
        client.write_all(b"ping").await.expect("write");
        let mut back = [0u8; 4];
        client.read_exact(&mut back).await.expect("echo");
        assert_eq!(&back, b"ping");

        let mut refused = UnixStream::connect(&socket).await.expect("connect");
        refused
            .write_all(b"CONNECT evil.example:443 HTTP/1.1\r\n\r\n")
            .await
            .expect("write");
        let mut reply = Vec::new();
        refused.read_to_end(&mut reply).await.expect("read");
        assert!(
            reply.starts_with(b"HTTP/1.1 403"),
            "{}",
            String::from_utf8_lossy(&reply)
        );
        assert!(denied.lock().expect("lock").contains("evil.example:443"));
    }
}
