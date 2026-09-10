//! A minimal, self-contained D-Bus client.
//!
//! The daemon needs exactly three things from D-Bus: list systemd units, subscribe to
//! unit changes, and post desktop notifications. The available Rust bindings either pull
//! in an async runtime with its own worker threads (`zbus`) or need `libdbus-1-dev`
//! headers that are not installed on the target. Both conflict with the goals here —
//! one thread, no C build dependencies — so the wire protocol is implemented directly.
//!
//! Only the subset actually used is supported. Anything unrecognised decodes to
//! `Value::Unsupported` rather than failing the whole message.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    ObjectPath(String),
    Signature(String),
    Array(Vec<Value>),
    Struct(Vec<Value>),
    Variant(Box<Value>),
    DictEntry(Box<Value>, Box<Value>),
    Unsupported,
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::ObjectPath(s) | Value::Signature(s) => Some(s),
            Value::Variant(v) => v.as_str(),
            _ => None,
        }
    }
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Value::U32(v) => Some(*v),
            Value::Variant(v) => v.as_u32(),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(v) | Value::Struct(v) => Some(v),
            Value::Variant(v) => v.as_array(),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Marshalling
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Marshal {
    pub buf: Vec<u8>,
}

impl Marshal {
    pub fn new() -> Self {
        Marshal { buf: Vec::with_capacity(256) }
    }

    /// D-Bus aligns every value to its own size; padding is always zero bytes.
    pub fn align(&mut self, n: usize) {
        while self.buf.len() % n != 0 {
            self.buf.push(0);
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    pub fn signature(&mut self, s: &str) {
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    /// Arrays are a u32 byte-length followed by the elements, where the length is
    /// measured *after* aligning to the element type — so it is back-patched.
    pub fn array<F: FnOnce(&mut Marshal)>(&mut self, elem_align: usize, f: F) {
        self.align(4);
        let len_pos = self.buf.len();
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        self.align(elem_align);
        let start = self.buf.len();
        f(self);
        let len = (self.buf.len() - start) as u32;
        self.buf[len_pos..len_pos + 4].copy_from_slice(&len.to_le_bytes());
    }

    pub fn struct_<F: FnOnce(&mut Marshal)>(&mut self, f: F) {
        self.align(8);
        f(self);
    }

    /// A variant is its own signature followed by the value.
    pub fn variant_str(&mut self, s: &str) {
        self.signature("s");
        self.string(s);
    }
    pub fn variant_u32(&mut self, v: u32) {
        self.signature("u");
        self.u32(v);
    }
    pub fn variant_bool(&mut self, v: bool) {
        self.signature("b");
        self.u32(if v { 1 } else { 0 });
    }
}

// ---------------------------------------------------------------------------
// Unmarshalling
// ---------------------------------------------------------------------------

pub struct Unmarshal<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Unmarshal<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Unmarshal { b, pos: 0 }
    }

    fn align(&mut self, n: usize) {
        while self.pos % n != 0 && self.pos < self.b.len() {
            self.pos += 1;
        }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.b.len() {
            return None;
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.align(4);
        self.take(4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u16(&mut self) -> Option<u16> {
        self.align(2);
        self.take(2).map(|s| u16::from_le_bytes([s[0], s[1]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.align(8);
        self.take(8).and_then(|s| s.try_into().ok()).map(u64::from_le_bytes)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        self.pos += 1; // NUL
        String::from_utf8(s.to_vec()).ok()
    }
    fn sigstring(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        let s = self.take(n)?;
        self.pos += 1; // NUL
        String::from_utf8(s.to_vec()).ok()
    }

    /// Read one complete value described by the signature starting at `sig[*i]`,
    /// advancing `i` past the type it consumed.
    pub fn value(&mut self, sig: &[u8], i: &mut usize) -> Option<Value> {
        let t = *sig.get(*i)?;
        *i += 1;
        Some(match t {
            b'y' => Value::Byte(self.u8()?),
            b'b' => Value::Bool(self.u32()? != 0),
            b'n' => Value::I16(self.u16()? as i16),
            b'q' => Value::U16(self.u16()?),
            b'i' => Value::I32(self.u32()? as i32),
            b'u' => Value::U32(self.u32()?),
            b'x' => Value::I64(self.u64()? as i64),
            b't' => Value::U64(self.u64()?),
            b'd' => {
                self.align(8);
                Value::F64(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
            }
            b's' => Value::Str(self.string()?),
            b'o' => Value::ObjectPath(self.string()?),
            b'g' => Value::Signature(self.sigstring()?),
            b'v' => {
                let s = self.sigstring()?;
                let sb = s.as_bytes();
                let mut j = 0;
                Value::Variant(Box::new(self.value(sb, &mut j)?))
            }
            b'a' => {
                let elem_start = *i;
                let n = self.u32()? as usize;
                // The array's declared length is measured after aligning to the
                // element type, so align before recording the start offset.
                self.align(elem_alignment(sig, elem_start));
                let end = self.pos + n;
                let mut items = Vec::new();
                while self.pos < end && self.pos < self.b.len() {
                    let mut j = elem_start;
                    items.push(self.value(sig, &mut j)?);
                    if items.len() > 100_000 {
                        break; // defensive: never allocate unboundedly from the wire
                    }
                }
                self.pos = end.min(self.b.len());
                skip_type(sig, i);
                Value::Array(items)
            }
            b'(' => {
                self.align(8);
                let mut items = Vec::new();
                while *i < sig.len() && sig[*i] != b')' {
                    items.push(self.value(sig, i)?);
                }
                *i += 1; // ')'
                Value::Struct(items)
            }
            b'{' => {
                self.align(8);
                let k = self.value(sig, i)?;
                let v = self.value(sig, i)?;
                *i += 1; // '}'
                Value::DictEntry(Box::new(k), Box::new(v))
            }
            b'h' => Value::U32(self.u32()?), // unix fd index
            _ => Value::Unsupported,
        })
    }
}

/// Alignment of the type that starts at `sig[i]`.
fn elem_alignment(sig: &[u8], i: usize) -> usize {
    match sig.get(i).copied().unwrap_or(b'y') {
        b'y' | b'g' | b'v' => 1,
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b's' | b'o' | b'a' | b'h' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

/// Advance `i` past one complete type in a signature, including nested containers.
fn skip_type(sig: &[u8], i: &mut usize) {
    let Some(&t) = sig.get(*i) else { return };
    *i += 1;
    match t {
        b'a' => skip_type(sig, i),
        b'(' => {
            let mut depth = 1;
            while *i < sig.len() && depth > 0 {
                match sig[*i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                *i += 1;
            }
        }
        b'{' => {
            let mut depth = 1;
            while *i < sig.len() && depth > 0 {
                match sig[*i] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                *i += 1;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

pub struct Connection {
    sock: UnixStream,
    serial: u32,
    pub unique_name: String,
    /// Receive buffer. A non-blocking read can return a partial message, so bytes
    /// accumulate here until a whole one is present.
    rbuf: Vec<u8>,
}

#[derive(Debug)]
pub struct Message {
    pub msg_type: u8,
    pub serial: u32,
    pub reply_serial: Option<u32>,
    pub member: Option<String>,
    pub interface: Option<String>,
    pub path: Option<String>,
    pub error_name: Option<String>,
    /// Unique bus name of the caller. **A reply must be addressed back to this**, or
    /// the bus has no destination to route it to and silently drops it — which
    /// presents as the client hanging until its timeout.
    pub sender: Option<String>,
    pub signature: String,
    pub body: Vec<u8>,
}

impl Message {
    pub fn args(&self) -> Vec<Value> {
        let sig = self.signature.as_bytes();
        let mut u = Unmarshal::new(&self.body);
        let mut i = 0;
        let mut out = Vec::new();
        while i < sig.len() {
            match u.value(sig, &mut i) {
                Some(v) => out.push(v),
                None => break,
            }
        }
        out
    }
}

impl Connection {
    pub fn session() -> std::io::Result<Connection> {
        let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
        let path = addr
            .split(',')
            .find_map(|p| p.strip_prefix("unix:path=").map(|s| s.to_string()))
            // The well-known fallback when the variable is not exported into a
            // systemd --user unit's environment.
            .unwrap_or_else(|| format!("/run/user/{}/bus", unsafe { libc::getuid() }));
        Connection::connect(&path)
    }

    pub fn system() -> std::io::Result<Connection> {
        let addr = std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap_or_default();
        let path = addr
            .split(',')
            .find_map(|p| p.strip_prefix("unix:path=").map(|s| s.to_string()))
            .unwrap_or_else(|| "/var/run/dbus/system_bus_socket".to_string());
        Connection::connect(&path)
    }

    pub fn connect(path: &str) -> std::io::Result<Connection> {
        let mut sock = UnixStream::connect(path)?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        sock.set_write_timeout(Some(Duration::from_secs(5)))?;

        // SASL handshake. EXTERNAL auth uses the peer credentials the kernel already
        // attached to the socket, so no secret is exchanged.
        sock.write_all(&[0u8])?;
        let uid = unsafe { libc::getuid() };
        let hex: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        sock.write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes())?;
        let line = read_line(&mut sock)?;
        if !line.starts_with("OK") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("D-Bus auth rejected: {}", line.trim()),
            ));
        }
        sock.write_all(b"BEGIN\r\n")?;

        let mut c = Connection { sock, serial: 0, unique_name: String::new(), rbuf: Vec::with_capacity(4096) };
        // Hello() is mandatory before any other traffic.
        let reply = c.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", "", &[])?;
        if let Some(Value::Str(n)) = reply.args().first() {
            c.unique_name = n.clone();
        }
        Ok(c)
    }

    pub fn set_timeout(&self, d: Option<Duration>) {
        let _ = self.sock.set_read_timeout(d);
    }

    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.sock.as_raw_fd()
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    /// Send a method call and wait for its reply.
    pub fn call(
        &mut self,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> std::io::Result<Message> {
        let serial = self.next_serial();
        let msg = build_method_call(serial, dest, path, iface, member, signature, body);
        self.sock.write_all(&msg)?;
        // Skip signals and unrelated replies until ours arrives.
        for _ in 0..64 {
            let m = self.read_message()?;
            if m.reply_serial == Some(serial) {
                if m.msg_type == 3 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("{}: {:?}", m.error_name.clone().unwrap_or_default(), m.args().first()),
                    ));
                }
                return Ok(m);
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no matching D-Bus reply"))
    }

    /// Reply to an incoming method call.
    ///
    /// `destination` is the caller's unique name, taken from the request's SENDER
    /// field. Omitting it makes the bus drop the reply and the client hang.
    pub fn send_reply(&mut self, destination: Option<&str>, reply_serial: u32,
                      signature: &str, body: &[u8]) -> std::io::Result<()> {
        let serial = self.next_serial();
        let msg = build_reply(serial, destination, reply_serial, signature, body);
        self.write_all_nb(&msg)
    }

    /// Fire and forget — used for `AddMatch` and signal subscriptions.
    pub fn send_no_reply(
        &mut self,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> std::io::Result<()> {
        let serial = self.next_serial();
        let mut msg = build_method_call(serial, dest, path, iface, member, signature, body);
        msg[2] |= 0x01; // NO_REPLY_EXPECTED
        self.sock.write_all(&msg)
    }

    /// Put the socket into non-blocking mode so it can be driven by epoll.
    pub fn set_nonblocking(&self) -> std::io::Result<()> {
        self.sock.set_nonblocking(true)
    }

    /// Claim a well-known bus name. Returns the D-Bus reply code; 1 means this
    /// connection is now the primary owner.
    pub fn request_name(&mut self, name: &str) -> std::io::Result<u32> {
        let mut b = Marshal::new();
        b.string(name);
        b.u32(4); // DBUS_NAME_FLAG_DO_NOT_QUEUE
        let r = self.call("org.freedesktop.DBus", "/org/freedesktop/DBus",
                          "org.freedesktop.DBus", "RequestName", "su", &b.buf)?;
        Ok(r.args().first().and_then(|v| v.as_u32()).unwrap_or(0))
    }

    /// Emit a signal. Signals are broadcast; no reply is expected or possible.
    pub fn send_signal(&mut self, path: &str, iface: &str, member: &str,
                       signature: &str, body: &[u8]) -> std::io::Result<()> {
        let serial = self.next_serial();
        let msg = build_signal(serial, path, iface, member, signature, body);
        self.write_all_nb(&msg)
    }

    /// Reply to a method call with an error rather than a return value.
    pub fn send_error(&mut self, destination: Option<&str>, reply_serial: u32,
                      error_name: &str, message: &str) -> std::io::Result<()> {
        let serial = self.next_serial();
        let mut body = Marshal::new();
        body.string(message);
        let msg = build_error(serial, destination, reply_serial, error_name, &body.buf);
        self.write_all_nb(&msg)
    }

    /// Write that tolerates a non-blocking socket. A bus peer that stops reading must
    /// not be able to block the daemon's event loop, so a would-block is reported
    /// rather than spun on.
    fn write_all_nb(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let mut off = 0;
        let mut spins = 0;
        while off < buf.len() {
            match self.sock.write(&buf[off..]) {
                Ok(0) => return Err(std::io::Error::new(ErrorKind::WriteZero, "bus socket closed")),
                Ok(n) => { off += n; spins = 0; }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    spins += 1;
                    if spins > 200 {
                        return Err(std::io::Error::new(ErrorKind::WouldBlock, "bus peer not draining"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Drain the socket and return whatever complete messages arrived. Never blocks.
    pub fn try_read_messages(&mut self) -> std::io::Result<Vec<Message>> {
        let mut chunk = [0u8; 8192];
        loop {
            match self.sock.read(&mut chunk) {
                Ok(0) => return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "bus closed")),
                Ok(n) => {
                    self.rbuf.extend_from_slice(&chunk[..n]);
                    if self.rbuf.len() > 8 << 20 {
                        return Err(std::io::Error::new(ErrorKind::InvalidData, "bus backlog too large"));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let mut out = Vec::new();
        while let Some(m) = self.take_buffered_message()? {
            out.push(m);
        }
        Ok(out)
    }

    /// Parse one complete message out of `rbuf`, or return `None` if it is incomplete.
    fn take_buffered_message(&mut self) -> std::io::Result<Option<Message>> {
        if self.rbuf.len() < 16 {
            return Ok(None);
        }
        let h = &self.rbuf[..16];
        if h[0] != b'l' {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "big-endian D-Bus message"));
        }
        let body_len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        let fields_len = u32::from_le_bytes([h[12], h[13], h[14], h[15]]) as usize;
        if body_len > 64 << 20 || fields_len > 1 << 20 {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "oversized D-Bus message"));
        }
        let pad = (8 - (fields_len % 8)) % 8;
        let total = 16 + fields_len + pad + body_len;
        if self.rbuf.len() < total {
            return Ok(None);
        }
        let raw: Vec<u8> = self.rbuf.drain(..total).collect();
        Ok(Some(Self::decode(&raw, fields_len, pad, body_len)))
    }

    fn decode(raw: &[u8], fields_len: usize, pad: usize, body_len: usize) -> Message {
        let msg_type = raw[1];
        let serial = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
        let fields = &raw[16..16 + fields_len];
        let bstart = 16 + fields_len + pad;
        let body = raw[bstart..bstart + body_len].to_vec();
        let mut m = Message {
            msg_type, serial, reply_serial: None, member: None, interface: None,
            path: None, error_name: None, sender: None, signature: String::new(), body,
        };
        let mut u = Unmarshal::new(fields);
        while u.pos < fields.len() {
            u.align(8);
            let Some(code) = u.u8() else { break };
            let Some(sig) = u.sigstring() else { break };
            let sb = sig.as_bytes();
            let mut i = 0;
            let Some(v) = u.value(sb, &mut i) else { break };
            match code {
                1 => m.path = v.as_str().map(String::from),
                2 => m.interface = v.as_str().map(String::from),
                3 => m.member = v.as_str().map(String::from),
                4 => m.error_name = v.as_str().map(String::from),
                5 => m.reply_serial = v.as_u32(),
                7 => m.sender = v.as_str().map(String::from),
                8 => m.signature = v.as_str().unwrap_or("").to_string(),
                _ => {}
            }
        }
        m
    }

    pub fn read_message(&mut self) -> std::io::Result<Message> {
        let mut hdr = [0u8; 16];
        self.sock.read_exact(&mut hdr)?;
        if hdr[0] != b'l' {
            // Big-endian peers are legal but do not occur on Linux desktops; refuse
            // clearly rather than silently misparsing.
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "big-endian D-Bus message"));
        }
        let msg_type = hdr[1];
        let body_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        let serial = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        let fields_len = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]) as usize;
        if body_len > 64 << 20 || fields_len > 1 << 20 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "oversized D-Bus message"));
        }
        let mut fields = vec![0u8; fields_len];
        self.sock.read_exact(&mut fields)?;
        // The header field array is padded to an 8-byte boundary before the body.
        let pad = (8 - (fields_len % 8)) % 8;
        if pad > 0 {
            let mut p = vec![0u8; pad];
            self.sock.read_exact(&mut p)?;
        }
        let mut body = vec![0u8; body_len];
        if body_len > 0 {
            self.sock.read_exact(&mut body)?;
        }

        let mut m = Message {
            msg_type,
            serial,
            reply_serial: None,
            member: None,
            interface: None,
            path: None,
            error_name: None,
            sender: None,
            signature: String::new(),
            body,
        };
        // Header fields are a(yv) but were read as a standalone buffer, so decode the
        // dict entries directly.
        let mut u = Unmarshal::new(&fields);
        while u.pos < fields.len() {
            u.align(8);
            let Some(code) = u.u8() else { break };
            let Some(sig) = u.sigstring() else { break };
            let sb = sig.as_bytes();
            let mut i = 0;
            let Some(v) = u.value(sb, &mut i) else { break };
            match code {
                1 => m.path = v.as_str().map(String::from),
                2 => m.interface = v.as_str().map(String::from),
                3 => m.member = v.as_str().map(String::from),
                4 => m.error_name = v.as_str().map(String::from),
                5 => m.reply_serial = v.as_u32(),
                7 => m.sender = v.as_str().map(String::from),
                8 => m.signature = v.as_str().unwrap_or("").to_string(),
                _ => {}
            }
        }
        Ok(m)
    }

    /// Subscribe to a signal. `rule` is a standard match rule string.
    pub fn add_match(&mut self, rule: &str) -> std::io::Result<()> {
        let mut b = Marshal::new();
        b.string(rule);
        self.send_no_reply(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            "s",
            &b.buf,
        )
    }
}

/// Build a METHOD_RETURN. Header fields are REPLY_SERIAL and, when the body is
/// non-empty, SIGNATURE.
fn build_reply(serial: u32, destination: Option<&str>, reply_serial: u32,
               signature: &str, body: &[u8]) -> Vec<u8> {
    let mut h = Marshal::new();
    h.u8(b'l');
    h.u8(2); // METHOD_RETURN
    h.u8(1); // NO_REPLY_EXPECTED
    h.u8(1);
    h.u32(body.len() as u32);
    h.u32(serial);
    h.array(8, |m| {
        m.align(8);
        m.u8(5); // REPLY_SERIAL
        m.signature("u");
        m.u32(reply_serial);
        if let Some(d) = destination {
            m.align(8);
            m.u8(6); // DESTINATION — without this the bus cannot route the reply
            m.signature("s");
            m.string(d);
        }
        if !signature.is_empty() {
            m.align(8);
            m.u8(8); // SIGNATURE
            m.signature("g");
            m.signature(signature);
        }
    });
    while h.buf.len() % 8 != 0 {
        h.buf.push(0);
    }
    h.buf.extend_from_slice(body);
    h.buf
}

fn build_signal(serial: u32, path: &str, iface: &str, member: &str,
                signature: &str, body: &[u8]) -> Vec<u8> {
    let mut h = Marshal::new();
    h.u8(b'l');
    h.u8(4); // SIGNAL
    h.u8(1); // NO_REPLY_EXPECTED
    h.u8(1);
    h.u32(body.len() as u32);
    h.u32(serial);
    h.array(8, |m| {
        m.align(8); m.u8(1); m.signature("o"); m.string(path);
        m.align(8); m.u8(2); m.signature("s"); m.string(iface);
        m.align(8); m.u8(3); m.signature("s"); m.string(member);
        if !signature.is_empty() {
            m.align(8); m.u8(8); m.signature("g"); m.signature(signature);
        }
    });
    while h.buf.len() % 8 != 0 { h.buf.push(0); }
    h.buf.extend_from_slice(body);
    h.buf
}

fn build_error(serial: u32, destination: Option<&str>, reply_serial: u32,
               error_name: &str, body: &[u8]) -> Vec<u8> {
    let mut h = Marshal::new();
    h.u8(b'l');
    h.u8(3); // ERROR
    h.u8(1);
    h.u8(1);
    h.u32(body.len() as u32);
    h.u32(serial);
    h.array(8, |m| {
        m.align(8); m.u8(4); m.signature("s"); m.string(error_name);
        m.align(8); m.u8(5); m.signature("u"); m.u32(reply_serial);
        if let Some(d) = destination {
            m.align(8); m.u8(6); m.signature("s"); m.string(d);
        }
        m.align(8); m.u8(8); m.signature("g"); m.signature("s");
    });
    while h.buf.len() % 8 != 0 { h.buf.push(0); }
    h.buf.extend_from_slice(body);
    h.buf
}

fn build_method_call(
    serial: u32,
    dest: &str,
    path: &str,
    iface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut h = Marshal::new();
    h.u8(b'l'); // little endian
    h.u8(1); // METHOD_CALL
    h.u8(0); // flags
    h.u8(1); // protocol version
    h.u32(body.len() as u32);
    h.u32(serial);
    // Header fields: a(yv)
    h.array(8, |m| {
        let mut field = |code: u8, sig: &str, write: &mut dyn FnMut(&mut Marshal)| {
            m.align(8);
            m.u8(code);
            m.signature(sig);
            write(m);
        };
        field(1, "o", &mut |m| m.string(path));
        if !iface.is_empty() {
            field(2, "s", &mut |m| m.string(iface));
        }
        field(3, "s", &mut |m| m.string(member));
        if !dest.is_empty() {
            field(6, "s", &mut |m| m.string(dest));
        }
        if !signature.is_empty() {
            field(8, "g", &mut |m| m.signature(signature));
        }
    });
    // Body begins on an 8-byte boundary.
    while h.buf.len() % 8 != 0 {
        h.buf.push(0);
    }
    h.buf.extend_from_slice(body);
    h.buf
}

fn read_line(s: &mut UnixStream) -> std::io::Result<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    while out.len() < 512 {
        s.read_exact(&mut b)?;
        out.push(b[0]);
        if out.ends_with(b"\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// ---------------------------------------------------------------------------
// Desktop notifications
// ---------------------------------------------------------------------------

/// Post a notification via `org.freedesktop.Notifications`.
///
/// `replaces_id` of 0 creates a new bubble; passing a previously-returned id updates
/// that bubble in place, which is how an escalating alert avoids stacking up.
pub fn notify(
    conn: &mut Connection,
    app_name: &str,
    replaces_id: u32,
    icon: &str,
    summary: &str,
    body: &str,
    urgency: u8,
    timeout_ms: i32,
) -> std::io::Result<u32> {
    let mut b = Marshal::new();
    b.string(app_name);
    b.u32(replaces_id);
    b.string(icon);
    b.string(summary);
    b.string(body);
    b.array(4, |_| {}); // actions: as (empty)
    // hints: a{sv}
    b.array(8, |m| {
        m.struct_(|m| {
            m.string("urgency");
            m.signature("y");
            m.u8(urgency);
        });
        m.struct_(|m| {
            m.string("category");
            m.variant_str("device");
        });
    });
    b.i32(timeout_ms);
    let reply = conn.call(
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        "susssasa{sv}i",
        &b.buf,
    )?;
    Ok(reply.args().first().and_then(|v| v.as_u32()).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Unit {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
}

/// `ListUnitsFiltered(["failed"])` — asking the manager to filter is much cheaper than
/// listing several hundred units and filtering here.
pub fn list_units_filtered(conn: &mut Connection, states: &[&str]) -> std::io::Result<Vec<Unit>> {
    let mut b = Marshal::new();
    b.array(4, |m| {
        for s in states {
            m.string(s);
        }
    });
    let reply = conn.call(
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "ListUnitsFiltered",
        "as",
        &b.buf,
    )?;
    let mut out = Vec::new();
    if let Some(Value::Array(items)) = reply.args().first() {
        for it in items {
            if let Value::Struct(f) = it {
                let g = |i: usize| f.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
                out.push(Unit {
                    name: g(0),
                    description: g(1),
                    load_state: g(2),
                    active_state: g(3),
                    sub_state: g(4),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_marshalled_with_length_and_nul() {
        let mut m = Marshal::new();
        m.string("abc");
        assert_eq!(m.buf, vec![3, 0, 0, 0, b'a', b'b', b'c', 0]);
    }

    #[test]
    fn values_are_aligned_to_their_size() {
        let mut m = Marshal::new();
        m.u8(1);
        m.u32(2);
        // One byte, then three pad bytes, then the u32 at offset 4.
        assert_eq!(m.buf.len(), 8);
        assert_eq!(&m.buf[4..8], &2u32.to_le_bytes());
    }

    #[test]
    fn a_string_array_round_trips() {
        let mut m = Marshal::new();
        m.array(4, |x| {
            x.string("failed");
            x.string("active");
        });
        let mut u = Unmarshal::new(&m.buf);
        let mut i = 0;
        let v = u.value(b"as", &mut i).unwrap();
        match v {
            Value::Array(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].as_str(), Some("failed"));
                assert_eq!(items[1].as_str(), Some("active"));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_array_decodes_to_an_empty_vec() {
        let mut m = Marshal::new();
        m.array(4, |_| {});
        let mut u = Unmarshal::new(&m.buf);
        let mut i = 0;
        assert_eq!(u.value(b"as", &mut i).unwrap(), Value::Array(vec![]));
    }

    #[test]
    fn signature_skipping_handles_nesting() {
        let sig = b"a(ssssssouso)u";
        let mut i = 0;
        skip_type(sig, &mut i);
        assert_eq!(i, 13, "should land on the trailing 'u'");
        assert_eq!(sig[i], b'u');
    }

    #[test]
    fn variants_decode_to_their_inner_value() {
        let mut m = Marshal::new();
        m.variant_str("hello");
        let mut u = Unmarshal::new(&m.buf);
        let mut i = 0;
        let v = u.value(b"v", &mut i).unwrap();
        assert_eq!(v.as_str(), Some("hello"));
    }

    #[test]
    fn a_truncated_message_body_does_not_panic() {
        // Claims a 100-byte string but supplies 4 bytes: must be None, not a crash.
        let buf = vec![100u8, 0, 0, 0, 1, 2, 3, 4];
        let mut u = Unmarshal::new(&buf);
        let mut i = 0;
        assert!(u.value(b"s", &mut i).is_none());
    }

    #[test]
    fn a_method_call_header_is_well_formed() {
        let msg = build_method_call(7, "org.freedesktop.DBus", "/org/freedesktop/DBus",
                                    "org.freedesktop.DBus", "Hello", "", &[]);
        assert_eq!(msg[0], b'l');
        assert_eq!(msg[1], 1, "METHOD_CALL");
        assert_eq!(msg[3], 1, "protocol version");
        assert_eq!(u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]), 0, "empty body");
        assert_eq!(u32::from_le_bytes([msg[8], msg[9], msg[10], msg[11]]), 7, "serial");
        assert_eq!(msg.len() % 8, 0, "body must start 8-byte aligned");
    }

    #[test]
    fn connects_to_the_real_session_bus_and_lists_failed_units() {
        // This is an integration check against the live system; skip cleanly where
        // there is no bus (CI containers, build chroots).
        let Ok(mut sys) = Connection::system() else {
            eprintln!("no system bus available, skipping");
            return;
        };
        assert!(sys.unique_name.starts_with(':'), "Hello() should return a unique name");
        let units = list_units_filtered(&mut sys, &["failed"]).expect("ListUnitsFiltered");
        for u in &units {
            assert_eq!(u.active_state, "failed");
            assert!(!u.name.is_empty());
        }
        eprintln!("failed units seen: {:?}", units.iter().map(|u| &u.name).collect::<Vec<_>>());
    }
}
