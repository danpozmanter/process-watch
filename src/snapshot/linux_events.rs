use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use super::ProcEvent;

const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const PROC_EVENT_FORK: u32 = 0x0000_0001;
const PROC_EVENT_EXIT: u32 = 0x8000_0000;
const NLMSG_HDR_LEN: usize = 16;
const CN_MSG_HDR_LEN: usize = 20;
/// Where a proc_event's data starts, past `what`, `cpu`, and `timestamp_ns`.
const EVENT_DATA: usize = 16;
/// The kernel caps the receive buffer it grants at net.core.rmem_max.
const RECEIVE_BUFFER_BYTES: libc::c_int = 8 << 20;

/// What the reader hands over: every event since the last hand-over, or
/// `None` when the socket overflowed and some were lost.
type Batch = Option<Vec<ProcEvent>>;

/// Fork and exit events from the kernel's process connector, which Linux 6.6
/// and later deliver to unprivileged listeners. A thread drains the socket
/// as events arrive, so a burst of short-lived processes between samples
/// does not overflow it.
pub struct EventFeed {
    wake: Arc<OwnedFd>,
    batches: Option<Receiver<Batch>>,
    reader: Option<JoinHandle<()>>,
}

impl EventFeed {
    /// A subscribed feed, or `None` where the kernel refuses this process.
    pub fn subscribe() -> Option<Self> {
        let socket = open_socket().ok()?;
        let wake = Arc::new(semaphore().ok()?);
        let (tx, rx) = mpsc::channel();
        let reader_wake = Arc::clone(&wake);
        let reader = std::thread::Builder::new()
            .name("pw-events".to_owned())
            .spawn(move || read_events(&socket, &reader_wake, &tx))
            .ok()?;
        Some(Self {
            wake,
            batches: Some(rx),
            reader: Some(reader),
        })
    }

    /// Every event up to this call since the previous drain, or `None` when
    /// some were lost or the reader has stopped.
    pub fn drain(&mut self) -> Batch {
        post(&self.wake).ok()?;
        self.batches.as_ref()?.recv().ok()?
    }
}

impl Drop for EventFeed {
    fn drop(&mut self) {
        // With the receiver gone, the reader's next hand-over fails and it returns.
        self.batches = None;
        let Some(reader) = self.reader.take() else {
            return;
        };
        if post(&self.wake).is_ok() {
            // The reader does not panic, so joining it reports nothing.
            let _ = reader.join();
        }
    }
}

/// The reader thread: collects events until the sampler posts the
/// semaphore, then hands over everything received up to that moment.
fn read_events(socket: &OwnedFd, wake: &OwnedFd, batches: &Sender<Batch>) {
    let mut batch: Batch = Some(Vec::new());
    let mut buf = [0u8; 4096];
    while let Ok(woken) = wait(socket, wake) {
        receive_all(socket, &mut buf, &mut batch);
        if woken && (take(wake).is_err() || batches.send(batch.replace(Vec::new())).is_err()) {
            return;
        }
    }
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A netlink socket subscribed to every process's forks and exits.
fn open_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket takes no pointers.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_CONNECTOR,
        )
    };
    check(fd)?;
    // SAFETY: `fd` was just opened and nothing else owns it.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: sockaddr_nl is plain data whose all-zero value is valid.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = CN_IDX_PROC;
    // SAFETY: `addr` is a sockaddr_nl of the length passed.
    check(unsafe {
        libc::bind(
            fd,
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    })?;
    let receive_buffer = RECEIVE_BUFFER_BYTES;
    // SAFETY: the option value is a c_int of the length passed.
    check(unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&raw const receive_buffer).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    })?;
    // Every kernel takes the bare form, which subscribes to all events;
    // 6.6 and later then narrow it to forks and exits.
    send_control(&socket, &PROC_CN_MCAST_LISTEN.to_ne_bytes())?;
    let mut narrowed = [0u8; 8];
    narrowed[..4].copy_from_slice(&PROC_CN_MCAST_LISTEN.to_ne_bytes());
    narrowed[4..].copy_from_slice(&(PROC_EVENT_FORK | PROC_EVENT_EXIT).to_ne_bytes());
    send_control(&socket, &narrowed)?;
    Ok(socket)
}

/// Sends a process connector control message to the kernel.
fn send_control(socket: &OwnedFd, payload: &[u8]) -> io::Result<()> {
    let total = NLMSG_HDR_LEN + CN_MSG_HDR_LEN + payload.len();
    let mut msg = Vec::with_capacity(total);
    // nlmsghdr: length, type, flags, sequence, port.
    msg.extend_from_slice(&(total as u32).to_ne_bytes());
    msg.extend_from_slice(&(libc::NLMSG_DONE as u16).to_ne_bytes());
    msg.extend_from_slice(&[0; 10]);
    // cn_msg: index, value, sequence, ack, payload length, flags.
    msg.extend_from_slice(&CN_IDX_PROC.to_ne_bytes());
    msg.extend_from_slice(&CN_VAL_PROC.to_ne_bytes());
    msg.extend_from_slice(&[0; 8]);
    msg.extend_from_slice(&(payload.len() as u16).to_ne_bytes());
    msg.extend_from_slice(&[0; 2]);
    msg.extend_from_slice(payload);
    // SAFETY: `msg` holds `msg.len()` readable bytes.
    let sent = unsafe { libc::send(socket.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) };
    whole(sent, msg.len())
}

/// Whether a read or write moved all `len` bytes; a negative count is the OS's error.
fn whole(moved: isize, len: usize) -> io::Result<()> {
    match usize::try_from(moved) {
        Ok(n) if n == len => Ok(()),
        Ok(n) => Err(io::Error::other(format!("moved {n} of {len} bytes"))),
        Err(_) => Err(io::Error::last_os_error()),
    }
}

/// A non-blocking semaphore eventfd: each post wakes the reader for one hand-over.
fn semaphore() -> io::Result<OwnedFd> {
    // SAFETY: eventfd takes no pointers.
    let fd = unsafe {
        libc::eventfd(
            0,
            libc::EFD_CLOEXEC | libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE,
        )
    };
    check(fd)?;
    // SAFETY: `fd` was just opened and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn post(semaphore: &OwnedFd) -> io::Result<()> {
    let one = 1u64;
    // SAFETY: `one` is the 8 readable bytes an eventfd write takes.
    let wrote = unsafe { libc::write(semaphore.as_raw_fd(), (&raw const one).cast(), 8) };
    whole(wrote, 8)
}

fn take(semaphore: &OwnedFd) -> io::Result<()> {
    let mut count = 0u64;
    // SAFETY: `count` is the 8 writable bytes an eventfd read fills.
    let read = unsafe { libc::read(semaphore.as_raw_fd(), (&raw mut count).cast(), 8) };
    whole(read, 8)
}

/// Blocks until the socket has messages or the semaphore is posted, and
/// answers whether it was posted.
fn wait(socket: &OwnedFd, wake: &OwnedFd) -> io::Result<bool> {
    let watch = |fd: &OwnedFd| libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut fds = [watch(socket), watch(wake)];
    loop {
        // SAFETY: `fds` holds the two pollfd records the call is told of.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc >= 0 {
            return Ok(fds[1].revents & libc::POLLIN != 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Reads every message waiting on the socket into `batch`, marking the batch
/// lost when the kernel dropped messages the socket had no room for.
fn receive_all(socket: &OwnedFd, buf: &mut [u8], batch: &mut Batch) {
    loop {
        match receive(socket, buf) {
            Ok(Some(len)) => {
                if let Some(events) = batch {
                    events.extend(parse(&buf[..len]));
                }
            }
            Ok(None) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => *batch = None,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            Err(_) => {
                *batch = None;
                return;
            }
        }
    }
}

/// One message's length, or `None` for a message the kernel did not send.
fn receive(socket: &OwnedFd, buf: &mut [u8]) -> io::Result<Option<usize>> {
    // SAFETY: sockaddr_nl is plain data whose all-zero value is valid.
    let mut from: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    let mut from_len = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
    // SAFETY: `buf` and `from` are writable for the lengths passed.
    let len = unsafe {
        libc::recvfrom(
            socket.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            (&raw mut from).cast(),
            &raw mut from_len,
        )
    };
    let len = usize::try_from(len).map_err(|_| io::Error::last_os_error())?;
    Ok((from.nl_pid == 0).then_some(len))
}

fn u32_at(msg: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(msg.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(msg: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(msg.get(at..at + 8)?.try_into().ok()?))
}

/// The process fork or exit a connector message reports; a thread's own
/// forks and exits, and every other event, answer `None`.
fn parse(msg: &[u8]) -> Option<ProcEvent> {
    let cn = NLMSG_HDR_LEN;
    if u32_at(msg, cn)? != CN_IDX_PROC || u32_at(msg, cn + 4)? != CN_VAL_PROC {
        return None;
    }
    let event = cn + CN_MSG_HDR_LEN;
    let what = u32_at(msg, event)?;
    let at_ns = i64::try_from(u64_at(msg, event + 8)?).ok()?;
    let field = |n: usize| u32_at(msg, event + EVENT_DATA + 4 * n).map(i64::from);
    match what {
        // parent_pid, parent_tgid, child_pid, child_tgid.
        PROC_EVENT_FORK => (field(2)? == field(3)?).then_some(ProcEvent::Fork {
            parent: field(1)?,
            child: field(3)?,
            at_ns,
        }),
        // process_pid, process_tgid, exit_code, exit_signal, parent_pid, parent_tgid.
        PROC_EVENT_EXIT => (field(0)? == field(1)?).then_some(ProcEvent::Exit {
            pid: field(1)?,
            parent: field(5)?,
            at_ns,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(what: u32, data: &[u32]) -> Vec<u8> {
        let mut msg = vec![0u8; NLMSG_HDR_LEN];
        for word in [CN_IDX_PROC, CN_VAL_PROC, 0, 0, 0] {
            msg.extend_from_slice(&word.to_ne_bytes());
        }
        msg.extend_from_slice(&what.to_ne_bytes());
        msg.extend_from_slice(&3u32.to_ne_bytes());
        msg.extend_from_slice(&5_000u64.to_ne_bytes());
        for word in data {
            msg.extend_from_slice(&word.to_ne_bytes());
        }
        msg
    }

    #[test]
    fn a_process_fork_names_its_parent_process() {
        assert_eq!(
            parse(&message(PROC_EVENT_FORK, &[11, 10, 20, 20])),
            Some(ProcEvent::Fork {
                parent: 10,
                child: 20,
                at_ns: 5_000
            })
        );
    }

    #[test]
    fn an_exit_names_the_process_that_reaps_it() {
        assert_eq!(
            parse(&message(PROC_EVENT_EXIT, &[20, 20, 0, 17, 1, 1])),
            Some(ProcEvent::Exit {
                pid: 20,
                parent: 1,
                at_ns: 5_000
            })
        );
    }

    #[test]
    fn threads_and_other_events_are_not_process_forks_or_exits() {
        assert_eq!(parse(&message(PROC_EVENT_FORK, &[11, 10, 21, 20])), None);
        assert_eq!(
            parse(&message(PROC_EVENT_EXIT, &[21, 20, 0, 17, 1, 1])),
            None
        );
        assert_eq!(parse(&message(0x2, &[20, 20])), None);
        assert_eq!(parse(&message(PROC_EVENT_FORK, &[11])), None);
    }

    #[test]
    fn a_drain_holds_a_child_that_started_and_exited_since_the_last() {
        let Some(mut feed) = EventFeed::subscribe() else {
            assert!(
                kernel_below(6, 6),
                "the process connector refused a listener"
            );
            return;
        };
        feed.drain();
        let status = std::process::Command::new("true").status().unwrap();
        assert!(status.success());
        let events = feed.drain().unwrap();
        let me = i64::from(std::process::id());
        let child = events.iter().find_map(|e| match *e {
            ProcEvent::Fork { parent, child, .. } if parent == me => Some(child),
            _ => None,
        });
        let child = child.unwrap_or_else(|| panic!("no fork from {me} in {events:?}"));
        assert!(events.iter().any(|e| matches!(
            *e,
            ProcEvent::Exit { pid, parent, .. } if pid == child && parent == me
        )));
    }

    /// Whether the running kernel is older than `major.minor`.
    fn kernel_below(major: u32, minor: u32) -> bool {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        let mut parts = release
            .split(['.', '-'])
            .map(|p| p.parse::<u32>().unwrap_or(0));
        let found = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        found < (major, minor)
    }
}
