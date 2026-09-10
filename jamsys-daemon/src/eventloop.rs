//! A single-threaded epoll loop.
//!
//! Every periodic and event-driven source in the daemon lives here. There is one thread,
//! one `epoll_wait`, and **one** timer. Ten collectors sharing a 10-second tier produce
//! one wakeup, not ten — which is the whole reason a monitoring daemon can be run on a
//! laptop without measurably shortening its battery life.

use std::io;
use std::os::unix::io::RawFd;

pub const TOK_TIMER: u64 = 1;
pub const TOK_SIGNAL: u64 = 2;
pub const TOK_IPC_LISTEN: u64 = 3;
pub const TOK_JOURNAL: u64 = 4;
pub const TOK_NETLINK_ROUTE: u64 = 5;
pub const TOK_NETLINK_UEVENT: u64 = 6;
pub const TOK_DBUS: u64 = 7;
/// IPC client connections are allocated tokens from here upwards.
pub const TOK_CLIENT_BASE: u64 = 1000;

pub struct EventLoop {
    epfd: RawFd,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    pub token: u64,
    pub readable: bool,
    pub hangup: bool,
}

impl EventLoop {
    pub fn new() -> io::Result<Self> {
        // SAFETY: epoll_create1 with a valid flag; the returned fd is checked.
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(EventLoop { epfd })
    }

    pub fn add(&self, fd: RawFd, token: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, token)
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: EPOLL_CTL_DEL ignores the event pointer on modern kernels, but a
        // non-null pointer is passed for compatibility with pre-2.6.9 behaviour.
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        let r = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, &mut ev) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn ctl(&self, op: libc::c_int, fd: RawFd, token: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLRDHUP) as u32,
            u64: token,
        };
        // SAFETY: `ev` outlives the call; `fd` is owned by the caller.
        let r = unsafe { libc::epoll_ctl(self.epfd, op, fd, &mut ev) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Block until at least one source is ready. `timeout_ms < 0` blocks indefinitely.
    ///
    /// `EINTR` is reported as an empty result rather than an error so a signal during
    /// the wait simply causes the caller's loop to iterate once.
    pub fn wait(&self, out: &mut Vec<Ready>, timeout_ms: i32) -> io::Result<()> {
        out.clear();
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 32];
        // SAFETY: `evs` is a valid array of the declared length.
        let n = unsafe { libc::epoll_wait(self.epfd, evs.as_mut_ptr(), evs.len() as i32, timeout_ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(());
            }
            return Err(e);
        }
        for ev in evs.iter().take(n as usize) {
            out.push(Ready {
                token: ev.u64,
                readable: ev.events & (libc::EPOLLIN as u32) != 0,
                hangup: ev.events & ((libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLRDHUP) as u32) != 0,
            });
        }
        Ok(())
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        // SAFETY: we own epfd.
        unsafe { libc::close(self.epfd) };
    }
}

/// A `timerfd` armed as a one-shot for the next scheduler deadline.
///
/// One-shot rather than periodic on purpose: the scheduler recomputes the next deadline
/// across all tiers after every tick, so intervals can change (idle throttling, config
/// reload) without the timer drifting or firing spuriously.
pub struct Timer {
    fd: RawFd,
}

impl Timer {
    pub fn new() -> io::Result<Self> {
        // CLOCK_MONOTONIC: intervals must not be disturbed by NTP steps.
        // SAFETY: valid clock id and flags.
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Timer { fd })
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Arm a one-shot `ms` from now. `ms == 0` is clamped to 1 ms because an all-zero
    /// `itimerspec` disarms a timerfd instead of firing it immediately.
    pub fn arm_in(&self, ms: u64) -> io::Result<()> {
        let ms = ms.max(1);
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec {
                tv_sec: (ms / 1000) as libc::time_t,
                tv_nsec: ((ms % 1000) * 1_000_000) as i64,
            },
        };
        // SAFETY: `spec` is fully initialised; null old_value is allowed.
        let r = unsafe { libc::timerfd_settime(self.fd, 0, &spec, std::ptr::null_mut()) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Drain the expiration counter. Must be called after the fd reports readable, or
    /// epoll will spin on a level-triggered fd that is never read.
    pub fn consume(&self) -> u64 {
        let mut buf = [0u8; 8];
        // SAFETY: 8-byte read into an 8-byte buffer, which is the timerfd contract.
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, 8) };
        if n == 8 { u64::from_ne_bytes(buf) } else { 0 }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: we own fd.
        unsafe { libc::close(self.fd) };
    }
}

/// Signals delivered as readable data rather than as interrupts, so shutdown is handled
/// in the same place as everything else instead of in an async-signal-safe handler.
pub struct Signals {
    fd: RawFd,
}

impl Signals {
    pub fn new(sigs: &[libc::c_int]) -> io::Result<Self> {
        // SAFETY: sigset is initialised by sigemptyset before use.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            for &s in sigs {
                libc::sigaddset(&mut set, s);
            }
            // Block default delivery, otherwise the process dies before signalfd reads.
            if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: -1 creates a new fd; `set` is valid.
        let fd = unsafe { libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Signals { fd })
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Read all pending signal numbers.
    pub fn read(&self) -> Vec<libc::c_int> {
        let mut out = Vec::new();
        loop {
            let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let sz = std::mem::size_of::<libc::signalfd_siginfo>();
            // SAFETY: reading exactly one siginfo into a correctly-sized buffer.
            let n = unsafe { libc::read(self.fd, &mut si as *mut _ as *mut libc::c_void, sz) };
            if n != sz as isize {
                break;
            }
            out.push(si.ssi_signo as libc::c_int);
        }
        out
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        // SAFETY: we own fd.
        unsafe { libc::close(self.fd) };
    }
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let r = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn timer_fires_once_and_is_reported_by_epoll() {
        let el = EventLoop::new().unwrap();
        let t = Timer::new().unwrap();
        el.add(t.fd(), TOK_TIMER).unwrap();
        t.arm_in(60).unwrap();

        let mut ready = Vec::new();
        let start = Instant::now();
        el.wait(&mut ready, 2000).unwrap();
        let waited = start.elapsed();

        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].token, TOK_TIMER);
        assert!(ready[0].readable);
        assert!(waited.as_millis() >= 50, "fired too early: {waited:?}");
        assert_eq!(t.consume(), 1, "exactly one expiration");

        // One-shot: after consuming, nothing more should arrive.
        ready.clear();
        el.wait(&mut ready, 120).unwrap();
        assert!(ready.is_empty(), "one-shot timer re-fired");
    }

    #[test]
    fn arm_in_zero_still_fires() {
        // An all-zero itimerspec disarms rather than firing; the clamp must prevent that.
        let el = EventLoop::new().unwrap();
        let t = Timer::new().unwrap();
        el.add(t.fd(), TOK_TIMER).unwrap();
        t.arm_in(0).unwrap();
        let mut ready = Vec::new();
        el.wait(&mut ready, 1000).unwrap();
        assert_eq!(ready.len(), 1, "arm_in(0) disarmed the timer instead of firing");
    }

    #[test]
    fn wait_times_out_cleanly_when_nothing_is_ready() {
        let el = EventLoop::new().unwrap();
        let t = Timer::new().unwrap();
        el.add(t.fd(), TOK_TIMER).unwrap();
        let mut ready = Vec::new();
        el.wait(&mut ready, 30).unwrap();
        assert!(ready.is_empty());
    }

    #[test]
    fn removing_an_fd_stops_its_events() {
        let el = EventLoop::new().unwrap();
        let t = Timer::new().unwrap();
        el.add(t.fd(), TOK_TIMER).unwrap();
        el.remove(t.fd()).unwrap();
        t.arm_in(20).unwrap();
        let mut ready = Vec::new();
        el.wait(&mut ready, 120).unwrap();
        assert!(ready.is_empty());
    }

    #[test]
    fn several_sources_are_multiplexed_on_one_wait() {
        let el = EventLoop::new().unwrap();
        let a = Timer::new().unwrap();
        let b = Timer::new().unwrap();
        el.add(a.fd(), 11).unwrap();
        el.add(b.fd(), 22).unwrap();
        a.arm_in(20).unwrap();
        b.arm_in(20).unwrap();
        let mut ready = Vec::new();
        // Both deadlines land inside one wait; a single wakeup must report both.
        std::thread::sleep(std::time::Duration::from_millis(60));
        el.wait(&mut ready, 500).unwrap();
        let toks: Vec<u64> = ready.iter().map(|r| r.token).collect();
        assert!(toks.contains(&11) && toks.contains(&22), "got {toks:?}");
    }
}
