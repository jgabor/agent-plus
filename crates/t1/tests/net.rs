//! A guest with a network device: `ap`'s init sets up its link, and its
//! Ethernet frames reach one host socket, the peer this test plays. The
//! guest boots in a process of its own, because that process confines
//! itself, as a run's does.

use std::env;
use std::ffi::c_int;
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use ap::boot::{self, Guest, Report};
use guest::net;
use guest::protocol::ToGuest;
use t1::{guest_binary, kernel};
use vmm::initramfs::Entry;

const CHILD_TEST: &str = "booted_process";
const LINK_VARIABLE: &str = "AP_NET_LINK";
const KERNEL_VARIABLE: &str = "AP_NET_KERNEL";
const INIT_VARIABLE: &str = "AP_NET_INIT";
const PROBE_VARIABLE: &str = "AP_NET_PROBE";
const TIMEOUT: Duration = Duration::from_secs(60);
const START: &str = "--- probe output\n";
const END: &str = "--- probe end\n";

/// The MAC address the peer answers from, as `ap` does.
const GATEWAY_MAC: [u8; 6] = net::GATEWAY_MAC;
const ARP: u16 = 0x0806;
const IPV4: u16 = 0x0800;
const IPV6: u16 = 0x86dd;
const UDP: u8 = 17;
const ICMPV6: u8 = 58;
const NEIGHBOR_SOLICITATION: u8 = 135;
const PORT: u16 = 7;

const F_SETFD: c_int = 2;

unsafe extern "C" {
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
}

/// What the peer saw the guest send.
#[derive(Default, Debug)]
struct Seen {
    /// Each ARP request: the sender's MAC and address, and the address it
    /// asks for.
    arp: Vec<([u8; 6], Ipv4Addr, Ipv4Addr)>,
    /// Each neighbor solicitation: its source and its target.
    solicitations: Vec<(Ipv6Addr, Ipv6Addr)>,
    /// Each UDP payload sent to the gateway over IPv4.
    datagrams: Vec<Vec<u8>>,
}

/// The kernel's path, fetched once for every test in this process.
fn kernel_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(kernel::vmlinux)
}

fn ethernet(to: [u8; 6], kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = to.to_vec();
    frame.extend(GATEWAY_MAC);
    frame.extend(kind.to_be_bytes());
    frame.extend(payload);
    frame
}

fn arp_reply(to_mac: [u8; 6], to: Ipv4Addr) -> Vec<u8> {
    let mut arp = vec![0, 1, 8, 0, 6, 4, 0, 2];
    arp.extend(GATEWAY_MAC);
    arp.extend(net::IPV4_GATEWAY.octets());
    arp.extend(to_mac);
    arp.extend(to.octets());
    ethernet(to_mac, ARP, &arp)
}

fn ipv4_checksum(header: &[u8]) -> u16 {
    let sum: u32 = header
        .chunks(2)
        .map(|pair| u32::from(u16::from_be_bytes([pair[0], pair[1]])))
        .sum();
    let folded = (sum & 0xffff) + (sum >> 16);
    !(((folded & 0xffff) + (folded >> 16)) as u16)
}

/// A UDP datagram from the gateway's port, with no checksum, which IPv4
/// allows.
fn udp_reply(to_mac: [u8; 6], to: Ipv4Addr, port: u16, payload: &[u8]) -> Vec<u8> {
    let total = (20 + 8 + payload.len()) as u16;
    let mut packet = vec![0x45, 0];
    packet.extend(total.to_be_bytes());
    packet.extend([0, 0, 0x40, 0, 64, UDP, 0, 0]);
    packet.extend(net::IPV4_GATEWAY.octets());
    packet.extend(to.octets());
    let checksum = ipv4_checksum(&packet);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
    packet.extend(PORT.to_be_bytes());
    packet.extend(port.to_be_bytes());
    packet.extend((8 + payload.len() as u16).to_be_bytes());
    packet.extend([0, 0]);
    packet.extend(payload);
    ethernet(to_mac, IPV4, &packet)
}

fn mac(bytes: &[u8]) -> [u8; 6] {
    bytes.try_into().unwrap()
}

fn ipv4(bytes: &[u8]) -> Ipv4Addr {
    <[u8; 4]>::try_from(bytes).unwrap().into()
}

fn ipv6(bytes: &[u8]) -> Ipv6Addr {
    <[u8; 16]>::try_from(bytes).unwrap().into()
}

/// Records a frame from the guest, and returns the peer's answer to it, if
/// it has one: the gateway's MAC address, and an echo's answer.
fn handle(frame: &[u8], seen: &mut Seen) -> Option<Vec<u8>> {
    let kind = u16::from_be_bytes(frame.get(12..14)?.try_into().ok()?);
    let body = &frame[14..];
    match kind {
        ARP if body.get(6..8)? == [0, 1] => {
            let (sender_mac, sender, target) =
                (mac(&body[8..14]), ipv4(&body[14..18]), ipv4(&body[24..28]));
            seen.arp.push((sender_mac, sender, target));
            (target == net::IPV4_GATEWAY).then(|| arp_reply(sender_mac, sender))
        }
        IPV4 if body.get(9) == Some(&UDP) => {
            let header = usize::from(body[0] & 0xf) * 4;
            let udp = body.get(header..)?;
            let source = u16::from_be_bytes([udp[0], udp[1]]);
            let destination = u16::from_be_bytes([udp[2], udp[3]]);
            if ipv4(&body[16..20]) != net::IPV4_GATEWAY || destination != PORT {
                return None;
            }
            seen.datagrams.push(udp.get(8..)?.to_vec());
            Some(udp_reply(
                mac(&frame[6..12]),
                ipv4(&body[12..16]),
                source,
                b"pong",
            ))
        }
        IPV6 if body.get(6) == Some(&ICMPV6) && body.get(40) == Some(&NEIGHBOR_SOLICITATION) => {
            let target = ipv6(body.get(48..64)?);
            seen.solicitations.push((ipv6(&body[8..24]), target));
            None
        }
        _ => None,
    }
}

/// Plays the other end of the link until `done`, and returns what it saw.
fn serve(peer: UnixDatagram, done: Arc<AtomicBool>) -> Seen {
    peer.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut seen = Seen::default();
    let mut buffer = [0; 2048];
    while !done.load(Ordering::SeqCst) {
        let Ok(len) = peer.recv(&mut buffer) else {
            continue;
        };
        if let Some(answer) = handle(&buffer[..len], &mut seen) {
            peer.send(&answer).unwrap();
        }
    }
    seen
}

/// Boots the probe in its own process, which inherits the VMM's end of the
/// link, while this one plays the peer. Returns what the probe printed, and
/// what the peer saw.
fn boot_linked() -> (String, Seen) {
    let (peer, link) = UnixDatagram::pair().unwrap();
    let fd = link.as_raw_fd();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(LINK_VARIABLE, fd.to_string())
        .env(KERNEL_VARIABLE, kernel_path())
        .env(INIT_VARIABLE, guest_binary::ap())
        .env(PROBE_VARIABLE, guest_binary::build("net-probe"))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    // SAFETY: the closure only clears close-on-exec on a descriptor this
    // process holds, which is safe between fork and exec.
    unsafe {
        command.pre_exec(move || match fcntl(fd, F_SETFD, 0) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        });
    }
    let done = Arc::new(AtomicBool::new(false));
    let peer = {
        let done = Arc::clone(&done);
        thread::spawn(move || serve(peer, done))
    };
    let output = command.output().unwrap();
    drop(link);
    done.store(true, Ordering::SeqCst);
    let seen = peer.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{:?}\n{stdout}", output.status);
    let probe = stdout
        .split_once(START)
        .and_then(|(_, rest)| rest.split_once(END))
        .map(|(probe, _)| probe.to_owned())
        .unwrap_or_else(|| panic!("no probe output:\n{stdout}"));
    (probe, seen)
}

/// What a guest gets: these binaries, and a link if it has one.
struct Machine<'a> {
    kernel: &'a [u8],
    init: &'a [u8],
    probe: &'a [u8],
    link: Option<OwnedFd>,
    confine: bool,
}

/// Boots the probe as the guest's command, and returns what it printed.
fn boot_probe(machine: Machine) -> String {
    let files = [Entry::File {
        path: "net-probe",
        mode: 0o755,
        contents: machine.probe,
    }];
    let guest = Guest {
        kernel: machine.kernel.into(),
        init: machine.init.into(),
        files: &files,
        layers: Vec::new(),
        request: Some(ToGuest::Run {
            argv: vec![b"/net-probe".to_vec()],
        }),
        timeout: TIMEOUT,
        confine: machine.confine,
        input: None,
    };
    let mut stdout = Vec::new();
    let status = boot::boot_linked(guest, None, machine.link, |report| {
        if let Report::Stdout(bytes) = report {
            stdout.extend_from_slice(bytes);
        }
    });
    assert_eq!(status.map_err(|error| error.to_string()), Ok(Some(0)));
    String::from_utf8(stdout).unwrap()
}

/// The process that boots the guest with the link it inherited, confined.
/// It does nothing in a normal test run.
#[test]
fn booted_process() {
    let Ok(fd) = env::var(LINK_VARIABLE) else {
        return;
    };
    // SAFETY: the parent made this descriptor for this process alone.
    let link = unsafe { OwnedFd::from_raw_fd(fd.parse().unwrap()) };
    let read = |variable| fs::read(env::var_os(variable).unwrap()).unwrap();
    let (kernel, init, probe) = (
        read(KERNEL_VARIABLE),
        read(INIT_VARIABLE),
        read(PROBE_VARIABLE),
    );
    let output = boot_probe(Machine {
        kernel: &kernel,
        init: &init,
        probe: &probe,
        link: Some(link),
        confine: true,
    });
    print!("{START}{output}{END}");
}

/// The lines of the file the probe printed after `== path`.
fn file<'a>(probe: &'a str, path: &str) -> Vec<&'a str> {
    probe
        .split_once(&format!("== {path}\n"))
        .and_then(|(_, rest)| rest.split_once("== end\n"))
        .unwrap_or_else(|| panic!("no {path} in:\n{probe}"))
        .0
        .lines()
        .collect()
}

/// Each line of a file the probe printed, split at whitespace.
fn rows<'a>(probe: &'a str, path: &str) -> Vec<Vec<&'a str>> {
    file(probe, path)
        .iter()
        .map(|line| line.split_whitespace().collect())
        .collect()
}

fn hex(address: Ipv6Addr) -> String {
    address
        .octets()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn the_init_sets_up_the_link_and_its_frames_reach_one_host_socket() {
    let (probe, seen) = boot_linked();

    assert!(probe.contains("ipv4=10.0.2.15/24\n"), "{probe}");
    // `address index prefix scope flags name`, in hex. Scope 0 is global.
    // The kernel adds a link-local address of its own.
    let global: Vec<_> = rows(&probe, "/proc/net/if_inet6")
        .into_iter()
        .filter(|fields| fields[5] == net::INTERFACE && fields[3] == "00")
        .collect();
    assert_eq!(global.len(), 1, "{probe}");
    assert_eq!(
        (global[0][0], global[0][2]),
        (hex(net::IPV6).as_str(), "40")
    );

    // `Iface Destination Gateway Flags ...`, with addresses little-endian.
    let defaults: Vec<_> = rows(&probe, "/proc/net/route")
        .into_iter()
        .filter(|fields| fields[1] == "00000000")
        .collect();
    let gateway = format!("{:08X}", u32::from_le_bytes(net::IPV4_GATEWAY.octets()));
    assert_eq!(defaults.len(), 1, "{probe}");
    assert_eq!(
        (defaults[0][0], defaults[0][2]),
        (net::INTERFACE, gateway.as_str())
    );

    // `destination length source length next-hop metric refs use flags name`.
    let defaults: Vec<_> = rows(&probe, "/proc/net/ipv6_route")
        .into_iter()
        .filter(|fields| fields[0] == hex(Ipv6Addr::UNSPECIFIED) && fields[1] == "00")
        .filter(|fields| fields[9] != "lo")
        .collect();
    assert_eq!(defaults.len(), 1, "{probe}");
    assert_eq!(
        (defaults[0][9], defaults[0][4]),
        (net::INTERFACE, hex(net::IPV6_GATEWAY).as_str())
    );

    assert_eq!(
        file(&probe, "/etc/resolv.conf"),
        ["nameserver 10.0.2.2", "nameserver fd61:7000::2"]
    );

    let asked = (net::MAC, net::IPV4, net::IPV4_GATEWAY);
    assert!(seen.arp.contains(&asked), "{seen:?}");
    let solicited = (net::IPV6, net::IPV6_GATEWAY);
    assert!(seen.solicitations.contains(&solicited), "{seen:?}\n{probe}");
    assert!(probe.contains("ipv6_sent=sent\n"), "{probe}");
    // The guest learned the gateway's MAC address from a frame the peer sent,
    // and then the peer's answer reached the probe.
    assert_eq!(seen.datagrams, [b"ping".to_vec()]);
    assert!(probe.contains("answer=pong from 10.0.2.2:7\n"), "{probe}");
}

#[test]
fn a_guest_without_the_device_gets_no_link_and_no_resolver() {
    let kernel = fs::read(kernel_path()).unwrap();
    let init = fs::read(guest_binary::ap()).unwrap();
    let probe = fs::read(guest_binary::build("net-probe")).unwrap();
    let probe = boot_probe(Machine {
        kernel: &kernel,
        init: &init,
        probe: &probe,
        link: None,
        confine: false,
    });
    assert!(probe.contains("ipv4=No such device"), "{probe}");
    assert_eq!(rows(&probe, "/proc/net/if_inet6"), Vec::<Vec<&str>>::new());
    // Only the header line.
    assert_eq!(file(&probe, "/proc/net/route").len(), 1, "{probe}");
    let resolver = file(&probe, "/etc/resolv.conf");
    assert!(resolver[0].contains("No such file"), "{probe}");
}
