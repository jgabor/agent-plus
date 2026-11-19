//! The virtio-net device: one Ethernet link, whose other end is a host
//! socket that keeps message boundaries, such as a datagram or seqpacket
//! Unix socket. Each message is one frame, with no virtio header. Frames go
//! to that socket and nowhere else.
//!
//! The guest's frames are sent on the vCPU thread, during its exits. Frames
//! for the guest are read by a [`Receiver`], on a thread of its own, which
//! wakes the vCPU so that [`Net::service`] can hand them over.
//!
//! The link is plain Ethernet: no checksum offload, no segmentation, and no
//! merged buffers, so a frame is at most [`MAX_FRAME`] bytes either way. A
//! frame that can't cross is dropped, as on a real link.

use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::memory::GuestMemory;
use crate::virtio::{Chain, Transport};

const DEVICE_ID: u32 = 1;
const MAC_FEATURE: u64 = 1 << 5;
const QUEUE_SIZE: u16 = 256;
const RECEIVE: usize = 0;
const TRANSMIT: usize = 1;
const QUEUES: usize = 2;

/// `struct virtio_net_hdr_v1`, which starts every buffer either way.
const HEADER: usize = 12;
/// The largest Ethernet frame without a VLAN tag or a checksum: a 14-byte
/// header and a 1500-byte payload.
pub const MAX_FRAME: usize = 1514;
/// How many frames may wait for the guest to post buffers. Past this, new
/// frames are dropped, so a guest that posts none can't fill host memory.
const INBOX: usize = 256;

/// How long the receiver waits for a frame before it looks at whether the
/// device is gone.
const RECEIVE_WAIT: Duration = Duration::from_millis(50);
/// How long the vCPU waits for room in the socket before it drops a frame.
const SEND_WAIT: Duration = Duration::from_millis(10);
const SOL_SOCKET: c_int = 1;
const SO_RCVTIMEO: c_int = 20;
const SO_SNDTIMEO: c_int = 21;

unsafe extern "C" {
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
}

/// What the device and its receiver share.
struct Link {
    socket: File,
    inbox: Mutex<VecDeque<Vec<u8>>>,
    /// The device is gone, so the receiver stops.
    gone: AtomicBool,
}

impl Link {
    fn inbox(&self) -> MutexGuard<'_, VecDeque<Vec<u8>>> {
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A virtio-net device with the MAC address it was given.
pub struct Net {
    transport: Transport,
    link: Arc<Link>,
}

impl Net {
    /// A device whose frames go to `socket`. Call it before the process is
    /// confined, because it sets how long the socket's calls wait.
    pub fn new(socket: OwnedFd, mac: [u8; 6]) -> io::Result<Self> {
        set_timeout(&socket, SO_RCVTIMEO, RECEIVE_WAIT)?;
        set_timeout(&socket, SO_SNDTIMEO, SEND_WAIT)?;
        Ok(Self {
            transport: Transport::new(DEVICE_ID, MAC_FEATURE, mac.to_vec(), QUEUES, QUEUE_SIZE),
            link: Arc::new(Link {
                socket: File::from(socket),
                inbox: Mutex::new(VecDeque::new()),
                gone: AtomicBool::new(false),
            }),
        })
    }

    /// The one descriptor a confined VMM must read.
    pub fn descriptor(&self) -> RawFd {
        self.link.socket.as_raw_fd()
    }

    /// What reads the frames for the guest, on a thread of its own.
    pub fn receiver(&self) -> Receiver {
        Receiver(Arc::clone(&self.link))
    }

    /// Handles a guest read of the device's registers.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        self.transport.read(offset, data);
    }

    /// Handles a guest write to the device's registers: it sends the frames
    /// the guest queued, or takes the buffers it posted. Returns true if the
    /// guest should get an interrupt.
    pub fn write(&mut self, memory: &GuestMemory, offset: u64, data: &[u8]) -> bool {
        match self.transport.write(offset, data).map(usize::from) {
            Some(TRANSMIT) => self.transmit(memory),
            Some(RECEIVE) => self.service(memory),
            _ => false,
        }
    }

    /// Hands the frames that wait for the guest to the buffers it posted.
    /// Returns true if the guest should get an interrupt.
    pub fn service(&mut self, memory: &GuestMemory) -> bool {
        let link = Arc::clone(&self.link);
        let mut inbox = link.inbox();
        let mut used = false;
        while !inbox.is_empty() {
            let Some(queue) = self.transport.queue(RECEIVE) else {
                break;
            };
            // A broken ring has already stopped the queue.
            let Ok(Some(chain)) = queue.pop(memory) else {
                break;
            };
            let frame = inbox.pop_front().unwrap_or_default();
            let len = deliver(memory, &chain, &frame);
            if queue.add_used(memory, chain.head, len).is_err() {
                break;
            }
            used = true;
        }
        drop(inbox);
        if used {
            self.transport.signal_used();
        }
        used
    }

    /// Sends every frame the guest queued, as far as one ring's worth.
    fn transmit(&mut self, memory: &GuestMemory) -> bool {
        let mut used = false;
        // As in the block device, a guest that queues more as it goes waits
        // for its next notification, so the vCPU always gets back to it.
        for _ in 0..QUEUE_SIZE {
            let Some(queue) = self.transport.queue(TRANSMIT) else {
                break;
            };
            let Ok(Some(chain)) = queue.pop(memory) else {
                break;
            };
            if let Some(frame) = frame(memory, &chain) {
                // A frame the socket has no room for is dropped, as a full
                // link drops it.
                let _ = (&self.link.socket).write(&frame);
            }
            if queue.add_used(memory, chain.head, 0).is_err() {
                break;
            }
            used = true;
        }
        if used {
            self.transport.signal_used();
        }
        used
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.link.gone.store(true, Ordering::SeqCst);
    }
}

/// Reads frames for the guest from the device's socket.
pub struct Receiver(Arc<Link>);

impl Receiver {
    /// Reads frames until the device is gone or the socket's other end hangs
    /// up, and calls `wake` after each one, so that [`Net::service`] runs. An
    /// empty message hangs up too. It holds the socket open until it returns.
    pub fn run(self, wake: impl Fn()) {
        let link = self.0;
        // One byte more than a frame, so that a longer message is seen.
        let mut buffer = vec![0; MAX_FRAME + 1];
        while !link.gone.load(Ordering::SeqCst) {
            match (&link.socket).read(&mut buffer) {
                Ok(0) => return,
                Ok(len) if len > MAX_FRAME => {}
                Ok(len) => {
                    let mut inbox = link.inbox();
                    if inbox.len() < INBOX {
                        inbox.push_back(buffer[..len].to_vec());
                    }
                    drop(inbox);
                    wake();
                }
                Err(error) if waited(&error) => {}
                Err(_) => return,
            }
        }
    }
}

/// Whether a socket call only ran out of time, or was interrupted.
fn waited(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn set_timeout(socket: &OwnedFd, option: c_int, wait: Duration) -> io::Result<()> {
    let timeout = [wait.as_secs() as i64, i64::from(wait.subsec_micros())];
    // SAFETY: `timeout` is a `struct timeval`, which outlives the call.
    let set = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            option,
            timeout.as_ptr().cast(),
            size_of_val(&timeout) as u32,
        )
    };
    match set {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// The frame in a chain the guest queued, after its header, if it is one
/// the link can carry and it lies in guest memory.
fn frame(memory: &GuestMemory, chain: &Chain) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for buffer in chain.buffers.iter().filter(|buffer| !buffer.writable) {
        let start = bytes.len();
        let end = start.checked_add(buffer.len as usize)?;
        if end > HEADER + MAX_FRAME {
            return None;
        }
        bytes.resize(end, 0);
        memory.read(buffer.address, &mut bytes[start..]).ok()?;
    }
    let frame = bytes.get(HEADER..).filter(|frame| !frame.is_empty())?;
    Some(frame.to_vec())
}

/// Writes a header and `frame` into a chain the guest posted, and returns
/// the used length. A chain with too little room gets nothing, and the
/// frame is dropped.
fn deliver(memory: &GuestMemory, chain: &Chain, frame: &[u8]) -> u32 {
    let mut bytes = [0; HEADER + MAX_FRAME];
    // `num_buffers`, which is always 1 without merged buffers.
    bytes[10..HEADER].copy_from_slice(&1_u16.to_le_bytes());
    bytes[HEADER..HEADER + frame.len()].copy_from_slice(frame);
    let mut rest = &bytes[..HEADER + frame.len()];
    let room: usize = chain
        .buffers
        .iter()
        .filter(|buffer| buffer.writable)
        .map(|buffer| buffer.len as usize)
        .sum();
    if room < rest.len() {
        return 0;
    }
    let len = rest.len() as u32;
    for buffer in chain.buffers.iter().filter(|buffer| buffer.writable) {
        let take = (buffer.len as usize).min(rest.len());
        let (now, later) = rest.split_at(take);
        if memory.write(buffer.address, now).is_err() {
            return 0;
        }
        rest = later;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Instant;

    const SIZE: u16 = 32;
    /// How many chains, of up to four buffers each, the tests lay out at once.
    const CHAINS: u16 = 8;
    const DEVICE_ID_REGISTER: u64 = 0x008;
    const DEVICE_FEATURES: u64 = 0x010;
    const QUEUE_SELECT: u64 = 0x030;
    const QUEUE_NUM: u64 = 0x038;
    const QUEUE_READY: u64 = 0x044;
    const QUEUE_NOTIFY: u64 = 0x050;
    const QUEUE_DESC: u64 = 0x080;
    const QUEUE_DRIVER: u64 = 0x090;
    const QUEUE_DEVICE: u64 = 0x0a0;
    const STATUS: u64 = 0x070;
    const CONFIG: u64 = 0x100;
    const DRIVER_OK: u32 = 4;
    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 0x0f];
    /// Each queue's rings, and where its buffers go.
    const RINGS: [u64; 2] = [0x1_0000, 0x2_0000];
    const BUFFERS: [u64; 2] = [0x4_0000, 0x8_0000];

    struct Driver {
        memory: GuestMemory,
        device: Net,
        peer: UnixDatagram,
        published: [u16; 2],
    }

    impl Driver {
        fn new() -> Self {
            let (mine, peer) = UnixDatagram::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut driver = Self {
                memory: GuestMemory::new(1 << 20).unwrap(),
                device: Net::new(OwnedFd::from(mine), MAC).unwrap(),
                peer,
                published: [0; 2],
            };
            for (queue, rings) in RINGS.iter().enumerate() {
                driver.set(QUEUE_SELECT, queue as u32);
                driver.set(QUEUE_NUM, u32::from(SIZE));
                driver.set(QUEUE_DESC, *rings as u32);
                driver.set(QUEUE_DRIVER, (*rings + 0x1000) as u32);
                driver.set(QUEUE_DEVICE, (*rings + 0x2000) as u32);
                driver.set(QUEUE_READY, 1);
            }
            driver.set(STATUS, DRIVER_OK);
            driver
        }

        fn set(&mut self, offset: u64, value: u32) -> bool {
            self.device
                .write(&self.memory, offset, &value.to_le_bytes())
        }

        fn get(&self, offset: u64) -> u32 {
            let mut data = [0; 4];
            self.device.read(offset, &mut data);
            u32::from_le_bytes(data)
        }

        /// Makes a chain of buffers of these lengths available on `queue`,
        /// writable or not, laid out one after another, and notifies when
        /// asked. Returns whether the guest got an interrupt.
        fn publish(&mut self, queue: usize, lens: &[u32], writable: bool, notify: bool) -> bool {
            let table = RINGS[queue];
            let slot = self.published[queue] % SIZE;
            // Each chain gets its own descriptors and its own buffer space.
            let first = slot % CHAINS * 4;
            let mut address = BUFFERS[queue] + u64::from(slot % CHAINS) * 0x4000;
            for (i, len) in lens.iter().enumerate() {
                let index = first + i as u16;
                let mut flags = if writable { WRITE } else { 0 };
                if i + 1 < lens.len() {
                    flags |= NEXT;
                }
                let at = table + u64::from(index) * 16;
                self.memory.write(at, &address.to_le_bytes()).unwrap();
                self.memory.write(at + 8, &len.to_le_bytes()).unwrap();
                self.memory.write(at + 12, &flags.to_le_bytes()).unwrap();
                self.memory
                    .write(at + 14, &(index + 1).to_le_bytes())
                    .unwrap();
                address += u64::from(*len);
            }
            let available = table + 0x1000;
            self.memory
                .write(available + 4 + u64::from(slot) * 2, &first.to_le_bytes())
                .unwrap();
            self.published[queue] += 1;
            self.memory
                .write(available + 2, &self.published[queue].to_le_bytes())
                .unwrap();
            notify && self.set(QUEUE_NOTIFY, queue as u32)
        }

        /// The used length of the chain in `slot` of `queue`'s used ring.
        fn used_len(&self, queue: usize, slot: u16) -> u32 {
            let used = RINGS[queue] + 0x2000;
            let mut len = [0; 4];
            self.memory
                .read(used + 4 + u64::from(slot) * 8 + 4, &mut len)
                .unwrap();
            u32::from_le_bytes(len)
        }

        fn used_count(&self, queue: usize) -> u16 {
            let mut index = [0; 2];
            self.memory
                .read(RINGS[queue] + 0x2000 + 2, &mut index)
                .unwrap();
            u16::from_le_bytes(index)
        }

        /// Sends a frame the guest wrote in buffers of these lengths.
        fn send(&mut self, frame: &[u8], lens: &[u32]) -> bool {
            let mut bytes = vec![0xee; HEADER];
            bytes.extend_from_slice(frame);
            let slot = self.published[TRANSMIT] % SIZE;
            let address = BUFFERS[TRANSMIT] + u64::from(slot % CHAINS) * 0x4000;
            self.memory.write(address, &bytes).unwrap();
            self.publish(TRANSMIT, lens, false, true)
        }

        /// What the guest finds in the receive buffers of `slot`.
        fn received(&self, slot: u16, len: usize) -> Vec<u8> {
            let address = BUFFERS[RECEIVE] + u64::from(slot % CHAINS) * 0x4000;
            let mut bytes = vec![0; len];
            self.memory.read(address, &mut bytes).unwrap();
            bytes
        }

        /// Runs the receiver until it has read `count` frames.
        fn receive(&self, count: usize) {
            let receiver = self.device.receiver();
            let woken = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&woken);
            let link = Arc::clone(&self.device.link);
            let thread = thread::spawn(move || {
                receiver.run(move || {
                    if counted.fetch_add(1, Ordering::SeqCst) + 1 == count {
                        link.gone.store(true, Ordering::SeqCst);
                    }
                })
            });
            thread.join().unwrap();
            self.device.link.gone.store(false, Ordering::SeqCst);
            assert_eq!(woken.load(Ordering::SeqCst), count);
        }
    }

    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    #[test]
    fn the_device_is_virtio_net_with_its_mac_address() {
        let driver = Driver::new();
        assert_eq!(driver.get(DEVICE_ID_REGISTER), 1);
        assert_eq!(driver.get(DEVICE_FEATURES), 1 << 5);
        let mut mac = [0; 6];
        driver.device.read(CONFIG, &mut mac);
        assert_eq!(mac, MAC);
    }

    #[test]
    fn a_frame_the_guest_sends_reaches_the_socket_without_its_header() {
        let mut driver = Driver::new();
        let sent = frame(100);
        assert!(driver.send(&sent, &[HEADER as u32, 40, 60]));
        let mut got = [0; 2048];
        let len = driver.peer.recv(&mut got).unwrap();
        assert_eq!(&got[..len], sent);
        assert_eq!(driver.used_count(TRANSMIT), 1);
    }

    #[test]
    fn the_header_and_frame_may_share_a_buffer() {
        let mut driver = Driver::new();
        let sent = frame(MAX_FRAME);
        assert!(driver.send(&sent, &[(HEADER + MAX_FRAME) as u32]));
        let mut got = [0; 2048];
        let len = driver.peer.recv(&mut got).unwrap();
        assert_eq!(&got[..len], sent);
    }

    #[test]
    fn a_frame_the_link_cant_carry_never_reaches_the_socket() {
        let mut driver = Driver::new();
        let long = frame(MAX_FRAME + 1);
        assert!(driver.send(&long, &[(HEADER + MAX_FRAME + 1) as u32]));
        assert!(driver.send(&[], &[HEADER as u32]));
        assert!(driver.send(&frame(20), &[HEADER as u32, 20]));
        let mut got = [0; 2048];
        assert_eq!(driver.peer.recv(&mut got).unwrap(), 20);
        assert_eq!(driver.used_count(TRANSMIT), 3);
    }

    #[test]
    fn a_frame_from_the_socket_reaches_a_posted_buffer_after_a_header() {
        let mut driver = Driver::new();
        assert!(!driver.publish(RECEIVE, &[HEADER as u32, 1514], true, true));
        let sent = frame(60);
        driver.peer.send(&sent).unwrap();
        driver.receive(1);
        assert!(driver.device.service(&driver.memory));
        assert_eq!(driver.used_len(RECEIVE, 0), (HEADER + 60) as u32);
        let mut expected = vec![0; HEADER];
        expected[10] = 1;
        expected.extend_from_slice(&sent);
        assert_eq!(driver.received(0, HEADER + 60), expected);
    }

    #[test]
    fn frames_wait_for_the_guest_to_post_buffers() {
        let mut driver = Driver::new();
        driver.peer.send(&frame(30)).unwrap();
        driver.peer.send(&frame(40)).unwrap();
        driver.receive(2);
        assert!(!driver.device.service(&driver.memory));
        // Posting a buffer is a notification, which hands over one frame.
        assert!(driver.publish(RECEIVE, &[2048], true, true));
        assert_eq!(driver.used_len(RECEIVE, 0), (HEADER + 30) as u32);
        assert!(driver.publish(RECEIVE, &[2048], true, true));
        assert_eq!(driver.used_len(RECEIVE, 1), (HEADER + 40) as u32);
        assert!(!driver.device.service(&driver.memory));
    }

    #[test]
    fn a_buffer_too_small_for_the_frame_gets_nothing() {
        let mut driver = Driver::new();
        driver.publish(RECEIVE, &[HEADER as u32 + 10], true, false);
        driver.peer.send(&frame(11)).unwrap();
        driver.receive(1);
        assert!(driver.device.service(&driver.memory));
        assert_eq!(driver.used_len(RECEIVE, 0), 0);
    }

    #[test]
    fn a_message_too_long_for_a_frame_is_dropped() {
        let mut driver = Driver::new();
        driver.publish(RECEIVE, &[4096], true, false);
        driver.peer.send(&frame(MAX_FRAME + 1)).unwrap();
        driver.peer.send(&frame(MAX_FRAME)).unwrap();
        driver.receive(1);
        assert!(driver.device.service(&driver.memory));
        assert_eq!(driver.used_len(RECEIVE, 0), (HEADER + MAX_FRAME) as u32);
    }

    #[test]
    fn frames_past_the_inbox_are_dropped() {
        let driver = Driver::new();
        // A socket holds few messages, so they are sent as they are read.
        let peer = driver.peer.try_clone().unwrap();
        let sender = thread::spawn(move || {
            for _ in 0..INBOX + 3 {
                peer.send(&frame(20)).unwrap();
            }
        });
        driver.receive(INBOX + 3);
        sender.join().unwrap();
        assert_eq!(driver.device.link.inbox().len(), INBOX);
    }

    #[test]
    fn the_receiver_stops_when_the_device_is_gone_or_the_peer_hangs_up() {
        let driver = Driver::new();
        let receiver = driver.device.receiver();
        let thread = thread::spawn(|| receiver.run(|| {}));
        let started = Instant::now();
        drop(driver.device);
        thread.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));

        let driver = Driver::new();
        let receiver = driver.device.receiver();
        let thread = thread::spawn(|| receiver.run(|| {}));
        driver.peer.send(&[]).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn nothing_moves_before_the_driver_is_ready() {
        let (mine, peer) = UnixDatagram::pair().unwrap();
        let mut device = Net::new(OwnedFd::from(mine), MAC).unwrap();
        let memory = GuestMemory::new(1 << 20).unwrap();
        peer.send(&frame(20)).unwrap();
        device.link.inbox().push_back(frame(20));
        assert!(!device.service(&memory));
        assert!(!device.write(&memory, QUEUE_NOTIFY, &1_u32.to_le_bytes()));
        assert_eq!(device.link.inbox().len(), 1);
    }
}
