//! The messages between `ap` on the host and the agent in the guest.
//!
//! A frame is a body length as a little-endian `u32`, then the body: one tag
//! byte and a payload. Each direction has its own type and its own tags, so a
//! request can only travel to the guest. [`ToHost`] reports what happened, and
//! has no way to ask the host for anything.

use std::io::{self, Read, Write};

/// The largest body either side reads or writes.
pub const MAX_BODY: usize = 64 * 1024;
/// The most output one frame carries.
pub const MAX_OUTPUT: usize = 32 * 1024;
/// The vsock port the init connects to on the host.
pub const PORT: u32 = 1024;

const RUN: u8 = 1;
const STDOUT: u8 = 2;
const STDERR: u8 = 3;
const EXITED: u8 = 4;
const READY: u8 = 5;

/// Why a frame was refused. The stream can't be trusted after any of these.
#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    TooLarge(usize),
    UnknownTag(u8),
    Malformed,
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> ProtocolError {
        ProtocolError::Io(error)
    }
}

/// What the host asks of the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToGuest {
    /// Runs a program. The first argument names it. Arguments are bytes,
    /// because Linux's are, and can't contain a NUL.
    Run { argv: Vec<Vec<u8>> },
}

/// What the guest tells the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToHost {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The program ended with this status: its exit code, 126 if it couldn't
    /// be executed, 127 if it wasn't found, or 128+n for signal n.
    Exited(u8),
    /// The init is up, on a kernel with this release.
    Ready(Vec<u8>),
}

impl ToGuest {
    /// Reads one message, or `None` if the stream ended between frames.
    pub fn read(reader: &mut impl Read) -> Result<Option<ToGuest>, ProtocolError> {
        let Some(body) = read_body(reader)? else {
            return Ok(None);
        };
        let (tag, payload) = body.split_first().ok_or(ProtocolError::Malformed)?;
        match *tag {
            RUN => {
                let args = payload.strip_suffix(&[0]).ok_or(ProtocolError::Malformed)?;
                let argv = args.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();
                Ok(Some(ToGuest::Run { argv }))
            }
            tag => Err(ProtocolError::UnknownTag(tag)),
        }
    }

    pub fn write(&self, writer: &mut impl Write) -> Result<(), ProtocolError> {
        let ToGuest::Run { argv } = self;
        let unsendable = |arg: &Vec<u8>| arg.contains(&0);
        if argv.is_empty() || argv.iter().any(unsendable) {
            return Err(ProtocolError::Malformed);
        }
        let mut payload = Vec::new();
        for arg in argv {
            payload.extend_from_slice(arg);
            payload.push(0);
        }
        write_frame(writer, RUN, &payload)
    }
}

impl ToHost {
    /// Reads one message, or `None` if the stream ended between frames.
    pub fn read(reader: &mut impl Read) -> Result<Option<ToHost>, ProtocolError> {
        let Some(body) = read_body(reader)? else {
            return Ok(None);
        };
        let (tag, payload) = body.split_first().ok_or(ProtocolError::Malformed)?;
        match (*tag, payload) {
            (STDOUT, output) => Ok(Some(ToHost::Stdout(output.to_vec()))),
            (STDERR, output) => Ok(Some(ToHost::Stderr(output.to_vec()))),
            (EXITED, [status]) => Ok(Some(ToHost::Exited(*status))),
            (EXITED, _) => Err(ProtocolError::Malformed),
            (READY, release) => Ok(Some(ToHost::Ready(release.to_vec()))),
            (tag, _) => Err(ProtocolError::UnknownTag(tag)),
        }
    }

    pub fn write(&self, writer: &mut impl Write) -> Result<(), ProtocolError> {
        match self {
            ToHost::Stdout(output) => write_frame(writer, STDOUT, output),
            ToHost::Stderr(output) => write_frame(writer, STDERR, output),
            ToHost::Exited(status) => write_frame(writer, EXITED, &[*status]),
            ToHost::Ready(release) => write_frame(writer, READY, release),
        }
    }
}

/// The length is checked before anything is allocated for the body.
fn read_body(reader: &mut impl Read) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut header = [0; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_BODY {
        return Err(ProtocolError::TooLarge(length));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_frame(writer: &mut impl Write, tag: u8, payload: &[u8]) -> Result<(), ProtocolError> {
    let length = payload.len() + 1;
    if length > MAX_BODY {
        return Err(ProtocolError::TooLarge(length));
    }
    let mut frame = Vec::with_capacity(length + 4);
    frame.extend_from_slice(&(length as u32).to_le_bytes());
    frame.push(tag);
    frame.extend_from_slice(payload);
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}
