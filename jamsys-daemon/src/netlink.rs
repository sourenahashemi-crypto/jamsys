//! Event-driven kernel notifications: rtnetlink for link/address/route changes, and
//! `NETLINK_KOBJECT_UEVENT` for device hotplug.
//!
//! These replace polling entirely. Without them the daemon would have to re-read
//! `/sys/class/net` every second to notice a Wi-Fi drop; with them it sleeps until the
//! kernel says something happened, which is both faster to react and free when idle.

use std::io;
use std::os::unix::io::RawFd;

const RTMGRP_LINK: u32 = 1;
const RTMGRP_IPV4_IFADDR: u32 = 0x10;
const RTMGRP_IPV4_ROUTE: u32 = 0x40;
const RTMGRP_IPV6_IFADDR: u32 = 0x100;
const RTMGRP_IPV6_ROUTE: u32 = 0x400;

/// A netlink socket bound to a multicast group set.
pub struct NetlinkSocket {
    fd: RawFd,
}

impl NetlinkSocket {
    fn open(protocol: libc::c_int, groups: u32) -> io::Result<NetlinkSocket> {
        // SAFETY: standard socket creation with valid constants.
        let fd = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, protocol)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sockaddr_nl is POD; zeroing then filling is the documented pattern.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_groups = groups;
        // pid 0 lets the kernel assign the port id, which avoids clashing with any
        // other netlink user in this process.
        let r = unsafe {
            libc::bind(
                fd,
                &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        // A burst of uevents during boot or a docking event can overflow the default
        // buffer; a larger one costs kernel memory only while queued.
        let sz: libc::c_int = 1 << 20;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &sz as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        Ok(NetlinkSocket { fd })
    }

    /// Link, address and route changes for both IP families.
    pub fn route() -> io::Result<NetlinkSocket> {
        Self::open(
            libc::NETLINK_ROUTE,
            RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV4_ROUTE | RTMGRP_IPV6_IFADDR | RTMGRP_IPV6_ROUTE,
        )
    }

    /// Device add/remove, as udev sees them.
    pub fn uevent() -> io::Result<NetlinkSocket> {
        Self::open(libc::NETLINK_KOBJECT_UEVENT, 1)
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Drain everything queued. Returns raw datagrams.
    pub fn drain(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 8192];
        loop {
            // SAFETY: reading into a buffer of its own declared length.
            let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n <= 0 {
                break;
            }
            out.push(buf[..n as usize].to_vec());
            if out.len() > 256 {
                // Under a storm, stop draining and let the next loop iteration
                // continue; the alternative is starving every other event source.
                break;
            }
        }
        out
    }
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        // SAFETY: we own fd.
        unsafe { libc::close(self.fd) };
    }
}

/// The rtnetlink message types worth reacting to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RouteEventKind {
    NewLink,
    DelLink,
    NewAddr,
    DelAddr,
    NewRoute,
    DelRoute,
}

/// Classify an rtnetlink datagram by its `nlmsghdr.nlmsg_type`.
///
/// Only the 16-bit type is needed — the daemon re-reads the full interface state from
/// sysfs after any change, which is simpler and less error-prone than incrementally
/// applying netlink attribute diffs.
pub fn parse_route_events(buf: &[u8]) -> Vec<RouteEventKind> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 16 <= buf.len() {
        let len = u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize;
        // A length below the header size would loop forever.
        if len < 16 || off + len > buf.len() {
            break;
        }
        let ty = u16::from_ne_bytes([buf[off + 4], buf[off + 5]]);
        let kind = match ty {
            16 => Some(RouteEventKind::NewLink),
            17 => Some(RouteEventKind::DelLink),
            20 => Some(RouteEventKind::NewAddr),
            21 => Some(RouteEventKind::DelAddr),
            24 => Some(RouteEventKind::NewRoute),
            25 => Some(RouteEventKind::DelRoute),
            _ => None,
        };
        if let Some(k) = kind {
            out.push(k);
        }
        // Messages are 4-byte aligned.
        off += (len + 3) & !3;
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Uevent {
    pub action: String,
    pub subsystem: String,
    pub devpath: String,
    pub devname: String,
}

/// Parse a `NETLINK_KOBJECT_UEVENT` datagram.
///
/// Two formats exist: the legacy `ACTION@DEVPATH\0KEY=VALUE\0...` and libudev's
/// monitor format with an 8-byte `libudev\0` magic prefix. Only the kernel format
/// arrives on group 1, but the prefix is checked so a libudev message is skipped
/// rather than mis-parsed.
pub fn parse_uevent(buf: &[u8]) -> Option<Uevent> {
    if buf.starts_with(b"libudev\0") {
        return None;
    }
    let mut ev = Uevent::default();
    let mut parts = buf.split(|&b| b == 0).filter(|p| !p.is_empty());
    let header = std::str::from_utf8(parts.next()?).ok()?;
    if let Some((action, devpath)) = header.split_once('@') {
        ev.action = action.to_string();
        ev.devpath = devpath.to_string();
    } else {
        return None;
    }
    for p in parts {
        let Ok(s) = std::str::from_utf8(p) else { continue };
        if let Some((k, v)) = s.split_once('=') {
            match k {
                "SUBSYSTEM" => ev.subsystem = v.to_string(),
                "DEVNAME" => ev.devname = v.to_string(),
                "ACTION" if ev.action.is_empty() => ev.action = v.to_string(),
                _ => {}
            }
        }
    }
    (!ev.action.is_empty()).then_some(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nlmsg(ty: u16, extra: usize) -> Vec<u8> {
        let len = 16 + extra;
        let mut v = Vec::new();
        v.extend_from_slice(&(len as u32).to_ne_bytes());
        v.extend_from_slice(&ty.to_ne_bytes());
        v.extend_from_slice(&0u16.to_ne_bytes()); // flags
        v.extend_from_slice(&0u32.to_ne_bytes()); // seq
        v.extend_from_slice(&0u32.to_ne_bytes()); // pid
        v.resize(len, 0);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn route_sockets_can_be_opened_unprivileged() {
        let s = NetlinkSocket::route().expect("rtnetlink needs no privileges");
        assert!(s.fd() >= 0);
        // Nothing is happening, so draining must return immediately and empty.
        assert!(s.drain().is_empty());
    }

    #[test]
    fn uevent_sockets_can_be_opened_unprivileged() {
        let s = NetlinkSocket::uevent().expect("uevent group 1 needs no privileges");
        assert!(s.fd() >= 0);
    }

    #[test]
    fn link_and_route_messages_are_classified() {
        let mut buf = nlmsg(16, 16); // RTM_NEWLINK
        buf.extend_from_slice(&nlmsg(24, 8)); // RTM_NEWROUTE
        buf.extend_from_slice(&nlmsg(21, 4)); // RTM_DELADDR
        let ev = parse_route_events(&buf);
        assert_eq!(ev, vec![RouteEventKind::NewLink, RouteEventKind::NewRoute, RouteEventKind::DelAddr]);
    }

    #[test]
    fn unknown_message_types_are_skipped_not_fatal() {
        let buf = nlmsg(3, 8); // NLMSG_DONE
        assert!(parse_route_events(&buf).is_empty());
    }

    #[test]
    fn a_truncated_or_zero_length_message_cannot_loop_forever() {
        // A declared length of 0 would spin the offset without advancing.
        let mut bad = vec![0u8; 16];
        bad[0] = 0;
        assert!(parse_route_events(&bad).is_empty());
        // Declared longer than the buffer.
        let mut over = nlmsg(16, 0);
        over[0] = 200;
        assert!(parse_route_events(&over).is_empty());
        assert!(parse_route_events(&[]).is_empty());
        assert!(parse_route_events(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn a_kernel_uevent_is_parsed() {
        let mut b = Vec::new();
        b.extend_from_slice(b"add@/devices/pci0000:00/0000:00:14.0/usb1/1-2\0");
        b.extend_from_slice(b"ACTION=add\0");
        b.extend_from_slice(b"DEVPATH=/devices/pci0000:00/0000:00:14.0/usb1/1-2\0");
        b.extend_from_slice(b"SUBSYSTEM=usb\0");
        b.extend_from_slice(b"DEVNAME=bus/usb/001/005\0");
        let e = parse_uevent(&b).unwrap();
        assert_eq!(e.action, "add");
        assert_eq!(e.subsystem, "usb");
        assert_eq!(e.devname, "bus/usb/001/005");
        assert!(e.devpath.ends_with("1-2"));
    }

    #[test]
    fn a_remove_uevent_is_parsed() {
        let mut b = Vec::new();
        b.extend_from_slice(b"remove@/devices/virtual/video4linux/video0\0");
        b.extend_from_slice(b"SUBSYSTEM=video4linux\0");
        let e = parse_uevent(&b).unwrap();
        assert_eq!(e.action, "remove");
        assert_eq!(e.subsystem, "video4linux");
    }

    #[test]
    fn libudev_format_messages_are_ignored() {
        let mut b = Vec::from(&b"libudev\0"[..]);
        b.extend_from_slice(&[0u8; 32]);
        assert!(parse_uevent(&b).is_none());
    }

    #[test]
    fn malformed_uevents_are_rejected() {
        assert!(parse_uevent(b"").is_none());
        assert!(parse_uevent(b"no-at-sign\0SUBSYSTEM=usb\0").is_none());
        // Non-UTF-8 in a value must not panic.
        let mut b = Vec::from(&b"add@/x\0"[..]);
        b.extend_from_slice(&[0xff, 0xfe, 0]);
        assert!(parse_uevent(&b).is_some());
    }
}
