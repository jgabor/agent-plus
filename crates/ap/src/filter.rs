//! Removes what guest output must not do to the user's terminal.
//!
//! The guest chooses every byte, so this reads them the way a terminal does:
//! in UTF-8, with 7-bit and 8-bit introducers, and with control characters
//! allowed inside a sequence. It removes clipboard access (OSC 52 and 5522),
//! the title reports (CSI 20 t and CSI 21 t), and DECRQSS, whose replies can
//! echo what the guest sent.
//!
//! A sequence is held back until it can be judged, and nothing unfinished is
//! written, so neither the other stream nor the next prompt can complete it.
//! Once a string sequence passes, its body is read as ordinary output. A
//! terminal ignores that body, and anything that can end the string can start
//! a new sequence, which is judged here the same way. Removing that sequence
//! also removes what would have ended the string, so the filter ends it.

const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ESC: u8 = 0x1b;
const DEL: u8 = 0x7f;
const C1_DCS: u8 = 0x90;
const C1_CSI: u8 = 0x9b;
const C1_ST: u8 = 0x9c;
const C1_OSC: u8 = 0x9d;
/// More than any real introducer needs. Longer sequences are removed, so a
/// guest can't make the host hold an unbounded amount.
const MAX_HELD: usize = 256;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Escape,
    Csi,
    Osc,
    Dcs,
    /// Inside a removed string, until whatever ends it.
    Removing,
    /// Saw ESC while removing. `\` ends the string, and anything else starts
    /// a new escape.
    RemovingEscape,
}

/// The filter for one stream of output. State carries across calls, because
/// a sequence can span frames.
pub struct Filter {
    state: State,
    held: Vec<u8>,
    overflowed: bool,
    /// The terminal is inside a string that passed and hasn't ended.
    string_open: bool,
    /// UTF-8 continuation bytes still expected.
    continuation: u8,
    /// Saw `0xc2`, which either starts a character or encodes a C1 control.
    after_c2: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            state: State::Ground,
            held: Vec::new(),
            overflowed: false,
            string_open: false,
            continuation: 0,
            after_c2: false,
        }
    }
}

impl Filter {
    /// Takes the next bytes from the guest, and returns what may be written.
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &byte in input {
            self.decode(byte, &mut out);
        }
        out
    }

    fn decode(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.after_c2 {
            self.after_c2 = false;
            if (0x80..=0x9f).contains(&byte) {
                self.control1(byte, &[0xc2, byte], out);
                return;
            }
            self.byte(0xc2, out);
        }
        if self.continuation > 0 {
            if byte & 0xc0 == 0x80 {
                self.continuation -= 1;
                self.byte(byte, out);
                return;
            }
            self.continuation = 0;
        }
        match byte {
            0xc2 => self.after_c2 = true,
            0x80..=0x9f => self.control1(byte, &[byte], out),
            _ => {
                self.continuation = match byte {
                    0xc3..=0xdf => 1,
                    0xe0..=0xef => 2,
                    0xf0..=0xf4 => 3,
                    _ => 0,
                };
                self.byte(byte, out);
            }
        }
    }

    /// An 8-bit control, `raw` as it arrived. It interrupts any sequence.
    fn control1(&mut self, control: u8, raw: &[u8], out: &mut Vec<u8>) {
        let removing = matches!(self.state, State::Removing | State::RemovingEscape);
        self.discard();
        match control {
            C1_CSI => self.begin(State::Csi, raw),
            C1_OSC => self.begin(State::Osc, raw),
            C1_DCS => self.begin(State::Dcs, raw),
            C1_ST if removing => {}
            _ => {
                self.string_open = false;
                out.extend_from_slice(raw);
            }
        }
    }

    fn byte(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.state == State::RemovingEscape {
            self.state = State::Ground;
            if byte == b'\\' {
                return;
            }
            self.begin(State::Escape, &[ESC]);
        }
        if byte == ESC {
            if matches!(self.state, State::Removing) {
                self.state = State::RemovingEscape;
            } else {
                self.discard();
                self.begin(State::Escape, &[ESC]);
            }
            return;
        }
        if byte == CAN || byte == SUB {
            if self.state != State::Removing {
                out.push(byte);
                self.string_open = false;
            }
            self.discard();
            return;
        }
        match self.state {
            State::Ground => {
                if byte == BEL {
                    self.string_open = false;
                }
                out.push(byte);
            }
            State::Escape => self.escape(byte, out),
            State::Csi => self.csi(byte, out),
            State::Osc => self.osc(byte, out),
            State::Dcs => self.dcs(byte, out),
            State::Removing => {
                if byte == BEL {
                    self.state = State::Ground;
                }
            }
            State::RemovingEscape => unreachable!("handled above"),
        }
    }

    fn escape(&mut self, byte: u8, out: &mut Vec<u8>) {
        if is_ignored_inside(byte) {
            if !self.hold(byte) {
                self.discard();
            }
            return;
        }
        self.held.push(byte);
        match byte {
            b'[' => self.state = State::Csi,
            b']' => self.state = State::Osc,
            b'P' => self.state = State::Dcs,
            _ => self.pass(out),
        }
    }

    fn csi(&mut self, byte: u8, out: &mut Vec<u8>) {
        match byte {
            0x40..=0x7e => {
                self.held.push(byte);
                if self.overflowed || is_title_report(&self.held) {
                    self.close_string(out);
                    self.discard();
                } else {
                    self.pass(out);
                }
            }
            0x80.. => {
                self.discard();
                out.push(byte);
            }
            _ => {
                if !self.hold(byte) {
                    self.held.clear();
                    self.overflowed = true;
                }
            }
        }
    }

    fn osc(&mut self, byte: u8, out: &mut Vec<u8>) {
        if byte != BEL && (is_ignored_inside(byte) || byte.is_ascii_digit()) {
            if !self.hold(byte) {
                self.remove(out);
            }
            return;
        }
        if !is_clipboard(&self.held) {
            self.held.push(byte);
            self.pass(out);
        } else if byte == BEL {
            self.close_string(out);
            self.discard();
        } else {
            self.remove(out);
        }
    }

    fn dcs(&mut self, byte: u8, out: &mut Vec<u8>) {
        match byte {
            0x40..=0x7e => {
                if byte == b'q' && intermediates(&self.held) == b"$" {
                    self.remove(out);
                } else {
                    self.held.push(byte);
                    self.pass(out);
                }
            }
            0x80.. => {
                self.discard();
                out.push(byte);
            }
            _ => {
                if !self.hold(byte) {
                    self.remove(out);
                }
            }
        }
    }

    fn begin(&mut self, state: State, introducer: &[u8]) {
        self.state = state;
        self.held.extend_from_slice(introducer);
    }

    /// Keeps a byte of the sequence, or returns false if there is no room.
    fn hold(&mut self, byte: u8) -> bool {
        if self.held.len() == MAX_HELD {
            return false;
        }
        self.held.push(byte);
        true
    }

    fn pass(&mut self, out: &mut Vec<u8>) {
        self.string_open =
            matches!(self.state, State::Osc | State::Dcs) && self.held.last() != Some(&BEL);
        out.append(&mut self.held);
        self.state = State::Ground;
    }

    fn discard(&mut self) {
        self.held.clear();
        self.overflowed = false;
        self.state = State::Ground;
    }

    fn close_string(&mut self, out: &mut Vec<u8>) {
        if self.string_open {
            self.string_open = false;
            out.extend_from_slice(&[ESC, b'\\']);
        }
    }

    fn remove(&mut self, out: &mut Vec<u8>) {
        self.close_string(out);
        self.discard();
        self.state = State::Removing;
    }
}

/// C0 controls and DEL, which a terminal acts on or ignores without leaving
/// the sequence.
fn is_ignored_inside(byte: u8) -> bool {
    byte < 0x20 || byte == DEL
}

/// Real codes have no leading zeros. Terminals disagree on how to read them,
/// some taking a fixed number of digits, so any such code is removed.
fn is_clipboard(held: &[u8]) -> bool {
    let digits: Vec<u8> = held.iter().copied().filter(u8::is_ascii_digit).collect();
    digits == b"52" || digits == b"5522" || (digits.len() > 1 && digits[0] == b'0')
}

fn is_title_report(held: &[u8]) -> bool {
    let (final_byte, body) = held.split_last().expect("a final byte was just held");
    if *final_byte != b't' || !intermediates(body).is_empty() {
        return false;
    }
    let params: Vec<u8> = body
        .iter()
        .copied()
        .filter(|byte| (0x30..=0x3f).contains(byte))
        .collect();
    let first = params.split(|byte| matches!(byte, b';' | b':')).next();
    matches!(first.map(trim_zeros), Some(b"20" | b"21"))
}

fn intermediates(held: &[u8]) -> Vec<u8> {
    held.iter()
        .copied()
        .filter(|byte| (0x20..=0x2f).contains(byte))
        .collect()
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let start = digits.iter().take_while(|byte| **byte == b'0').count();
    &digits[start..]
}
