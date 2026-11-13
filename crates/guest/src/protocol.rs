//! The messages between `ap` on the host and the agent in the guest.
//!
//! A frame is a body length as a little-endian `u32`, then the body: one tag
//! byte and a payload. Each direction has its own type and its own tags, so a
//! request can only travel to the guest. [`ToHost`] reports what happened, and
//! has no way to ask the host for anything.

use std::io::{self, Read, Write};

/// The largest body either side reads or writes.
pub const MAX_BODY: usize = 64 * 1024;
/// The most output one frame carries. Half a body leaves room for the tag.
pub const MAX_OUTPUT: usize = MAX_BODY / 2;
/// The vsock port the init connects to on the host.
pub const PORT: u32 = 1024;
/// The vsock port the agent connects to on the host, once for each command
/// it may be asked to run beside the harness.
pub const EXEC_PORT: u32 = 1025;

const RUN: u8 = 1;
const STDOUT: u8 = 2;
const STDERR: u8 = 3;
const EXITED: u8 = 4;
const READY: u8 = 5;
const START: u8 = 6;
const FAILED: u8 = 7;
const START_IN_TERMINAL: u8 = 8;
const INPUT: u8 = 9;
const RESIZE: u8 = 10;
const EXEC: u8 = 11;
const EXEC_IN_TERMINAL: u8 = 12;
const END_OF_INPUT: u8 = 13;

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
    /// Starts the harness as the user `agent`, as [`crate::agent`] says.
    Start(Start),
    /// Runs a command beside the harness, as `agent`, with the harness's
    /// environment and directory.
    Exec(Exec),
    /// Bytes for the program's input: typed into its terminal, if it has
    /// one.
    Input(Vec<u8>),
    /// The end of a command's input. A program in a terminal ignores it.
    EndOfInput,
    /// The new size of the program's terminal.
    Resize(Size),
}

/// A command to run beside the harness. Arguments can't contain a NUL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// The command, and its arguments.
    pub argv: Vec<Vec<u8>>,
    /// The size of the terminal the command runs in. Without one, it runs
    /// with pipes.
    pub terminal: Option<Size>,
}

/// A terminal's size, in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub columns: u16,
}

impl Size {
    fn to_bytes(self) -> [u8; 4] {
        let [a, b] = self.rows.to_le_bytes();
        let [c, d] = self.columns.to_le_bytes();
        [a, b, c, d]
    }

    fn from_bytes(bytes: &[u8; 4]) -> Size {
        Size {
            rows: u16::from_le_bytes([bytes[0], bytes[1]]),
            columns: u16::from_le_bytes([bytes[2], bytes[3]]),
        }
    }
}

/// How to start the harness. No value holds a NUL, because each one reaches
/// `execve` or `chdir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    /// The harness and its arguments, without the prompt.
    pub argv: Vec<Vec<u8>>,
    /// The harness's whole environment. A key is not empty and has no `=`.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    /// The directory the harness starts in.
    pub directory: Vec<u8>,
    /// The prompt, which follows the last argument. `None` takes the one the
    /// agent was built with.
    pub prompt: Option<Vec<u8>>,
    /// The size of the terminal the harness starts in. Without one, it
    /// starts with pipes.
    pub terminal: Option<Size>,
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
    /// The harness couldn't start, for this reason.
    Failed(Vec<u8>),
}

impl ToGuest {
    /// Reads one message, or `None` if the stream ended between frames.
    pub fn read(reader: &mut impl Read) -> Result<Option<ToGuest>, ProtocolError> {
        let Some(body) = read_body(reader)? else {
            return Ok(None);
        };
        let (tag, payload) = body.split_first().ok_or(ProtocolError::Malformed)?;
        match *tag {
            RUN => Ok(Some(ToGuest::Run {
                argv: read_argv(payload)?,
            })),
            EXEC => Ok(Some(ToGuest::Exec(Exec {
                argv: read_argv(payload)?,
                terminal: None,
            }))),
            EXEC_IN_TERMINAL => {
                let (size, rest) = payload
                    .split_first_chunk::<4>()
                    .ok_or(ProtocolError::Malformed)?;
                Ok(Some(ToGuest::Exec(Exec {
                    argv: read_argv(rest)?,
                    terminal: Some(Size::from_bytes(size)),
                })))
            }
            END_OF_INPUT if payload.is_empty() => Ok(Some(ToGuest::EndOfInput)),
            END_OF_INPUT => Err(ProtocolError::Malformed),
            START => Ok(Some(ToGuest::Start(read_start(payload, None)?))),
            START_IN_TERMINAL => {
                let (size, rest) = payload
                    .split_first_chunk::<4>()
                    .ok_or(ProtocolError::Malformed)?;
                let start = read_start(rest, Some(Size::from_bytes(size)))?;
                Ok(Some(ToGuest::Start(start)))
            }
            INPUT => Ok(Some(ToGuest::Input(payload.to_vec()))),
            RESIZE => match payload.try_into() {
                Ok(size) => Ok(Some(ToGuest::Resize(Size::from_bytes(size)))),
                Err(_) => Err(ProtocolError::Malformed),
            },
            tag => Err(ProtocolError::UnknownTag(tag)),
        }
    }

    pub fn write(&self, writer: &mut impl Write) -> Result<(), ProtocolError> {
        match self {
            ToGuest::Run { argv } => write_frame(writer, RUN, &argv_payload(argv)?),
            ToGuest::Exec(exec) => match exec.terminal {
                None => write_frame(writer, EXEC, &argv_payload(&exec.argv)?),
                Some(size) => {
                    let payload = [&size.to_bytes()[..], &argv_payload(&exec.argv)?].concat();
                    write_frame(writer, EXEC_IN_TERMINAL, &payload)
                }
            },
            ToGuest::EndOfInput => write_frame(writer, END_OF_INPUT, &[]),
            ToGuest::Start(start) => match start.terminal {
                None => write_frame(writer, START, &start_payload(start)?),
                Some(size) => {
                    let payload = [&size.to_bytes()[..], &start_payload(start)?].concat();
                    write_frame(writer, START_IN_TERMINAL, &payload)
                }
            },
            ToGuest::Input(bytes) => write_frame(writer, INPUT, bytes),
            ToGuest::Resize(size) => write_frame(writer, RESIZE, &size.to_bytes()),
        }
    }
}

/// Arguments, each ending with a NUL. There is at least one.
fn argv_payload(argv: &[Vec<u8>]) -> Result<Vec<u8>, ProtocolError> {
    if argv.is_empty() || argv.iter().any(|arg| arg.contains(&0)) {
        return Err(ProtocolError::Malformed);
    }
    let mut payload = Vec::new();
    for arg in argv {
        payload.extend_from_slice(arg);
        payload.push(0);
    }
    Ok(payload)
}

fn read_argv(payload: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
    let args = payload.strip_suffix(&[0]).ok_or(ProtocolError::Malformed)?;
    Ok(args.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect())
}

/// A start's payload: the number of arguments and the number of variables,
/// each a little-endian `u32`, then the arguments, the variables as
/// `key=value`, the directory, and the prompt if there is one. Every string
/// ends with a NUL. A start in a terminal has its own tag, and the terminal's
/// rows and columns, each a little-endian `u16`, before the payload.
fn start_payload(start: &Start) -> Result<Vec<u8>, ProtocolError> {
    if start.argv.is_empty() || start.env.iter().any(|(key, _)| !is_key(key)) {
        return Err(ProtocolError::Malformed);
    }
    let count = |n: usize| u32::try_from(n).map_err(|_| ProtocolError::Malformed);
    let mut payload = Vec::new();
    payload.extend_from_slice(&count(start.argv.len())?.to_le_bytes());
    payload.extend_from_slice(&count(start.env.len())?.to_le_bytes());
    let variables = start
        .env
        .iter()
        .map(|(key, value)| [key.as_slice(), b"=", value].concat());
    let strings = (start.argv.iter().cloned())
        .chain(variables)
        .chain([start.directory.clone()])
        .chain(start.prompt.clone());
    for text in strings {
        if text.contains(&0) {
            return Err(ProtocolError::Malformed);
        }
        payload.extend_from_slice(&text);
        payload.push(0);
    }
    Ok(payload)
}

fn read_start(payload: &[u8], terminal: Option<Size>) -> Result<Start, ProtocolError> {
    let (argc, rest) = payload
        .split_first_chunk::<4>()
        .ok_or(ProtocolError::Malformed)?;
    let (envc, rest) = rest
        .split_first_chunk::<4>()
        .ok_or(ProtocolError::Malformed)?;
    let (argc, envc) = (u32::from_le_bytes(*argc), u32::from_le_bytes(*envc));
    let strings = rest.strip_suffix(&[0]).ok_or(ProtocolError::Malformed)?;
    let mut strings = strings.split(|byte| *byte == 0).map(<[u8]>::to_vec);
    // The counts come from the other side, so nothing is reserved for them.
    let argv: Vec<Vec<u8>> = strings.by_ref().take(argc as usize).collect();
    let mut env = Vec::new();
    for variable in strings.by_ref().take(envc as usize) {
        let at = variable.iter().position(|byte| *byte == b'=');
        let at = at.ok_or(ProtocolError::Malformed)?;
        env.push((variable[..at].to_vec(), variable[at + 1..].to_vec()));
    }
    let directory = strings.next().ok_or(ProtocolError::Malformed)?;
    let prompt = strings.next();
    let complete = argv.len() == argc as usize && env.len() == envc as usize;
    if !complete || argv.is_empty() || strings.next().is_some() {
        return Err(ProtocolError::Malformed);
    }
    if env.iter().any(|(key, _)| !is_key(key)) {
        return Err(ProtocolError::Malformed);
    }
    Ok(Start {
        argv,
        env,
        directory,
        prompt,
        terminal,
    })
}

fn is_key(key: &[u8]) -> bool {
    !key.is_empty() && !key.contains(&b'=')
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
            (FAILED, reason) => Ok(Some(ToHost::Failed(reason.to_vec()))),
            (tag, _) => Err(ProtocolError::UnknownTag(tag)),
        }
    }

    pub fn write(&self, writer: &mut impl Write) -> Result<(), ProtocolError> {
        match self {
            ToHost::Stdout(output) => write_frame(writer, STDOUT, output),
            ToHost::Stderr(output) => write_frame(writer, STDERR, output),
            ToHost::Exited(status) => write_frame(writer, EXITED, &[*status]),
            ToHost::Ready(release) => write_frame(writer, READY, release),
            ToHost::Failed(reason) => write_frame(writer, FAILED, reason),
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
