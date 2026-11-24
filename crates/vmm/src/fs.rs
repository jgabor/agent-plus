//! The virtio-fs device: one share, whose FUSE requests it relays to the
//! share server, a process of its own that reaches the host's files.
//!
//! The device reads no file and parses no more of a request than its unique
//! id, which it needs to fail a request the server can't answer. Each request
//! travels to the server as a frame, and the server's frame is the answer:
//!
//! - to the server: the length of what follows as a little-endian `u32`, the
//!   share's index among the server's shares as a `u32`, and the request's
//!   bytes, as the guest wrote them.
//! - to the device: the length of what follows as a `u32`, and the answer's
//!   bytes, or none for a request that gets no answer.
//!
//! Every byte of a request comes from the guest, and every byte of an
//! answer reaches only the guest's buffers, so neither is trusted here.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::memory::GuestMemory;
use crate::virtio::{Chain, Transport};

const DEVICE_ID: u32 = 26;
const QUEUE_SIZE: u16 = 256;
/// The high-priority queue, then one request queue.
const QUEUES: usize = 2;
const TAG_LEN: usize = 36;
const REQUEST_QUEUES: u32 = 1;

/// The most a request or an answer may hold. Reads are smaller, because the
/// guest asks for at most 32 pages at a time.
pub const MAX_FRAME: usize = 1 << 20;
const IN_HEADER: usize = 40;
const OUT_HEADER: usize = 16;
const EIO: i32 = 5;

/// How long one read or write of the socket waits before the device looks
/// at the clock again.
const POLL: Duration = Duration::from_millis(50);
const SOL_SOCKET: c_int = 1;
const SO_RCVTIMEO: c_int = 20;
const SO_SNDTIMEO: c_int = 21;

unsafe extern "C" {
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
}

/// Where requests go: the share server, or a stand-in for one.
pub trait Server {
    /// Answers the request for share `share`. An empty answer means the
    /// request gets none.
    fn call(&mut self, share: u32, request: &[u8]) -> io::Result<Vec<u8>>;
}

/// The VMM's end of the socket to the share server. It gives up on a
/// request once the guest's time is over, so a server that hangs can't hold
/// the vCPU past its timeout. After any failure it fails every request,
/// because a late answer would be taken for the next one.
pub struct Relay {
    socket: File,
    deadline: Instant,
    broken: bool,
}

impl Relay {
    /// A relay over `socket`, a stream to the share server, which waits for
    /// no answer past `deadline`. Call it before the process is confined.
    pub fn new(socket: OwnedFd, deadline: Instant) -> io::Result<Self> {
        let timeout = [POLL.as_secs() as i64, i64::from(POLL.subsec_micros())];
        for option in [SO_RCVTIMEO, SO_SNDTIMEO] {
            // SAFETY: `timeout` is a `struct timeval`, which outlives the
            // call.
            let set = unsafe {
                setsockopt(
                    socket.as_raw_fd(),
                    SOL_SOCKET,
                    option,
                    timeout.as_ptr().cast(),
                    size_of_val(&timeout) as u32,
                )
            };
            if set != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            socket: File::from(socket),
            deadline,
            broken: false,
        })
    }

    /// The one descriptor a confined VMM must read.
    pub fn descriptor(&self) -> std::os::fd::RawFd {
        self.socket.as_raw_fd()
    }

    /// Waits again after a read or write that timed out, until the deadline.
    fn retry(&self, error: io::Error) -> io::Result<()> {
        match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                if Instant::now() < self.deadline =>
            {
                Ok(())
            }
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                Err(io::ErrorKind::TimedOut.into())
            }
            _ => Err(error),
        }
    }

    fn read_exact(&mut self, mut buffer: &mut [u8]) -> io::Result<()> {
        while !buffer.is_empty() {
            match self.socket.read(buffer) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(len) => buffer = &mut buffer[len..],
                Err(error) => self.retry(error)?,
            }
        }
        Ok(())
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            match self.socket.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(len) => bytes = &bytes[len..],
                Err(error) => self.retry(error)?,
            }
        }
        Ok(())
    }

    fn exchange(&mut self, frame: &[u8]) -> io::Result<Vec<u8>> {
        self.write_all(frame)?;
        let mut header = [0; 4];
        self.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::other("the share server's answer is too long"));
        }
        let mut answer = vec![0; len];
        self.read_exact(&mut answer)?;
        Ok(answer)
    }
}

impl Server for Relay {
    fn call(&mut self, share: u32, request: &[u8]) -> io::Result<Vec<u8>> {
        let mut frame = Vec::with_capacity(8 + request.len());
        frame.extend((request.len() as u32).to_le_bytes());
        frame.extend(share.to_le_bytes());
        frame.extend(request);
        if self.broken {
            return Err(io::Error::other("the share server is gone"));
        }
        let answer = self.exchange(&frame);
        self.broken = answer.is_err();
        answer
    }
}

/// A virtio-fs device for one share.
pub struct Fs {
    transport: Transport,
    share: u32,
}

impl Fs {
    /// The device for the share that its server numbers `share`, which the
    /// guest mounts by `tag`, cut to 36 bytes.
    pub fn new(share: u32, tag: &[u8]) -> Self {
        let mut config = vec![0; TAG_LEN];
        let len = tag.len().min(TAG_LEN);
        config[..len].copy_from_slice(&tag[..len]);
        config.extend(REQUEST_QUEUES.to_le_bytes());
        Self {
            transport: Transport::new(DEVICE_ID, 0, config, QUEUES, QUEUE_SIZE),
            share,
        }
    }

    /// Handles a guest read of the device's registers.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        self.transport.read(offset, data);
    }

    /// Handles a guest write to the device's registers, and relays every
    /// request on the queue it notifies. Returns true if the guest should get
    /// an interrupt.
    pub fn write(
        &mut self,
        memory: &GuestMemory,
        offset: u64,
        data: &[u8],
        server: &mut impl Server,
    ) -> bool {
        let Some(index) = self.transport.write(offset, data) else {
            return false;
        };
        let index = usize::from(index);
        let mut used = false;
        // As in the block device, a guest whose answers publish more
        // requests waits for its next notification.
        for _ in 0..QUEUE_SIZE {
            let Some(queue) = self.transport.queue(index) else {
                break;
            };
            let Ok(Some(chain)) = queue.pop(memory) else {
                break;
            };
            let len = self.serve(memory, &chain, server);
            let Some(queue) = self.transport.queue(index) else {
                break;
            };
            if queue.add_used(memory, chain.head, len).is_err() {
                break;
            }
            used = true;
        }
        if used {
            self.transport.signal_used();
        }
        used
    }

    /// Relays one request, and returns how many bytes of answer it wrote.
    fn serve(&self, memory: &GuestMemory, chain: &Chain, server: &mut impl Server) -> u32 {
        let room = writable_len(chain);
        let Some(request) = readable(memory, chain) else {
            return fail(memory, chain, None, room);
        };
        let answer = match server.call(self.share, &request) {
            Ok(answer) if answer.len() <= room => answer,
            _ => return fail(memory, chain, Some(&request), room),
        };
        match write_at(memory, chain, &answer) {
            true => answer.len() as u32,
            false => 0,
        }
    }
}

/// Answers a request with `EIO`, if it has a unique id to answer and room
/// for the answer.
fn fail(memory: &GuestMemory, chain: &Chain, request: Option<&[u8]>, room: usize) -> u32 {
    let Some(unique) = request.and_then(|request| request.get(8..16)) else {
        return 0;
    };
    if request.is_some_and(|request| request.len() < IN_HEADER) || room < OUT_HEADER {
        return 0;
    }
    let mut answer = [0; OUT_HEADER];
    answer[..4].copy_from_slice(&(OUT_HEADER as u32).to_le_bytes());
    answer[4..8].copy_from_slice(&(-EIO).to_le_bytes());
    answer[8..].copy_from_slice(unique);
    match write_at(memory, chain, &answer) {
        true => OUT_HEADER as u32,
        false => 0,
    }
}

/// The request's bytes, if they fit in a frame and lie in guest memory.
fn readable(memory: &GuestMemory, chain: &Chain) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for buffer in chain.buffers.iter().filter(|buffer| !buffer.writable) {
        let start = bytes.len();
        let end = start.checked_add(buffer.len as usize)?;
        if end > MAX_FRAME {
            return None;
        }
        bytes.resize(end, 0);
        memory.read(buffer.address, &mut bytes[start..]).ok()?;
    }
    Some(bytes)
}

fn writable_len(chain: &Chain) -> usize {
    chain
        .buffers
        .iter()
        .filter(|buffer| buffer.writable)
        .map(|buffer| buffer.len as usize)
        .sum()
}

/// Writes `bytes` across the chain's writable buffers. Returns false if a
/// buffer couldn't be written.
fn write_at(memory: &GuestMemory, chain: &Chain, mut bytes: &[u8]) -> bool {
    for buffer in chain.buffers.iter().filter(|buffer| buffer.writable) {
        if bytes.is_empty() {
            break;
        }
        let take = (buffer.len as usize).min(bytes.len());
        let (now, later) = bytes.split_at(take);
        if memory.write(buffer.address, now).is_err() {
            return false;
        }
        bytes = later;
    }
    bytes.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::thread;

    const SIZE: u16 = 8;
    const QUEUE_SELECT: u64 = 0x030;
    const QUEUE_NUM: u64 = 0x038;
    const QUEUE_READY: u64 = 0x044;
    const QUEUE_NOTIFY: u64 = 0x050;
    const QUEUE_DESC: u64 = 0x080;
    const QUEUE_DRIVER: u64 = 0x090;
    const QUEUE_DEVICE: u64 = 0x0a0;
    const DEVICE_ID_REGISTER: u64 = 0x008;
    const STATUS: u64 = 0x070;
    const CONFIG: u64 = 0x100;
    const DRIVER_OK: u32 = 4;
    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    const TABLE: u64 = 0x1000;
    const AVAILABLE: u64 = 0x2000;
    const USED: u64 = 0x3000;
    const REQUEST_AT: u64 = 0x4000;
    const ANSWER_AT: u64 = 0x8000;
    const SO_SNDBUF: c_int = 7;

    /// Answers each request with its own bytes reversed, and records what
    /// it was sent.
    #[derive(Default)]
    struct Mirror {
        calls: Vec<(u32, Vec<u8>)>,
        broken: bool,
    }

    impl Server for Mirror {
        fn call(&mut self, share: u32, request: &[u8]) -> io::Result<Vec<u8>> {
            self.calls.push((share, request.to_vec()));
            if self.broken {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            Ok(request.iter().rev().copied().collect())
        }
    }

    struct Driver {
        memory: GuestMemory,
        device: Fs,
        published: u16,
    }

    impl Driver {
        fn new() -> Self {
            let mut driver = Self {
                memory: GuestMemory::new(1 << 20).unwrap(),
                device: Fs::new(3, b"share0"),
                published: 0,
            };
            driver.set(QUEUE_SELECT, 1);
            driver.set(QUEUE_NUM, u32::from(SIZE));
            driver.set(QUEUE_DESC, TABLE as u32);
            driver.set(QUEUE_DRIVER, AVAILABLE as u32);
            driver.set(QUEUE_DEVICE, USED as u32);
            driver.set(QUEUE_READY, 1);
            driver.set(STATUS, DRIVER_OK);
            driver
        }

        fn set(&mut self, offset: u64, value: u32) -> bool {
            self.device.write(
                &self.memory,
                offset,
                &value.to_le_bytes(),
                &mut Mirror::default(),
            )
        }

        fn get(&self, offset: u64) -> u32 {
            let mut data = [0; 4];
            self.device.read(offset, &mut data);
            u32::from_le_bytes(data)
        }

        fn descriptor(&self, index: u16, address: u64, len: u32, flags: u16) {
            let at = TABLE + u64::from(index) * 16;
            let next = if flags & NEXT != 0 { index + 1 } else { 0 };
            self.memory.write(at, &address.to_le_bytes()).unwrap();
            self.memory.write(at + 8, &len.to_le_bytes()).unwrap();
            self.memory.write(at + 12, &flags.to_le_bytes()).unwrap();
            self.memory.write(at + 14, &next.to_le_bytes()).unwrap();
        }

        /// Sends `request` in readable buffers of `parts` bytes, with
        /// writable buffers of `room` bytes, and returns the used length.
        fn request(
            &mut self,
            request: &[u8],
            parts: &[usize],
            room: &[u32],
            server: &mut impl Server,
        ) -> u32 {
            self.memory.write(REQUEST_AT, request).unwrap();
            let mut index = 0;
            let mut at = REQUEST_AT;
            for part in parts {
                self.descriptor(index, at, *part as u32, NEXT);
                at += *part as u64;
                index += 1;
            }
            let mut at = ANSWER_AT;
            for (i, len) in room.iter().enumerate() {
                let last = i + 1 == room.len();
                self.descriptor(index, at, *len, if last { WRITE } else { WRITE | NEXT });
                at += u64::from(*len);
                index += 1;
            }
            let slot = u64::from(self.published % SIZE);
            self.memory
                .write(AVAILABLE + 4 + slot * 2, &0u16.to_le_bytes())
                .unwrap();
            self.published += 1;
            self.memory
                .write(AVAILABLE + 2, &self.published.to_le_bytes())
                .unwrap();
            let notify = 1u32.to_le_bytes();
            assert!(
                self.device
                    .write(&self.memory, QUEUE_NOTIFY, &notify, server),
                "the request got no interrupt"
            );
            let mut len = [0; 4];
            self.memory.read(USED + 4 + slot * 8 + 4, &mut len).unwrap();
            u32::from_le_bytes(len)
        }

        fn answer(&self, len: usize) -> Vec<u8> {
            let mut data = vec![0; len];
            self.memory.read(ANSWER_AT, &mut data).unwrap();
            data
        }
    }

    fn request(len: usize) -> Vec<u8> {
        let mut request: Vec<u8> = (0..len).map(|i| i as u8).collect();
        request[8..16].copy_from_slice(&0x1122_3344_5566_7788_u64.to_le_bytes());
        request
    }

    #[test]
    fn the_device_is_virtio_fs_with_its_tag_and_one_request_queue() {
        let driver = Driver::new();
        assert_eq!(driver.get(DEVICE_ID_REGISTER), 26);
        assert_eq!(driver.get(CONFIG), u32::from_le_bytes(*b"shar"));
        assert_eq!(driver.get(CONFIG + 4), u32::from_le_bytes(*b"e0\0\0"));
        assert_eq!(driver.get(CONFIG + 36), 1);
        let long = Fs::new(0, &[b'x'; 64]);
        let mut tag = [0; 40];
        long.read(CONFIG, &mut tag);
        assert_eq!(tag[35], b'x');
        assert_eq!(&tag[36..], &1u32.to_le_bytes());
    }

    #[test]
    fn a_request_reaches_the_server_whole_and_its_answer_reaches_the_guest() {
        let mut driver = Driver::new();
        let mut server = Mirror::default();
        let sent = request(100);
        let len = driver.request(&sent, &[40, 60], &[30, 200], &mut server);
        assert_eq!(server.calls, [(3, sent.clone())]);
        assert_eq!(len, 100);
        let reversed: Vec<u8> = sent.iter().rev().copied().collect();
        assert_eq!(driver.answer(100), reversed);
    }

    #[test]
    fn an_answer_with_no_room_fails_the_request() {
        let mut driver = Driver::new();
        let mut server = Mirror::default();
        let len = driver.request(&request(100), &[100], &[50], &mut server);
        assert_eq!(len, 16);
        let answer = driver.answer(16);
        assert_eq!(answer[..4], 16u32.to_le_bytes());
        assert_eq!(answer[4..8], (-5i32).to_le_bytes());
        assert_eq!(answer[8..], 0x1122_3344_5566_7788_u64.to_le_bytes());
    }

    #[test]
    fn a_server_that_fails_fails_the_request() {
        let mut driver = Driver::new();
        let mut server = Mirror {
            broken: true,
            ..Mirror::default()
        };
        assert_eq!(driver.request(&request(64), &[64], &[16], &mut server), 16);
        assert_eq!(driver.answer(8)[4..8], (-5i32).to_le_bytes());
    }

    #[test]
    fn a_request_too_short_to_answer_gets_nothing() {
        let mut driver = Driver::new();
        let mut server = Mirror {
            broken: true,
            ..Mirror::default()
        };
        assert_eq!(driver.request(&request(20), &[20], &[16], &mut server), 0);
    }

    #[test]
    fn an_empty_answer_writes_nothing() {
        struct Silent;
        impl Server for Silent {
            fn call(&mut self, _: u32, _: &[u8]) -> io::Result<Vec<u8>> {
                Ok(Vec::new())
            }
        }
        let mut driver = Driver::new();
        assert_eq!(driver.request(&request(48), &[48], &[], &mut Silent), 0);
    }

    #[test]
    fn a_request_larger_than_a_frame_never_reaches_the_server() {
        let mut driver = Driver::new();
        let mut server = Mirror::default();
        // The guest's buffers may overlap, so a large request needs little
        // memory.
        let sent = request(64);
        driver.memory.write(REQUEST_AT, &sent).unwrap();
        let parts = vec![MAX_FRAME / 6; 7];
        let mut index = 0;
        for part in &parts {
            driver.descriptor(index, REQUEST_AT, *part as u32, NEXT);
            index += 1;
        }
        driver.descriptor(index, ANSWER_AT, 16, WRITE);
        driver
            .memory
            .write(AVAILABLE + 2, &1u16.to_le_bytes())
            .unwrap();
        let notify = 1u32.to_le_bytes();
        assert!(
            driver
                .device
                .write(&driver.memory, QUEUE_NOTIFY, &notify, &mut server)
        );
        assert!(server.calls.is_empty());
        let mut len = [0; 4];
        driver.memory.read(USED + 8, &mut len).unwrap();
        assert_eq!(u32::from_le_bytes(len), 0);
    }

    #[test]
    fn the_relay_frames_a_request_and_reads_the_answer() {
        let (mine, theirs) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut relay = Relay::new(OwnedFd::from(mine), deadline).unwrap();
        let server = thread::spawn(move || {
            let mut theirs = theirs;
            let mut header = [0; 8];
            theirs.read_exact(&mut header).unwrap();
            let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let share = u32::from_le_bytes(header[4..].try_into().unwrap());
            let mut request = vec![0; len];
            theirs.read_exact(&mut request).unwrap();
            // The answer arrives in two pieces, a poll apart.
            theirs.write_all(&[3, 0]).unwrap();
            thread::sleep(POLL * 2);
            theirs.write_all(&[0, 0, b'o', b'k', b'!']).unwrap();
            (share, request)
        });
        assert_eq!(relay.call(7, b"hello").unwrap(), b"ok!");
        assert_eq!(server.join().unwrap(), (7, b"hello".to_vec()));
    }

    #[test]
    fn the_relay_gives_up_at_the_deadline_and_at_a_hang_up() {
        let (mine, theirs) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + POLL;
        let mut relay = Relay::new(OwnedFd::from(mine), deadline).unwrap();
        let error = relay.call(0, b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(theirs);
        assert!(relay.call(0, b"x").is_err());

        let (mine, mut theirs) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut relay = Relay::new(OwnedFd::from(mine), deadline).unwrap();
        theirs
            .write_all(&(MAX_FRAME as u32 + 1).to_le_bytes())
            .unwrap();
        assert!(relay.call(0, b"x").is_err());
    }

    #[test]
    fn a_server_that_stops_reading_can_t_hold_a_request_past_the_deadline() {
        let (mine, theirs) = UnixStream::pair().unwrap();
        // A small buffer, so the request can't fit in it, and a server that
        // never reads leaves the write waiting.
        let small: c_int = 4096;
        // SAFETY: `small` is an `int`, which outlives the call.
        let set = unsafe {
            setsockopt(
                mine.as_raw_fd(),
                SOL_SOCKET,
                SO_SNDBUF,
                (&raw const small).cast(),
                size_of::<c_int>() as u32,
            )
        };
        assert_eq!(set, 0);
        let deadline = Instant::now() + POLL * 2;
        let mut relay = Relay::new(OwnedFd::from(mine), deadline).unwrap();
        let error = relay.call(0, &vec![0; MAX_FRAME]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(Instant::now() < deadline + Duration::from_secs(5));
        drop(theirs);
    }

    #[test]
    fn after_a_failure_the_relay_fails_every_request() {
        let (mine, mut theirs) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + POLL;
        let mut relay = Relay::new(OwnedFd::from(mine), deadline).unwrap();
        assert!(relay.call(0, b"x").is_err());
        // An answer that comes late is never taken for a later request's.
        theirs.write_all(&[2, 0, 0, 0, b'o', b'k']).unwrap();
        relay.deadline = Instant::now() + Duration::from_secs(30);
        assert!(relay.call(0, b"y").is_err());
    }
}
