//! IPC server: newline-delimited JSON over a Unix socket.
//!
//! Access control is the filesystem plus `SO_PEERCRED`, not an application-level
//! handshake. The operation set is closed and typed — see `docs/IPC-PROTOCOL.md`.
//! There is deliberately no operation that runs a command or reads an arbitrary path.

use crate::eventloop::{set_nonblocking, TOK_CLIENT_BASE};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Refuse a line longer than this rather than buffering unboundedly.
const MAX_LINE: usize = 64 * 1024;
/// A UI plus a couple of CLI queries is plenty; more suggests something is wrong.
const MAX_CLIENTS: usize = 8;

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: u64,
    pub op: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl Response {
    pub fn ok(id: u64, data: serde_json::Value) -> Response {
        Response { id: Some(id), ok: true, data: Some(data), error: None }
    }
    pub fn err(id: u64, code: &str, msg: impl Into<String>) -> Response {
        Response {
            id: Some(id),
            ok: false,
            data: None,
            error: Some(ErrorBody { code: code.into(), message: msg.into() }),
        }
    }
}

pub struct Client {
    stream: UnixStream,
    pub token: u64,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    pub subscriptions: Vec<String>,
}

impl Client {
    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Read available bytes and return whatever complete requests arrived.
    /// `Err` means the connection should be dropped.
    pub fn read_requests(&mut self) -> Result<Vec<Request>, String> {
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("peer closed".into()),
                Ok(n) => {
                    self.inbuf.extend_from_slice(&chunk[..n]);
                    if self.inbuf.len() > MAX_LINE {
                        return Err("request exceeded 64 KiB".into());
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        let mut out = Vec::new();
        while let Some(pos) = self.inbuf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.inbuf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<Request>(text) {
                Ok(r) => out.push(r),
                Err(e) => {
                    // A malformed line is reported on that connection only; it never
                    // affects other clients or the daemon.
                    let _ = self.send(&Response::err(0, "bad_request", format!("invalid JSON: {e}")));
                }
            }
        }
        Ok(out)
    }

    pub fn send<T: Serialize>(&mut self, v: &T) -> std::io::Result<()> {
        let mut s = serde_json::to_vec(v)?;
        s.push(b'\n');
        self.outbuf.extend_from_slice(&s);
        self.flush()
    }

    /// Non-blocking write. A UI that stops reading must not be able to block the
    /// daemon's event loop, so unsent bytes stay buffered and are retried.
    pub fn flush(&mut self) -> std::io::Result<()> {
        while !self.outbuf.is_empty() {
            match self.stream.write(&self.outbuf) {
                Ok(0) => break,
                Ok(n) => {
                    self.outbuf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        // Cap the backlog: a wedged client is disconnected rather than allowed to
        // grow the daemon's memory.
        if self.outbuf.len() > 4 << 20 {
            return Err(std::io::Error::new(ErrorKind::Other, "client output backlog exceeded 4 MiB"));
        }
        Ok(())
    }

    pub fn wants(&self, topic: &str) -> bool {
        self.subscriptions.iter().any(|t| t == topic)
    }
}

pub struct IpcServer {
    listener: UnixListener,
    path: PathBuf,
    clients: HashMap<u64, Client>,
    next_token: u64,
}

impl IpcServer {
    /// Bind the socket inside a `0700` directory owned by this user.
    pub fn bind(dir: &Path) -> std::io::Result<IpcServer> {
        std::fs::create_dir_all(dir)?;
        // Enforce 0700 even if the directory already existed with looser bits.
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(dir)?.permissions();
            p.set_mode(0o700);
            std::fs::set_permissions(dir, p)?;
        }
        let path = dir.join("sock");
        // sockaddr_un.sun_path is a fixed 108-byte field. An unusual XDG_RUNTIME_DIR
        // can exceed it, and the raw kernel error ("path must be shorter than SUN_LEN")
        // does not tell the user which path or what to do about it.
        const SUN_PATH_MAX: usize = 100;
        if path.as_os_str().len() > SUN_PATH_MAX {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "socket path is {} bytes, over the {SUN_PATH_MAX}-byte kernel limit: {}\n                     Set XDG_RUNTIME_DIR to a shorter directory (normally /run/user/$UID).",
                    path.as_os_str().len(),
                    path.display()
                ),
            ));
        }
        // A stale socket from a killed daemon would make bind() fail with EADDRINUSE.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&path)?.permissions();
            p.set_mode(0o600);
            std::fs::set_permissions(&path, p)?;
        }
        set_nonblocking(listener.as_raw_fd())?;
        Ok(IpcServer { listener, path, clients: HashMap::new(), next_token: TOK_CLIENT_BASE })
    }

    pub fn fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Accept pending connections. Returns the new clients' tokens for epoll registration.
    pub fn accept(&mut self) -> Vec<(u64, RawFd)> {
        let mut new = Vec::new();
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Defence in depth: the 0700 directory should already make this
                    // impossible, but check the peer's UID rather than trusting it.
                    match peer_uid(stream.as_raw_fd()) {
                        // SAFETY: getuid never fails.
                        Some(uid) if uid == unsafe { libc::getuid() } => {}
                        other => {
                            crate::log_warn!("rejecting IPC peer with uid {other:?}");
                            continue;
                        }
                    }
                    if self.clients.len() >= MAX_CLIENTS {
                        crate::log_warn!("IPC client limit reached, refusing connection");
                        continue;
                    }
                    if set_nonblocking(stream.as_raw_fd()).is_err() {
                        continue;
                    }
                    let token = self.next_token;
                    self.next_token += 1;
                    let fd = stream.as_raw_fd();
                    self.clients.insert(
                        token,
                        Client { stream, token, inbuf: Vec::new(), outbuf: Vec::new(), subscriptions: Vec::new() },
                    );
                    new.push((token, fd));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    crate::log_warn!("IPC accept failed: {e}");
                    break;
                }
            }
        }
        new
    }

    pub fn client(&mut self, token: u64) -> Option<&mut Client> {
        self.clients.get_mut(&token)
    }

    pub fn drop_client(&mut self, token: u64) -> Option<RawFd> {
        self.clients.remove(&token).map(|c| c.stream.as_raw_fd())
    }

    /// Push a frame to every client subscribed to `topic`. Returns tokens that failed
    /// and should be dropped.
    pub fn broadcast(&mut self, topic: &str, data: &serde_json::Value) -> Vec<u64> {
        let frame = serde_json::json!({"push": topic, "data": data});
        let mut dead = Vec::new();
        for (tok, c) in self.clients.iter_mut() {
            if !c.wants(topic) {
                continue;
            }
            if c.send(&frame).is_err() {
                dead.push(*tok);
            }
        }
        dead
    }

    pub fn tokens(&self) -> Vec<u64> {
        self.clients.keys().copied().collect()
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn peer_uid(fd: RawFd) -> Option<u32> {
    // SAFETY: ucred is POD; the size is passed and updated by the kernel.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jamsys-ipc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_socket_directory_and_socket_are_locked_down() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("perms");
        let s = IpcServer::bind(&d).unwrap();
        let dm = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        let sm = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dm, 0o700, "directory must be private to this user");
        assert_eq!(sm, 0o600, "socket must be private to this user");
        drop(s);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn an_over_long_socket_path_is_refused_with_a_useful_message() {
        let long = std::env::temp_dir().join("a".repeat(120));
        let msg = match IpcServer::bind(&long) {
            Ok(_) => panic!("an over-long socket path should be refused"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("kernel limit"), "unhelpful message: {msg}");
        assert!(msg.contains("XDG_RUNTIME_DIR"), "should say how to fix it: {msg}");
        let _ = std::fs::remove_dir_all(&long);
    }

    #[test]
    fn a_stale_socket_does_not_block_startup() {
        let d = tmpdir("stale");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("sock"), b"leftover").unwrap();
        // A daemon killed with SIGKILL leaves the file behind; bind must recover.
        let s = IpcServer::bind(&d).expect("must replace a stale socket");
        assert!(s.path().exists());
        drop(s);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_socket_is_removed_on_shutdown() {
        let d = tmpdir("cleanup");
        let p = {
            let s = IpcServer::bind(&d).unwrap();
            s.path().to_path_buf()
        };
        assert!(!p.exists(), "socket must not be left behind");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_request_round_trips() {
        let d = tmpdir("roundtrip");
        let mut srv = IpcServer::bind(&d).unwrap();
        let mut cli = UnixStream::connect(srv.path()).unwrap();
        let new = srv.accept();
        assert_eq!(new.len(), 1);
        let tok = new[0].0;

        cli.write_all(b"{\"id\":7,\"op\":\"ping\"}\n").unwrap();
        let reqs = srv.client(tok).unwrap().read_requests().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].op, "ping");
        assert_eq!(reqs[0].id, 7);

        srv.client(tok).unwrap().send(&Response::ok(7, serde_json::json!({"pong": true}))).unwrap();
        let mut r = std::io::BufReader::new(&mut cli);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["pong"], true);
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn several_requests_in_one_write_are_all_parsed() {
        let d = tmpdir("pipeline");
        let mut srv = IpcServer::bind(&d).unwrap();
        let mut cli = UnixStream::connect(srv.path()).unwrap();
        let tok = srv.accept()[0].0;
        cli.write_all(b"{\"op\":\"ping\"}\n{\"op\":\"coverage\"}\n{\"op\":\"stats\"}\n").unwrap();
        let reqs = srv.client(tok).unwrap().read_requests().unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[2].op, "stats");
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_partial_line_waits_for_the_rest() {
        let d = tmpdir("partial");
        let mut srv = IpcServer::bind(&d).unwrap();
        let mut cli = UnixStream::connect(srv.path()).unwrap();
        let tok = srv.accept()[0].0;
        cli.write_all(b"{\"op\":\"pi").unwrap();
        assert!(srv.client(tok).unwrap().read_requests().unwrap().is_empty());
        cli.write_all(b"ng\"}\n").unwrap();
        let reqs = srv.client(tok).unwrap().read_requests().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].op, "ping");
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn malformed_json_kills_neither_the_connection_nor_the_daemon() {
        let d = tmpdir("malformed");
        let mut srv = IpcServer::bind(&d).unwrap();
        let mut cli = UnixStream::connect(srv.path()).unwrap();
        let tok = srv.accept()[0].0;
        cli.write_all(b"this is not json\n{\"op\":\"ping\"}\n").unwrap();
        let reqs = srv.client(tok).unwrap().read_requests().expect("connection must survive");
        assert_eq!(reqs.len(), 1, "the valid request after the garbage must still arrive");
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn an_oversized_request_is_refused() {
        let d = tmpdir("oversize");
        let mut srv = IpcServer::bind(&d).unwrap();
        let mut cli = UnixStream::connect(srv.path()).unwrap();
        let tok = srv.accept()[0].0;
        // 128 KiB with no newline: the buffer cap must trip.
        let junk = vec![b'x'; 128 * 1024];
        let _ = cli.write_all(&junk);
        let r = srv.client(tok).unwrap().read_requests();
        assert!(r.is_err(), "an unbounded line must be refused");
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_client_limit_is_enforced() {
        let d = tmpdir("limit");
        let mut srv = IpcServer::bind(&d).unwrap();
        let mut held = Vec::new();
        for _ in 0..(MAX_CLIENTS + 4) {
            if let Ok(s) = UnixStream::connect(srv.path()) {
                held.push(s);
            }
            srv.accept();
        }
        assert_eq!(srv.client_count(), MAX_CLIENTS);
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn broadcast_only_reaches_subscribers() {
        let d = tmpdir("broadcast");
        let mut srv = IpcServer::bind(&d).unwrap();
        let mut a = UnixStream::connect(srv.path()).unwrap();
        // Take the token accept() actually assigned to `a`. `tokens()` iterates a
        // HashMap, so indexing it would subscribe an arbitrary client and this test
        // would block forever whenever the hash order put `b` first.
        let tok_a = srv.accept()[0].0;
        let _b = UnixStream::connect(srv.path()).unwrap();
        let tok_b = srv.accept()[0].0;
        assert_ne!(tok_a, tok_b);
        srv.client(tok_a).unwrap().subscriptions.push("alert".into());

        srv.broadcast("alert", &serde_json::json!({"title": "hot"}));
        // A timeout so a future regression fails the test instead of hanging the suite.
        a.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut r = std::io::BufReader::new(&mut a);
        let mut line = String::new();
        r.read_line(&mut line).expect("subscriber should have received the push");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["push"], "alert");
        assert_eq!(v["data"]["title"], "hot");
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_disconnected_client_is_detected() {
        let d = tmpdir("disconnect");
        let mut srv = IpcServer::bind(&d).unwrap();
        let cli = UnixStream::connect(srv.path()).unwrap();
        let tok = srv.accept()[0].0;
        drop(cli);
        assert!(srv.client(tok).unwrap().read_requests().is_err());
        assert!(srv.drop_client(tok).is_some());
        assert_eq!(srv.client_count(), 0);
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn peer_credentials_are_readable() {
        let d = tmpdir("peercred");
        let mut srv = IpcServer::bind(&d).unwrap();
        let _cli = UnixStream::connect(srv.path()).unwrap();
        let new = srv.accept();
        assert_eq!(new.len(), 1, "our own uid must be accepted");
        assert_eq!(peer_uid(new[0].1), Some(unsafe { libc::getuid() }));
        drop(srv);
        std::fs::remove_dir_all(&d).ok();
    }
}
