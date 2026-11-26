//! The lookups that `ap run` makes for the network process, which can't
//! read your host's resolver configuration once it is confined. `ap run`
//! looks each name up through the host's own resolver at the time it is
//! asked, so a run follows the host when it changes networks.
//!
//! Each question is one message: `?`, an id of four bytes, then the name.
//! Each answer is one message: the question's id, then each address, as a
//! byte for its family, 4 or 6, and its bytes. A name that doesn't resolve
//! gets no addresses. The same socket carries the network process's
//! reports of what it blocked, which [`super::blocked`] reads.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::blocked::{Reports, TAG};
use super::decisions::Keeper;

/// The longest name a question holds.
const LONGEST_NAME: usize = 253;
/// The threads that answer at once.
const WORKERS: usize = 4;
/// The questions that wait for a thread to answer them, far more than the
/// network process has waiting at once.
const WAITING: usize = 1024;
/// The most addresses an answer holds.
const MOST_ADDRESSES: usize = 16;
/// The longest answer.
pub(super) const LONGEST: usize = 4 + MOST_ADDRESSES * 17;
/// The longest message the network process sends: a question, or a
/// report.
const LONGEST_SENT: usize = 512;
/// The first byte of a question.
const QUESTION: u8 = b'?';

/// Answers the network process's questions on `socket`, the
/// [`super::Started::lookups`] of its start, until the network process
/// ends, and keeps its reports: its decisions in `keeper`, if there is one,
/// and the hosts it blocked. It answers none unless `answers`, for a
/// network process whose rules can reach nothing, which never asks. One
/// thread reads, so reports never wait for a lookup. A few threads answer
/// at once, each one question at a time, so a slow name doesn't hold up the
/// others, and the network process can't make `ap run` start more. The rest
/// wait their turn, up to [`WAITING`] of them. A question past those goes
/// unanswered.
pub fn answer_lookups(socket: OwnedFd, answers: bool, mut keeper: Option<Keeper>) -> Lookups {
    let socket = Arc::new(UnixDatagram::from(socket));
    let reports = Arc::new(Mutex::new(Reports::default()));
    let (running, ended) = mpsc::channel::<()>();
    let (questions, asked) = mpsc::sync_channel::<(u32, String)>(WAITING);
    let asked = Arc::new(Mutex::new(asked));
    for _ in 0..WORKERS {
        let socket = Arc::clone(&socket);
        let asked = Arc::clone(&asked);
        let started = thread::Builder::new().spawn(move || {
            loop {
                let next = asked.lock().map(|asked| asked.recv());
                let Ok(Ok((id, name))) = next else { break };
                let _ = socket.send(&write_answer(id, &look_up(&name)));
            }
        });
        // With no thread to answer, a connection to a name fails, as it
        // would with no resolver.
        drop(started);
    }
    let read = Arc::clone(&reports);
    let started = thread::Builder::new().spawn(move || {
        let mut buffer = [0; LONGEST_SENT + 1];
        loop {
            let len = match socket.recv(&mut buffer) {
                Ok(0) => break,
                Ok(len) => len,
                // The network process ended with an answer unread. The
                // reports it sent before it are still to read.
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(_) => break,
            };
            let message = &buffer[..len];
            if message.first() == Some(&TAG) {
                let decision = keeper.as_mut().is_some_and(|keeper| keeper.take(message));
                if decision {
                    continue;
                }
                if let Ok(mut reports) = read.lock() {
                    reports.take(message);
                }
            } else if let (true, Some(question)) = (answers, read_question(message)) {
                let _ = questions.try_send(question);
            }
        }
        drop(running);
    });
    drop(started);
    Lookups { reports, ended }
}

/// The network process's lookups, which [`answer_lookups`] answers.
pub struct Lookups {
    reports: Arc<Mutex<Reports>>,
    /// Disconnects once the thread that reads has ended.
    ended: mpsc::Receiver<()>,
}

impl Lookups {
    /// What the network process reported, once it has ended, or `wait`
    /// has passed.
    pub fn reports(self, wait: Duration) -> Reports {
        // Nothing is ever sent, so this returns once the thread that reads
        // drops its sender, or the wait ends.
        let _ = self.ended.recv_timeout(wait);
        std::mem::take(
            &mut *self
                .reports
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }
}

/// The addresses your host's resolver has for `name`, in its order, each
/// once.
fn look_up(name: &str) -> Vec<IpAddr> {
    let mut found: Vec<IpAddr> = Vec::new();
    for address in (name, 0).to_socket_addrs().into_iter().flatten() {
        if !found.contains(&address.ip()) && found.len() < MOST_ADDRESSES {
            found.push(address.ip());
        }
    }
    found
}

pub(super) fn write_question(id: u32, name: &str) -> Option<Vec<u8>> {
    (name.len() <= LONGEST_NAME)
        .then(|| [&[QUESTION][..], &id.to_be_bytes(), name.as_bytes()].concat())
}

fn read_question(message: &[u8]) -> Option<(u32, String)> {
    let (id, name) = message.strip_prefix(&[QUESTION])?.split_at_checked(4)?;
    let name = std::str::from_utf8(name).ok()?;
    (name.len() <= LONGEST_NAME && !name.is_empty())
        .then(|| (u32::from_be_bytes(id.try_into().unwrap()), name.to_owned()))
}

fn write_answer(id: u32, addresses: &[IpAddr]) -> Vec<u8> {
    let mut message = id.to_be_bytes().to_vec();
    for address in addresses.iter().take(MOST_ADDRESSES) {
        match address {
            IpAddr::V4(v4) => {
                message.push(4);
                message.extend(v4.octets());
            }
            IpAddr::V6(v6) => {
                message.push(6);
                message.extend(v6.octets());
            }
        }
    }
    message
}

pub(super) fn read_answer(message: &[u8]) -> Option<(u32, Vec<IpAddr>)> {
    let (id, mut rest) = message.split_at_checked(4)?;
    let mut addresses = Vec::new();
    while let Some((&family, after)) = rest.split_first() {
        let (address, after) = match family {
            4 => {
                let (bytes, after) = after.split_at_checked(4)?;
                let bytes: [u8; 4] = bytes.try_into().ok()?;
                (IpAddr::V4(Ipv4Addr::from(bytes)), after)
            }
            6 => {
                let (bytes, after) = after.split_at_checked(16)?;
                let bytes: [u8; 16] = bytes.try_into().ok()?;
                (IpAddr::V6(Ipv6Addr::from(bytes)), after)
            }
            _ => return None,
        };
        addresses.push(address);
        rest = after;
    }
    Some((u32::from_be_bytes(id.try_into().unwrap()), addresses))
}

/// The network process's side: it asks, and takes answers as they come.
pub(super) struct Asker {
    socket: UnixDatagram,
    next: u32,
}

impl Asker {
    pub(super) fn new(socket: UnixDatagram) -> Asker {
        Asker { socket, next: 0 }
    }

    pub(super) fn fd(&self) -> i32 {
        self.socket.as_raw_fd()
    }

    /// Asks for `name`, and returns the id its answer will carry.
    pub(super) fn ask(&mut self, name: &str) -> io::Result<u32> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let question = write_question(id, name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
        super::proxy::send(self.socket.as_raw_fd(), &question)?;
        Ok(id)
    }

    /// The next answer, if one has come. Once `ap run` has ended, it is an
    /// error of the kind [`io::ErrorKind::UnexpectedEof`].
    pub(super) fn answer(&mut self) -> io::Result<Option<(u32, Vec<IpAddr>)>> {
        let mut buffer = [0; LONGEST];
        match super::proxy::recv(self.socket.as_raw_fd(), &mut buffer)? {
            None => Ok(None),
            Some(0) => Err(io::ErrorKind::UnexpectedEof.into()),
            Some(len) => Ok(read_answer(&buffer[..len])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_and_an_answer_read_back() {
        let question = write_question(7, "api.anthropic.com").unwrap();
        assert_eq!(
            read_question(&question),
            Some((7, "api.anthropic.com".to_owned()))
        );
        assert_eq!(write_question(1, &"a".repeat(254)), None);
        assert_eq!(read_question(b"?\0\0\0\x01"), None);
        assert_eq!(read_question(b"\0\0\0\x01example.com"), None);
        let addresses: Vec<IpAddr> =
            vec!["192.0.2.1".parse().unwrap(), "2001:db8::1".parse().unwrap()];
        assert_eq!(
            read_answer(&write_answer(9, &addresses)),
            Some((9, addresses))
        );
        assert_eq!(read_answer(&write_answer(3, &[])), Some((3, vec![])));
        assert_eq!(read_answer(&[0, 0, 0, 1, 4, 1, 2]), None);
        assert_eq!(read_answer(&[0, 0, 0, 1, 5]), None);
    }

    /// Asks `localhost` on a link to [`answer_lookups`], reports a host,
    /// and returns whether an answer came, and the reports.
    fn ask_and_report(answers: bool) -> (bool, Reports) {
        let (mine, theirs) = super::super::link().unwrap();
        let lookups = answer_lookups(theirs, answers, None);
        let mine = UnixDatagram::from(mine);
        mine.send(&write_question(1, "localhost").unwrap()).unwrap();
        mine.send(b"!lookup example.com").unwrap();
        mine.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buffer = [0; LONGEST];
        let answered = mine.recv(&mut buffer).is_ok();
        drop(mine);
        (answered, lookups.reports(Duration::from_secs(5)))
    }

    #[test]
    fn a_network_that_reaches_nothing_gets_no_lookups() {
        let (answered, reports) = ask_and_report(true);
        assert!(answered);
        assert_eq!(reports.blocked.len(), 1, "{reports:?}");
        // A network process that can reach nothing never asks, so a
        // question from it can't carry anything out through your resolver.
        let (answered, reports) = ask_and_report(false);
        assert!(!answered);
        assert_eq!(reports.blocked.len(), 1, "{reports:?}");
    }

    #[test]
    fn a_decision_the_network_process_reports_reaches_the_run_s_log() {
        let args: Vec<std::ffi::OsString> =
            ["--", "-", "+host.ap.internal"].map(Into::into).to_vec();
        let (network, harness) = super::super::rules(&args).unwrap();
        let (mine, theirs) = super::super::link().unwrap();
        let reporter = super::super::blocked::Reporter::new(mine.as_raw_fd(), network, harness);
        let path = std::env::temp_dir().join(format!("ap-lookup-log-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = std::fs::File::options()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let keeper = Keeper::new(&args, log.try_clone().unwrap());
        let lookups = answer_lookups(theirs, true, Some(keeper));
        let loopback = "127.0.0.1".parse().unwrap();
        reporter.decided(true, Some("host.ap.internal"), loopback, 8080);
        reporter.report(net::Blocked::Lookup("denied.example".to_owned()));
        // A lookup of the name's other records is the same decision.
        reporter.report(net::Blocked::Lookup("denied.example".to_owned()));
        // Once another decision comes between, it is a new one.
        reporter.decided(true, Some("host.ap.internal"), loopback, 8080);
        reporter.report(net::Blocked::Lookup("denied.example".to_owned()));
        drop((reporter, mine));
        let reports = lookups.reports(Duration::from_secs(5));
        // The host it blocked is still reported.
        assert_eq!(reports.blocked.len(), 1, "{reports:?}");
        let mut log = log;
        std::io::Seek::rewind(&mut log).unwrap();
        let mut log = std::io::BufReader::new(log);
        let mut lines = Vec::new();
        while let Some(part) = crate::runs::read_log(&mut log).unwrap() {
            match part {
                crate::runs::Logged::Decision(_, line) => lines.push(line),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            lines,
            [
                "allow  host.ap.internal:8080  NETWORK +host.ap.internal",
                "deny   denied.example  the default",
                "allow  host.ap.internal:8080  NETWORK +host.ap.internal",
                "deny   denied.example  the default",
            ]
        );
    }

    #[test]
    fn a_lookup_goes_through_the_host_s_resolver() {
        let found = look_up("localhost");
        assert!(
            found.iter().all(|address| address.is_loopback()) && !found.is_empty(),
            "{found:?}"
        );
        assert_eq!(
            look_up("name.that.does.not.exist.invalid"),
            Vec::<IpAddr>::new()
        );
    }
}
