//! The virtio-vsock device: stream sockets between the guest and this process.
//!
//! The guest connects, and the host only listens. Everything in a packet comes
//! from the guest, so nothing it says is trusted, and it can't make the host
//! hold more than a fixed amount per connection.

use crate::memory::{GuestMemory, OutOfBounds};
use crate::virtio::{Chain, Transport};
use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound::{Excluded, Unbounded};

const DEVICE_ID: u32 = 19;
const QUEUE_SIZE: u16 = 256;
const RX: usize = 0;
const TX: usize = 1;
/// The driver expects a third queue, for events the device never sends.
const QUEUES: usize = 3;
const STATUS: u64 = 0x070;

const HOST_CID: u64 = 2;
const HEADER: usize = 44;
const TYPE_STREAM: u16 = 1;

const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

const SHUTDOWN_RECEIVE: u32 = 1;
const SHUTDOWN_SEND: u32 = 2;

/// The largest payload in one packet, which is also Linux's limit.
const MAX_PAYLOAD: usize = 64 << 10;
/// How much a guest may have unread on the host, per connection.
const BUFFER: u32 = 256 << 10;
/// How much the host may queue for the guest, per connection.
const MAX_OUTBOUND: usize = 1 << 20;
const MAX_CONNECTIONS: usize = 64;
/// How many replies with no connection may wait for a receive buffer. At this
/// limit the device stops reading the guest's packets, so that a guest that
/// never posts a buffer only stalls itself.
const MAX_CONTROL: usize = 256;

/// One open connection, as the host names it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Connection {
    guest_port: u32,
    host_port: u32,
    /// Which device reset this belongs after, so that a handle kept across a
    /// reset can't reach a new connection on the same ports.
    generation: u64,
}

impl Connection {
    /// The host port the guest connected to.
    pub fn port(&self) -> u32 {
        self.host_port
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Header {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    kind: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Header {
    fn parse(raw: &[u8; HEADER]) -> Self {
        let u16_at = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        let u32_at =
            |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let u64_at = |at: usize| u64::from_le_bytes(std::array::from_fn(|i| raw[at + i]));
        Self {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            kind: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }

    fn encode(&self) -> [u8; HEADER] {
        let mut raw = [0; HEADER];
        raw[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        raw[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        raw[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        raw[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        raw[24..28].copy_from_slice(&self.len.to_le_bytes());
        raw[28..30].copy_from_slice(&self.kind.to_le_bytes());
        raw[30..32].copy_from_slice(&self.op.to_le_bytes());
        raw[32..36].copy_from_slice(&self.flags.to_le_bytes());
        raw[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        raw[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        raw
    }
}

#[derive(Default)]
struct State {
    accepted: bool,
    owes_response: bool,
    owes_credit: bool,
    /// The host asked for credit, and the guest hasn't given any since.
    asked_for_credit: bool,
    inbound: VecDeque<u8>,
    outbound: VecDeque<u8>,
    /// What the guest last said it can hold, and has consumed.
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes sent to the guest, and bytes the host has consumed. Both wrap.
    tx_cnt: u32,
    fwd_cnt: u32,
    advertised_fwd_cnt: u32,
    guest_stopped_sending: bool,
    guest_stopped_receiving: bool,
    /// The connection is over, and only waits for the host to let go.
    dead: bool,
    host_closed: bool,
    shutdown_sent: bool,
}

impl State {
    /// How many more bytes the guest said it can take. A guest that reports
    /// nonsense gets no credit, and only stalls its own connection.
    fn credit(&self) -> usize {
        self.peer_buf_alloc
            .saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt)) as usize
    }

    /// Data waits while the host has asked for credit. Any guest packet that
    /// gives credit clears the request, so data with credit is never stuck.
    fn has_packet(&self) -> bool {
        !self.dead
            && (self.owes_response
                || self.owes_credit
                || (self.outbound.is_empty() && self.host_closed && !self.shutdown_sent)
                || (!self.outbound.is_empty() && !self.asked_for_credit))
    }

    /// The next packet for the guest, with room for `room` payload bytes.
    fn next_packet(&mut self, room: usize) -> Option<(u16, u32, Vec<u8>)> {
        if self.dead {
            return None;
        }
        if self.owes_response {
            self.owes_response = false;
            return Some((OP_RESPONSE, 0, Vec::new()));
        }
        let len = room.min(self.credit()).min(self.outbound.len());
        if len > 0 {
            self.tx_cnt = self.tx_cnt.wrapping_add(len as u32);
            return Some((OP_RW, 0, self.outbound.drain(..len).collect()));
        }
        if !self.outbound.is_empty() && self.credit() == 0 && !self.asked_for_credit {
            self.asked_for_credit = true;
            return Some((OP_CREDIT_REQUEST, 0, Vec::new()));
        }
        if self.outbound.is_empty() && self.host_closed && !self.shutdown_sent {
            self.shutdown_sent = true;
            return Some((OP_SHUTDOWN, SHUTDOWN_RECEIVE | SHUTDOWN_SEND, Vec::new()));
        }
        if self.owes_credit {
            return Some((OP_CREDIT_UPDATE, 0, Vec::new()));
        }
        None
    }

    fn finished(&self) -> bool {
        self.dead && (self.host_closed || (!self.accepted && self.inbound.is_empty()))
    }
}

/// A virtio-vsock device and the host's end of its connections.
pub struct Vsock {
    transport: Transport,
    guest_cid: u64,
    listening: Vec<u32>,
    connections: BTreeMap<Connection, State>,
    control: VecDeque<Header>,
    generation: u64,
    /// The connection served last, so that sharing is fair across calls.
    last_served: Connection,
}

impl Vsock {
    /// A device for a guest numbered `guest_cid`, which may connect to the
    /// host ports in `listening`.
    pub fn new(guest_cid: u64, listening: &[u32]) -> Self {
        Self {
            transport: Transport::new(
                DEVICE_ID,
                0,
                guest_cid.to_le_bytes().to_vec(),
                QUEUES,
                QUEUE_SIZE,
            ),
            guest_cid,
            listening: listening.to_vec(),
            connections: BTreeMap::new(),
            control: VecDeque::new(),
            generation: 0,
            last_served: Connection::default(),
        }
    }

    /// Handles a guest read of the device's registers.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        self.transport.read(offset, data);
    }

    /// Handles a guest write to the device's registers. Returns true if the
    /// guest should get an interrupt.
    pub fn write(&mut self, memory: &GuestMemory, offset: u64, data: &[u8]) -> bool {
        let notified = self.transport.write(offset, data).is_some();
        if offset == STATUS && self.status() == 0 {
            self.connections.clear();
            self.control.clear();
            self.generation += 1;
        }
        notified && self.service(memory)
    }

    fn status(&self) -> u32 {
        let mut status = [0; 4];
        self.transport.read(STATUS, &mut status);
        u32::from_le_bytes(status)
    }

    /// Moves packets in both directions. Call it after using a connection.
    /// Returns true if the guest should get an interrupt.
    pub fn service(&mut self, memory: &GuestMemory) -> bool {
        // Sending first makes room for the replies that reading adds.
        let sent = self.fill_receive_buffers(memory);
        let read = self.read_guest_packets(memory);
        let replied = self.fill_receive_buffers(memory);
        let used = sent || read || replied;
        if used {
            self.transport.signal_used();
        }
        used
    }

    /// The next connection the host hasn't seen yet.
    pub fn accept(&mut self) -> Option<Connection> {
        let (connection, state) = self.connections.iter_mut().find(|(_, s)| !s.accepted)?;
        state.accepted = true;
        Some(*connection)
    }

    /// Queues bytes for the guest, and returns how many fit.
    pub fn send(&mut self, connection: Connection, bytes: &[u8]) -> usize {
        let Some(state) = self.connections.get_mut(&connection) else {
            return 0;
        };
        if state.dead || state.host_closed || state.guest_stopped_receiving {
            return 0;
        }
        let len = bytes.len().min(MAX_OUTBOUND - state.outbound.len());
        state.outbound.extend(&bytes[..len]);
        len
    }

    /// Reads bytes from the guest, and returns how many there were.
    pub fn recv(&mut self, connection: Connection, buffer: &mut [u8]) -> usize {
        let Some(state) = self.connections.get_mut(&connection) else {
            return 0;
        };
        let len = buffer.len().min(state.inbound.len());
        for (slot, byte) in buffer.iter_mut().zip(state.inbound.drain(..len)) {
            *slot = byte;
        }
        state.fwd_cnt = state.fwd_cnt.wrapping_add(len as u32);
        // The guest's view is at most half a buffer behind, so it always
        // has credit left and can't stall waiting for this update.
        if state.fwd_cnt.wrapping_sub(state.advertised_fwd_cnt) >= BUFFER / 2 {
            state.owes_credit = true;
        }
        len
    }

    /// Whether the guest will send nothing more, and all it sent has been read.
    pub fn guest_finished(&self, connection: Connection) -> bool {
        self.connections.get(&connection).is_none_or(|state| {
            state.inbound.is_empty() && (state.dead || state.guest_stopped_sending)
        })
    }

    /// Closes the host's end, after what it queued reaches the guest.
    pub fn close(&mut self, connection: Connection) {
        if let Some(state) = self.connections.get_mut(&connection) {
            state.host_closed = true;
            state.inbound.clear();
        }
        self.forget_if_finished(connection);
    }

    fn forget_if_finished(&mut self, connection: Connection) {
        if self
            .connections
            .get(&connection)
            .is_some_and(State::finished)
        {
            self.connections.remove(&connection);
        }
    }

    fn read_guest_packets(&mut self, memory: &GuestMemory) -> bool {
        let mut used = false;
        while self.control.len() < MAX_CONTROL {
            let Some(queue) = self.transport.queue(TX) else {
                break;
            };
            // A broken ring has already stopped the queue.
            let Ok(Some(chain)) = queue.pop(memory) else {
                break;
            };
            self.take_packet(memory, &chain);
            let Some(queue) = self.transport.queue(TX) else {
                break;
            };
            if queue.add_used(memory, chain.head, 0).is_err() {
                break;
            }
            used = true;
        }
        used
    }

    fn take_packet(&mut self, memory: &GuestMemory, chain: &Chain) {
        let readable: usize = chain
            .buffers
            .iter()
            .filter(|buffer| !buffer.writable)
            .map(|buffer| buffer.len as usize)
            .sum();
        let mut raw = [0; HEADER];
        if readable < HEADER || read_chain(memory, chain, 0, &mut raw).is_err() {
            return;
        }
        let header = Header::parse(&raw);
        // A packet that claims another machine's address has no one to answer.
        if header.src_cid != self.guest_cid || header.dst_cid != HOST_CID {
            return;
        }
        if header.kind != TYPE_STREAM {
            self.refuse(&header);
            return;
        }
        let connection = Connection {
            guest_port: header.src_port,
            host_port: header.dst_port,
            generation: self.generation,
        };
        if header.op == OP_REQUEST {
            self.open(connection, &header);
            return;
        }
        let Some(state) = self
            .connections
            .get_mut(&connection)
            .filter(|state| !state.dead)
        else {
            self.refuse(&header);
            return;
        };
        state.peer_buf_alloc = header.buf_alloc;
        state.peer_fwd_cnt = header.fwd_cnt;
        if state.credit() > 0 {
            state.asked_for_credit = false;
        }
        match header.op {
            OP_RW => {
                if !take_payload(memory, chain, state, &header, readable) {
                    self.abort(connection, &header);
                }
            }
            OP_CREDIT_REQUEST => state.owes_credit = true,
            OP_CREDIT_UPDATE => {}
            OP_SHUTDOWN => {
                state.guest_stopped_sending |= header.flags & SHUTDOWN_SEND != 0;
                state.guest_stopped_receiving |= header.flags & SHUTDOWN_RECEIVE != 0;
                if state.guest_stopped_receiving {
                    state.outbound.clear();
                }
                if state.guest_stopped_sending && state.guest_stopped_receiving {
                    self.abort(connection, &header);
                }
            }
            // A reset, or an operation with no meaning, ends the connection.
            // `refuse` never answers a reset.
            _ => self.abort(connection, &header),
        }
        self.forget_if_finished(connection);
    }

    fn open(&mut self, connection: Connection, header: &Header) {
        // The guest thinks a reset ends the connection, so it has to.
        if self.connections.contains_key(&connection) {
            self.abort(connection, header);
            return;
        }
        if !self.listening.contains(&connection.host_port)
            || self.connections.len() >= MAX_CONNECTIONS
        {
            self.refuse(header);
            return;
        }
        self.connections.insert(
            connection,
            State {
                owes_response: true,
                peer_buf_alloc: header.buf_alloc,
                peer_fwd_cnt: header.fwd_cnt,
                ..State::default()
            },
        );
    }

    /// Ends a connection from the host's side.
    fn abort(&mut self, connection: Connection, header: &Header) {
        if let Some(state) = self.connections.get_mut(&connection) {
            state.dead = true;
            state.outbound.clear();
        }
        self.refuse(header);
        self.forget_if_finished(connection);
    }

    /// Queues a reset that answers `header`.
    fn refuse(&mut self, header: &Header) {
        if header.op != OP_RST {
            self.control.push_back(Header {
                src_cid: HOST_CID,
                dst_cid: self.guest_cid,
                src_port: header.dst_port,
                dst_port: header.src_port,
                kind: TYPE_STREAM,
                op: OP_RST,
                buf_alloc: BUFFER,
                ..Header::default()
            });
        }
    }

    fn fill_receive_buffers(&mut self, memory: &GuestMemory) -> bool {
        let Self {
            transport,
            guest_cid,
            connections,
            control,
            last_served,
            ..
        } = self;
        let Some(queue) = transport.queue(RX) else {
            return false;
        };
        let mut used = false;
        // One packet per connection each round, so that none can starve the
        // rest. Each round uses up a buffer, and the guest has only so many.
        loop {
            let mut progress = false;
            while let Some(reply) = control.front().copied() {
                let Ok(Some(chain)) = queue.pop(memory) else {
                    return used;
                };
                let len = write_packet(memory, &chain, &reply, &[]);
                if len > 0 {
                    control.pop_front();
                }
                if queue.add_used(memory, chain.head, len).is_err() {
                    return used;
                }
                used = true;
                progress = true;
            }
            let order: Vec<Connection> = connections
                .range((Excluded(*last_served), Unbounded))
                .chain(connections.range(..=*last_served))
                .map(|(connection, _)| *connection)
                .collect();
            for connection in order {
                let Some(state) = connections.get_mut(&connection) else {
                    continue;
                };
                if !state.has_packet() {
                    continue;
                }
                let Ok(Some(chain)) = queue.pop(memory) else {
                    return used;
                };
                let room = writable_len(&chain).saturating_sub(HEADER).min(MAX_PAYLOAD);
                let mut len = 0;
                if writable_len(&chain) >= HEADER
                    && let Some((op, flags, payload)) = state.next_packet(room)
                {
                    state.owes_credit = false;
                    state.advertised_fwd_cnt = state.fwd_cnt;
                    let header = Header {
                        src_cid: HOST_CID,
                        dst_cid: *guest_cid,
                        src_port: connection.host_port,
                        dst_port: connection.guest_port,
                        len: payload.len() as u32,
                        kind: TYPE_STREAM,
                        op,
                        flags,
                        buf_alloc: BUFFER,
                        fwd_cnt: state.fwd_cnt,
                    };
                    len = write_packet(memory, &chain, &header, &payload);
                }
                if queue.add_used(memory, chain.head, len).is_err() {
                    return used;
                }
                *last_served = connection;
                used = true;
                progress = true;
            }
            if !progress {
                return used;
            }
        }
    }
}

/// Adds the payload of a data packet to the connection. Returns false if the
/// guest broke the rules, which ends the connection.
fn take_payload(
    memory: &GuestMemory,
    chain: &Chain,
    state: &mut State,
    header: &Header,
    readable: usize,
) -> bool {
    let len = header.len as usize;
    if state.guest_stopped_sending
        || len > MAX_PAYLOAD
        || len > readable - HEADER
        || state.inbound.len() + len > BUFFER as usize
    {
        return false;
    }
    // Once the host has closed, nothing will read this.
    if state.host_closed {
        return true;
    }
    let mut payload = vec![0; len];
    if read_chain(memory, chain, HEADER, &mut payload).is_err() {
        return false;
    }
    state.inbound.extend(payload);
    true
}

fn writable_len(chain: &Chain) -> usize {
    chain
        .buffers
        .iter()
        .filter(|buffer| buffer.writable)
        .map(|buffer| buffer.len as usize)
        .sum()
}

/// Fills `out` from the chain's readable buffers, starting `skip` bytes in.
fn read_chain(
    memory: &GuestMemory,
    chain: &Chain,
    mut skip: usize,
    out: &mut [u8],
) -> Result<(), OutOfBounds> {
    let mut filled = 0;
    for buffer in chain.buffers.iter().filter(|buffer| !buffer.writable) {
        let len = buffer.len as usize;
        if skip >= len {
            skip -= len;
            continue;
        }
        let take = (len - skip).min(out.len() - filled);
        memory.read(
            buffer.address + skip as u64,
            &mut out[filled..filled + take],
        )?;
        filled += take;
        skip = 0;
        if filled == out.len() {
            break;
        }
    }
    Ok(())
}

/// Writes a packet across the chain's writable buffers, and returns how many
/// bytes it took, or 0 if it doesn't fit.
fn write_packet(memory: &GuestMemory, chain: &Chain, header: &Header, payload: &[u8]) -> u32 {
    if writable_len(chain) < HEADER + payload.len() {
        return 0;
    }
    let raw = header.encode();
    let mut parts = [&raw[..], payload];
    let mut written = 0;
    for buffer in chain.buffers.iter().filter(|buffer| buffer.writable) {
        let mut at = buffer.address;
        let mut room = buffer.len as usize;
        for part in &mut parts {
            let take = room.min(part.len());
            let (now, later) = part.split_at(take);
            if memory.write(at, now).is_err() {
                return 0;
            }
            *part = later;
            at += take as u64;
            room -= take;
            written += take;
        }
    }
    written as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST: u64 = 3;
    const PORT: u32 = 1024;
    const GUEST_PORT: u32 = 5000;
    const SIZE: u16 = 16;
    const QUEUE_NUM: u64 = 0x038;
    const QUEUE_SELECT: u64 = 0x030;
    const QUEUE_READY: u64 = 0x044;
    const QUEUE_NOTIFY: u64 = 0x050;
    const QUEUE_DESC: u64 = 0x080;
    const QUEUE_DRIVER: u64 = 0x090;
    const QUEUE_DEVICE: u64 = 0x0a0;
    const DRIVER_OK: u32 = 4;
    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    const TX_DATA: u64 = 0x10_0000;
    const RX_DATA: u64 = 0x20_0000;
    const RX_BUFFER: u32 = 4096;

    fn table(queue: usize) -> u64 {
        0x1_0000 * (queue as u64 + 1)
    }
    fn available(queue: usize) -> u64 {
        table(queue) + 0x1000
    }
    fn used(queue: usize) -> u64 {
        table(queue) + 0x2000
    }

    /// A fake guest driver, which writes the rings as Linux would.
    struct Driver {
        memory: GuestMemory,
        device: Vsock,
        tx_published: u16,
        rx_published: u16,
        rx_seen: u16,
        interrupted: bool,
    }

    impl Driver {
        fn new() -> Self {
            let mut driver = Self {
                memory: GuestMemory::new(1 << 23).unwrap(),
                device: Vsock::new(GUEST, &[PORT]),
                tx_published: 0,
                rx_published: 0,
                rx_seen: 0,
                interrupted: false,
            };
            driver.restart();
            driver
        }

        /// Sets the rings up on this driver's device.
        fn restart(&mut self) {
            for queue in 0..QUEUES {
                self.set(QUEUE_SELECT, queue as u32);
                self.set(QUEUE_NUM, u32::from(SIZE));
                self.set(QUEUE_DESC, table(queue) as u32);
                self.set(QUEUE_DRIVER, available(queue) as u32);
                self.set(QUEUE_DEVICE, used(queue) as u32);
                self.set(QUEUE_READY, 1);
            }
            self.set(STATUS, DRIVER_OK);
        }

        fn set(&mut self, offset: u64, value: u32) {
            self.interrupted |= self
                .device
                .write(&self.memory, offset, &value.to_le_bytes());
        }

        fn descriptor(&self, queue: usize, i: u16, address: u64, len: u32, flags: u16, next: u16) {
            let at = table(queue) + u64::from(i) * 16;
            self.memory.write(at, &address.to_le_bytes()).unwrap();
            self.memory.write(at + 8, &len.to_le_bytes()).unwrap();
            self.memory.write(at + 12, &flags.to_le_bytes()).unwrap();
            self.memory.write(at + 14, &next.to_le_bytes()).unwrap();
        }

        fn publish_tx(&mut self, head: u16) {
            let slot = u64::from(self.tx_published % SIZE);
            self.memory
                .write(available(TX) + 4 + slot * 2, &head.to_le_bytes())
                .unwrap();
            self.tx_published = self.tx_published.wrapping_add(1);
            self.memory
                .write(available(TX) + 2, &self.tx_published.to_le_bytes())
                .unwrap();
            self.set(QUEUE_NOTIFY, TX as u32);
        }

        /// Sends one packet, with its header and payload in two descriptors.
        fn send(&mut self, header: Header, payload: &[u8]) {
            self.memory.write(TX_DATA, &header.encode()).unwrap();
            self.memory.write(TX_DATA + 4096, payload).unwrap();
            self.descriptor(TX, 0, TX_DATA, HEADER as u32, NEXT, 1);
            self.descriptor(TX, 1, TX_DATA + 4096, payload.len() as u32, 0, 0);
            self.publish_tx(0);
        }

        fn post_rx(&mut self, count: u16) {
            for _ in 0..count {
                let head = self.rx_published % SIZE;
                let address = RX_DATA + u64::from(head) * u64::from(RX_BUFFER);
                self.descriptor(RX, head, address, RX_BUFFER, WRITE, 0);
                self.memory
                    .write(available(RX) + 4 + u64::from(head) * 2, &head.to_le_bytes())
                    .unwrap();
                self.rx_published = self.rx_published.wrapping_add(1);
            }
            self.memory
                .write(available(RX) + 2, &self.rx_published.to_le_bytes())
                .unwrap();
            self.set(QUEUE_NOTIFY, RX as u32);
        }

        fn used_index(&self, queue: usize) -> u16 {
            let mut bytes = [0; 2];
            self.memory.read(used(queue) + 2, &mut bytes).unwrap();
            u16::from_le_bytes(bytes)
        }

        /// The packets the device has written that this driver hasn't seen.
        fn received(&mut self) -> Vec<(Header, Vec<u8>)> {
            let mut packets = Vec::new();
            while self.rx_seen != self.used_index(RX) {
                let at = used(RX) + 4 + u64::from(self.rx_seen % SIZE) * 8;
                let mut element = [0; 8];
                self.memory.read(at, &mut element).unwrap();
                let head = u32::from_le_bytes(element[..4].try_into().unwrap());
                let len = u32::from_le_bytes(element[4..].try_into().unwrap()) as usize;
                let address = RX_DATA + u64::from(head) * u64::from(RX_BUFFER);
                let mut raw = [0; HEADER];
                self.memory.read(address, &mut raw).unwrap();
                let mut payload = vec![0; len - HEADER];
                self.memory
                    .read(address + HEADER as u64, &mut payload)
                    .unwrap();
                packets.push((Header::parse(&raw), payload));
                self.rx_seen = self.rx_seen.wrapping_add(1);
            }
            packets
        }

        fn ops(&mut self) -> Vec<u16> {
            self.received().iter().map(|(h, _)| h.op).collect()
        }

        fn service(&mut self) {
            self.interrupted |= self.device.service(&self.memory);
        }

        /// A connection the guest opened and the host accepted, with its
        /// response already taken.
        fn connected(&mut self) -> Connection {
            self.post_rx(4);
            self.send(packet(OP_REQUEST), &[]);
            assert_eq!(self.ops(), [OP_RESPONSE]);
            self.device.accept().unwrap()
        }
    }

    fn packet(op: u16) -> Header {
        Header {
            src_cid: GUEST,
            dst_cid: HOST_CID,
            src_port: GUEST_PORT,
            dst_port: PORT,
            kind: TYPE_STREAM,
            op,
            buf_alloc: 1 << 20,
            ..Header::default()
        }
    }

    fn data(len: usize) -> Header {
        Header {
            len: len as u32,
            ..packet(OP_RW)
        }
    }

    fn shutdown(flags: u32) -> Header {
        Header {
            flags,
            ..packet(OP_SHUTDOWN)
        }
    }

    #[test]
    fn the_guest_reads_its_cid_and_device_id() {
        let device = Vsock::new(0x1234_5678_9abc, &[]);
        let mut id = [0; 4];
        device.read(0x008, &mut id);
        assert_eq!(u32::from_le_bytes(id), 19);
        let mut cid = [0; 8];
        device.read(0x100, &mut cid);
        assert_eq!(u64::from_le_bytes(cid), 0x1234_5678_9abc);
    }

    #[test]
    fn a_header_survives_encoding() {
        let raw: [u8; HEADER] = std::array::from_fn(|i| i as u8);
        assert_eq!(Header::parse(&raw).encode(), raw);
    }

    #[test]
    fn a_request_to_a_listening_port_is_accepted_and_interrupts() {
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver.interrupted = false;
        driver.send(packet(OP_REQUEST), &[]);
        let packets = driver.received();
        assert!(driver.interrupted);
        let (reply, _) = packets[0];
        assert_eq!(reply.op, OP_RESPONSE);
        assert_eq!((reply.src_cid, reply.dst_cid), (HOST_CID, GUEST));
        assert_eq!((reply.src_port, reply.dst_port), (PORT, GUEST_PORT));
        assert_eq!(reply.kind, TYPE_STREAM);
        assert_eq!(reply.buf_alloc, BUFFER);
        assert_eq!(driver.device.accept().map(|c| c.port()), Some(PORT));
        assert!(driver.device.accept().is_none());
    }

    #[test]
    fn a_request_to_a_closed_port_is_reset() {
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver.send(
            Header {
                dst_port: PORT + 1,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        let packets = driver.received();
        let (reply, _) = packets[0];
        assert_eq!(reply.op, OP_RST);
        assert_eq!((reply.src_cid, reply.dst_cid), (HOST_CID, GUEST));
        assert_eq!((reply.src_port, reply.dst_port), (PORT + 1, GUEST_PORT));
        assert_eq!(reply.kind, TYPE_STREAM);
        assert_eq!(reply.buf_alloc, BUFFER);
        assert!(driver.device.accept().is_none());
    }

    #[test]
    fn a_second_request_on_an_open_connection_is_reset() {
        let mut driver = Driver::new();
        driver.connected();
        driver.send(packet(OP_REQUEST), &[]);
        assert_eq!(driver.ops(), [OP_RST]);
    }

    #[test]
    fn a_second_request_ends_the_connection_it_collides_with() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(packet(OP_REQUEST), &[]);
        assert_eq!(driver.device.send(connection, b"x"), 0);
        assert!(driver.device.guest_finished(connection));
    }

    #[test]
    fn a_handle_kept_across_a_reset_cant_reach_a_new_connection() {
        let mut driver = Driver::new();
        let old = driver.connected();
        driver.set(STATUS, 0);
        let mut driver = Driver {
            device: driver.device,
            ..Driver::new()
        };
        driver.restart();
        let new = driver.connected();
        assert_ne!(old, new);
        assert_eq!(driver.device.send(old, b"x"), 0);
        driver.device.close(old);
        assert_eq!(driver.device.send(new, b"x"), 1);
    }

    #[test]
    fn data_reaches_the_host_in_order_across_descriptors() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(data(5), b"hello");
        driver.send(data(6), b" world");
        let mut buffer = [0; 32];
        let len = driver.device.recv(connection, &mut buffer);
        assert_eq!(&buffer[..len], b"hello world");
        assert_eq!(driver.device.recv(connection, &mut buffer), 0);
    }

    #[test]
    fn a_payload_may_start_in_the_header_descriptor_and_continue_in_the_next() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.memory.write(TX_DATA, &data(5).encode()).unwrap();
        driver.memory.write(TX_DATA + 44, b"he").unwrap();
        driver.memory.write(TX_DATA + 46, b"XXXX").unwrap();
        driver.memory.write(TX_DATA + 4096, b"lloYYYYY").unwrap();
        driver.descriptor(TX, 0, TX_DATA, 46, NEXT, 1);
        driver.descriptor(TX, 1, TX_DATA + 4096, 8, 0, 0);
        driver.publish_tx(0);
        let mut buffer = [0; 8];
        assert_eq!(driver.device.recv(connection, &mut buffer), 5);
        assert_eq!(&buffer[..5], b"hello");
    }

    #[test]
    fn a_packet_in_one_descriptor_is_read_too() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.memory.write(TX_DATA, &data(2).encode()).unwrap();
        driver.memory.write(TX_DATA + 44, b"ok").unwrap();
        driver.descriptor(TX, 0, TX_DATA, 46, 0, 0);
        driver.publish_tx(0);
        let mut buffer = [0; 4];
        assert_eq!(driver.device.recv(connection, &mut buffer), 2);
        assert_eq!(&buffer[..2], b"ok");
    }

    #[test]
    fn host_data_reaches_the_guest_split_to_fit_its_buffers() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        let bytes: Vec<u8> = (0..6000).map(|i| i as u8).collect();
        assert_eq!(driver.device.send(connection, &bytes), bytes.len());
        driver.service();
        let packets = driver.received();
        assert!(packets.len() >= 2);
        assert!(
            packets
                .iter()
                .all(|(h, p)| h.op == OP_RW && h.len as usize == p.len())
        );
        assert_eq!(packets.concat_payloads(), bytes);
    }

    trait Concat {
        fn concat_payloads(&self) -> Vec<u8>;
    }

    impl Concat for Vec<(Header, Vec<u8>)> {
        fn concat_payloads(&self) -> Vec<u8> {
            self.iter()
                .flat_map(|(_, payload)| payload.clone())
                .collect()
        }
    }

    #[test]
    fn nothing_is_lost_when_the_guest_has_no_buffers_yet() {
        let mut driver = Driver::new();
        driver.send(packet(OP_REQUEST), &[]);
        assert_eq!(driver.used_index(RX), 0);
        driver.post_rx(1);
        assert_eq!(driver.ops(), [OP_RESPONSE]);
    }

    #[test]
    fn the_host_sends_no_more_than_the_guest_has_credit_for() {
        let mut driver = Driver::new();
        driver.post_rx(4);
        driver.send(
            Header {
                buf_alloc: 10,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        let connection = driver.device.accept().unwrap();
        driver.device.send(connection, &[7; 25]);
        driver.service();
        let packets = driver.received();
        assert_eq!(packets.concat_payloads().len(), 10);

        // The guest consumes 4 bytes, which frees exactly that much.
        driver.post_rx(2);
        driver.send(
            Header {
                buf_alloc: 10,
                fwd_cnt: 4,
                ..packet(OP_CREDIT_UPDATE)
            },
            &[],
        );
        assert_eq!(driver.received().concat_payloads().len(), 4);
    }

    #[test]
    fn a_guest_reporting_impossible_credit_gets_no_data_and_nothing_breaks() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(
            Header {
                buf_alloc: 100,
                fwd_cnt: 50,
                ..packet(OP_CREDIT_UPDATE)
            },
            &[],
        );
        driver.device.send(connection, &[1; 20]);
        driver.service();
        assert_eq!(driver.ops(), [OP_CREDIT_REQUEST]);
    }

    #[test]
    fn a_blocked_host_asks_for_credit_once_and_resumes_when_it_comes() {
        let mut driver = Driver::new();
        driver.post_rx(4);
        driver.send(
            Header {
                buf_alloc: 4,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        let connection = driver.device.accept().unwrap();
        driver.device.send(connection, &[7; 8]);
        driver.service();
        assert_eq!(driver.ops(), [OP_RESPONSE, OP_RW, OP_CREDIT_REQUEST]);
        driver.post_rx(4);
        driver.service();
        assert!(driver.ops().is_empty(), "asking once is enough");
        // A packet that gives no credit leaves the request standing.
        driver.send(
            Header {
                buf_alloc: 4,
                ..packet(OP_CREDIT_UPDATE)
            },
            &[],
        );
        assert!(driver.ops().is_empty(), "no credit came");
        driver.send(
            Header {
                buf_alloc: 4,
                fwd_cnt: 4,
                ..packet(OP_CREDIT_UPDATE)
            },
            &[],
        );
        assert_eq!(driver.received().concat_payloads(), [7; 4]);
    }

    #[test]
    fn a_busy_connection_cant_starve_another_across_calls() {
        let mut driver = Driver::new();
        let first = driver.connected();
        driver.send(
            Header {
                src_port: GUEST_PORT + 1,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        assert_eq!(driver.ops(), [OP_RESPONSE]);
        let second = driver.device.accept().unwrap();
        driver.device.send(first, &vec![1; 40_000]);
        driver.service();
        assert_eq!(driver.ops(), [OP_RW, OP_RW], "the spare buffers are gone");
        driver.device.send(second, &[2; 8]);
        driver.post_rx(1);
        let packets = driver.received();
        assert_eq!(packets[0].0.dst_port, GUEST_PORT + 1);
    }

    #[test]
    fn a_credit_request_from_a_guest_with_no_room_is_only_answered() {
        let mut driver = Driver::new();
        driver.connected();
        driver.send(
            Header {
                buf_alloc: 0,
                ..packet(OP_CREDIT_REQUEST)
            },
            &[],
        );
        assert_eq!(driver.ops(), [OP_CREDIT_UPDATE]);
    }

    #[test]
    fn a_request_that_claims_bytes_were_consumed_gets_no_credit() {
        let mut driver = Driver::new();
        driver.post_rx(4);
        driver.send(
            Header {
                buf_alloc: 10,
                fwd_cnt: 4,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        let connection = driver.device.accept().unwrap();
        driver.device.send(connection, &[7; 25]);
        driver.service();
        assert_eq!(driver.ops(), [OP_RESPONSE, OP_CREDIT_REQUEST]);
    }

    #[test]
    fn the_host_queues_no_more_than_its_limit() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        let bytes = vec![1; MAX_OUTBOUND + 1];
        assert_eq!(driver.device.send(connection, &bytes), MAX_OUTBOUND);
        assert_eq!(driver.device.send(connection, &bytes), 0);
    }

    #[test]
    fn a_packet_the_guest_sends_interrupts_even_with_no_reply() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.interrupted = false;
        driver.send(data(2), b"hi");
        assert!(driver.interrupted);
        assert_eq!(driver.device.recv(connection, &mut [0; 2]), 2);
    }

    #[test]
    fn writing_a_status_other_than_zero_keeps_the_connections() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.set(STATUS, DRIVER_OK);
        assert_eq!(driver.device.send(connection, b"x"), 1);
    }

    #[test]
    fn a_credit_request_is_answered_with_what_the_host_consumed() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(data(3), b"abc");
        driver.device.recv(connection, &mut [0; 3]);
        driver.send(packet(OP_CREDIT_REQUEST), &[]);
        let packets = driver.received();
        assert_eq!(packets[0].0.op, OP_CREDIT_UPDATE);
        assert_eq!(packets[0].0.fwd_cnt, 3);
        assert_eq!(packets[0].0.buf_alloc, BUFFER);
    }

    #[test]
    fn the_host_announces_credit_before_the_guest_could_run_out() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        let chunk = vec![0; MAX_PAYLOAD];
        for _ in 0..2 {
            driver.send(data(MAX_PAYLOAD), &chunk);
        }
        let mut buffer = vec![0; MAX_PAYLOAD * 2];
        driver.device.recv(connection, &mut buffer[..MAX_PAYLOAD]);
        driver.service();
        assert!(driver.received().is_empty(), "under half a buffer is quiet");
        driver.device.recv(connection, &mut buffer);
        driver.service();
        let packets = driver.received();
        assert_eq!(packets[0].0.op, OP_CREDIT_UPDATE);
        assert_eq!(packets[0].0.fwd_cnt, BUFFER / 2);
    }

    #[test]
    fn a_guest_that_ignores_flow_control_is_reset_and_the_host_stays_bounded() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        let chunk = vec![1; MAX_PAYLOAD];
        for _ in 0..BUFFER as usize / MAX_PAYLOAD {
            driver.send(data(MAX_PAYLOAD), &chunk);
        }
        assert!(driver.ops().is_empty(), "a full buffer is within the rules");
        driver.send(data(1), &[1]);
        assert_eq!(driver.ops(), [OP_RST]);
        let state = &driver.device.connections[&connection];
        assert_eq!(state.inbound.len(), BUFFER as usize);
        assert!(state.dead);
    }

    #[test]
    fn a_guest_shutting_down_its_sending_side_ends_the_stream_after_the_data() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(data(2), b"hi");
        driver.send(shutdown(SHUTDOWN_SEND), &[]);
        assert!(!driver.device.guest_finished(connection));
        driver.device.recv(connection, &mut [0; 2]);
        assert!(driver.device.guest_finished(connection));
        // The other direction still works.
        assert_eq!(driver.device.send(connection, b"x"), 1);
        driver.service();
        assert_eq!(driver.ops(), [OP_RW]);
    }

    #[test]
    fn data_after_a_guest_shutdown_resets_the_connection() {
        let mut driver = Driver::new();
        driver.connected();
        driver.send(shutdown(SHUTDOWN_SEND), &[]);
        driver.send(data(1), &[1]);
        assert_eq!(driver.ops(), [OP_RST]);
    }

    #[test]
    fn a_guest_shutting_down_receiving_drops_what_the_host_queued() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(
            Header {
                buf_alloc: 0,
                ..packet(OP_CREDIT_UPDATE)
            },
            &[],
        );
        driver.device.send(connection, b"late");
        driver.send(shutdown(SHUTDOWN_RECEIVE), &[]);
        assert_eq!(driver.device.send(connection, b"x"), 0);
        driver.service();
        assert!(!driver.ops().contains(&OP_RW));
        // The guest can still send.
        driver.send(data(2), b"hi");
        assert!(driver.ops().is_empty());
        assert_eq!(driver.device.recv(connection, &mut [0; 2]), 2);
    }

    #[test]
    fn a_full_guest_shutdown_is_answered_with_a_reset_and_keeps_unread_data() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(data(2), b"hi");
        driver.send(shutdown(SHUTDOWN_SEND | SHUTDOWN_RECEIVE), &[]);
        assert_eq!(driver.ops(), [OP_RST]);
        let mut buffer = [0; 2];
        assert_eq!(driver.device.recv(connection, &mut buffer), 2);
        assert!(driver.device.guest_finished(connection));
        driver.device.close(connection);
        assert!(driver.device.connections.is_empty());
    }

    #[test]
    fn a_host_close_flushes_first_and_a_guest_reset_finishes_it() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.device.send(connection, b"bye");
        driver.device.close(connection);
        assert_eq!(driver.device.send(connection, b"x"), 0);
        driver.service();
        let packets = driver.received();
        assert_eq!(packets[0].1, b"bye");
        assert_eq!(packets[1].0.op, OP_SHUTDOWN);
        assert_eq!(packets[1].0.flags, SHUTDOWN_SEND | SHUTDOWN_RECEIVE);
        assert_eq!(driver.device.connections.len(), 1);
        driver.send(packet(OP_RST), &[]);
        assert!(driver.ops().is_empty(), "a reset is never answered");
        assert!(driver.device.connections.is_empty());
    }

    #[test]
    fn data_arriving_after_a_host_close_is_dropped_without_a_reset() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.device.close(connection);
        driver.service();
        assert_eq!(driver.ops(), [OP_SHUTDOWN]);
        driver.send(data(1), &[1]);
        assert!(driver.ops().is_empty());
        assert!(driver.device.connections[&connection].inbound.is_empty());
    }

    #[test]
    fn a_guest_reset_stops_sending_and_ends_the_stream() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(packet(OP_RST), &[]);
        assert_eq!(driver.device.send(connection, b"x"), 0);
        assert!(driver.device.guest_finished(connection));
    }

    #[test]
    fn a_connection_that_died_unaccepted_with_no_data_is_forgotten() {
        let mut driver = Driver::new();
        driver.post_rx(2);
        driver.send(packet(OP_REQUEST), &[]);
        driver.send(packet(OP_RST), &[]);
        assert!(driver.device.accept().is_none());
    }

    #[test]
    fn a_connection_that_died_unaccepted_keeps_its_data_for_the_host() {
        let mut driver = Driver::new();
        driver.post_rx(2);
        driver.send(packet(OP_REQUEST), &[]);
        driver.send(data(2), b"hi");
        driver.send(packet(OP_RST), &[]);
        let connection = driver.device.accept().unwrap();
        let mut buffer = [0; 2];
        assert_eq!(driver.device.recv(connection, &mut buffer), 2);
        assert_eq!(&buffer, b"hi");
    }

    #[test]
    fn packets_with_no_connection_are_reset_but_a_reset_is_not() {
        for op in [OP_RW, OP_SHUTDOWN, OP_CREDIT_UPDATE, OP_CREDIT_REQUEST] {
            let mut driver = Driver::new();
            driver.post_rx(1);
            driver.send(packet(op), &[]);
            assert_eq!(driver.ops(), [OP_RST], "op {op}");
        }
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver.send(packet(OP_RST), &[]);
        assert!(driver.ops().is_empty());
    }

    #[test]
    fn a_header_shorter_than_it_must_be_is_dropped_and_the_chain_returned() {
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver.descriptor(TX, 0, TX_DATA, HEADER as u32 - 1, 0, 0);
        driver.publish_tx(0);
        assert_eq!(driver.used_index(TX), 1);
        assert!(driver.ops().is_empty());
    }

    #[test]
    fn device_writable_buffers_dont_count_as_a_header() {
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver
            .memory
            .write(TX_DATA, &packet(OP_REQUEST).encode())
            .unwrap();
        driver.descriptor(TX, 0, TX_DATA, HEADER as u32, WRITE, 0);
        driver.publish_tx(0);
        assert!(driver.ops().is_empty());
        assert!(driver.device.accept().is_none());
    }

    #[test]
    fn a_packet_claiming_another_address_is_dropped() {
        let mut driver = Driver::new();
        driver.post_rx(2);
        driver.send(
            Header {
                src_cid: GUEST + 1,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        driver.send(
            Header {
                dst_cid: GUEST,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        assert!(driver.ops().is_empty());
        assert!(driver.device.accept().is_none());
    }

    #[test]
    fn a_packet_of_another_socket_type_is_reset() {
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver.send(
            Header {
                kind: 2,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        assert_eq!(driver.ops(), [OP_RST]);
        assert!(driver.device.accept().is_none());
    }

    #[test]
    fn an_unknown_operation_resets_the_connection() {
        for op in [0, OP_RESPONSE, 8, u16::MAX] {
            let mut driver = Driver::new();
            let connection = driver.connected();
            driver.send(packet(op), &[]);
            assert_eq!(driver.ops(), [OP_RST], "op {op}");
            assert_eq!(driver.device.send(connection, b"x"), 0);
        }
    }

    #[test]
    fn a_length_past_the_end_of_the_chain_resets_the_connection() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(data(100), &[0; 50]);
        assert_eq!(driver.ops(), [OP_RST]);
        assert_eq!(driver.device.recv(connection, &mut [0; 8]), 0);
    }

    #[test]
    fn a_length_over_the_packet_limit_resets_the_connection() {
        for len in [MAX_PAYLOAD + 1, u32::MAX as usize] {
            let mut driver = Driver::new();
            driver.connected();
            driver.send(data(len), &[0; MAX_PAYLOAD + 1]);
            assert_eq!(driver.ops(), [OP_RST], "len {len}");
        }
    }

    #[test]
    fn the_number_of_connections_is_bounded() {
        let mut driver = Driver::new();
        for port in 0..MAX_CONNECTIONS as u32 {
            driver.send(
                Header {
                    src_port: port,
                    ..packet(OP_REQUEST)
                },
                &[],
            );
        }
        driver.post_rx(SIZE);
        assert_eq!(driver.ops().len(), SIZE as usize);
        driver.send(
            Header {
                src_port: u32::MAX,
                ..packet(OP_REQUEST)
            },
            &[],
        );
        assert_eq!(driver.device.connections.len(), MAX_CONNECTIONS);
        assert!(driver.device.control.len() == 1);
    }

    #[test]
    fn a_guest_that_never_posts_buffers_stops_being_read() {
        let mut driver = Driver::new();
        for _ in 0..MAX_CONTROL + 10 {
            driver.send(packet(OP_RW), &[]);
        }
        assert_eq!(driver.device.control.len(), MAX_CONTROL);
        assert_eq!(usize::from(driver.used_index(TX)), MAX_CONTROL);
    }

    #[test]
    fn replies_resume_when_buffers_arrive() {
        let mut driver = Driver::new();
        for _ in 0..MAX_CONTROL + 4 {
            driver.send(packet(OP_RW), &[]);
        }
        driver.post_rx(SIZE);
        assert_eq!(driver.ops().len(), SIZE as usize);
        assert_eq!(usize::from(driver.used_index(TX)), MAX_CONTROL + 4);
    }

    #[test]
    fn a_receive_buffer_too_small_for_a_header_is_returned_empty() {
        let mut driver = Driver::new();
        driver.send(packet(OP_RW), &[]);
        driver.descriptor(RX, 0, RX_DATA, HEADER as u32 - 1, WRITE, 0);
        driver
            .memory
            .write(available(RX) + 4, &0u16.to_le_bytes())
            .unwrap();
        driver
            .memory
            .write(available(RX) + 2, &1u16.to_le_bytes())
            .unwrap();
        driver.rx_published = 1;
        driver.set(QUEUE_NOTIFY, RX as u32);
        assert_eq!(driver.used_index(RX), 1);
        assert_eq!(driver.device.control.len(), 1, "the reply still waits");
    }

    #[test]
    fn a_read_only_receive_buffer_is_never_written() {
        let mut driver = Driver::new();
        driver.send(packet(OP_RW), &[]);
        driver.memory.write(RX_DATA, &[0xaa; 64]).unwrap();
        driver.descriptor(RX, 0, RX_DATA, 64, 0, 0);
        driver
            .memory
            .write(available(RX) + 2, &1u16.to_le_bytes())
            .unwrap();
        driver.set(QUEUE_NOTIFY, RX as u32);
        let mut bytes = [0; 64];
        driver.memory.read(RX_DATA, &mut bytes).unwrap();
        assert_eq!(bytes, [0xaa; 64]);
    }

    #[test]
    fn a_packet_may_span_several_receive_buffers() {
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver.send(packet(OP_REQUEST), &[]);
        driver.received();
        let connection = driver.device.accept().unwrap();
        driver.device.send(connection, b"spanning");
        driver.descriptor(RX, 1, RX_DATA + 0x8000, 40, WRITE | NEXT, 2);
        driver.descriptor(RX, 2, RX_DATA + 0x9000, 40, WRITE, 0);
        driver
            .memory
            .write(available(RX) + 4 + 2, &1u16.to_le_bytes())
            .unwrap();
        driver
            .memory
            .write(available(RX) + 2, &2u16.to_le_bytes())
            .unwrap();
        driver.set(QUEUE_NOTIFY, RX as u32);
        let mut head = [0; 40];
        driver.memory.read(RX_DATA + 0x8000, &mut head).unwrap();
        let mut tail = [0; 12];
        driver.memory.read(RX_DATA + 0x9000, &mut tail).unwrap();
        assert_eq!(&tail[4..], b"spanning");
        assert_eq!(&head[16..20], &PORT.to_le_bytes());
    }

    #[test]
    fn a_broken_ring_stops_the_device_without_a_panic() {
        let mut driver = Driver::new();
        driver.post_rx(1);
        driver.descriptor(TX, 0, u64::MAX - 4, 64, 0, 0);
        driver.publish_tx(0);
        assert_eq!(driver.used_index(TX), 0);
        // The queue stays stopped, and a good packet changes nothing.
        driver.send(packet(OP_REQUEST), &[]);
        assert_eq!(driver.used_index(TX), 0);
        assert!(driver.device.accept().is_none());
    }

    #[test]
    fn a_device_reset_forgets_every_connection() {
        let mut driver = Driver::new();
        let connection = driver.connected();
        driver.send(packet(OP_RW), &[]);
        driver.set(STATUS, 0);
        assert!(driver.device.connections.is_empty());
        assert!(driver.device.control.is_empty());
        assert_eq!(driver.device.send(connection, b"x"), 0);
        assert!(driver.device.guest_finished(connection));
    }

    #[test]
    fn a_device_that_isnt_ready_does_nothing() {
        let mut device = Vsock::new(GUEST, &[PORT]);
        let memory = GuestMemory::new(4096).unwrap();
        assert!(!device.service(&memory));
    }

    #[test]
    fn an_unknown_connection_reads_and_writes_nothing() {
        let mut driver = Driver::new();
        let connection = Connection {
            guest_port: 1,
            host_port: 2,
            generation: 0,
        };
        assert_eq!(driver.device.send(connection, b"x"), 0);
        assert_eq!(driver.device.recv(connection, &mut [0; 1]), 0);
        driver.device.close(connection);
    }
}
