//! A static `/init` for T1: it reports ready, reads one request, answers with
//! output on both streams and a status, and powers off when the host hangs up.

use guest::linux::{Linux, connect_to_host};
use guest::protocol::{MAX_OUTPUT, PORT, ToGuest, ToHost};
use guest::{Channel, Event};
use std::io;

const STDOUT_BYTES: usize = 4 << 20;

struct Answer;

impl Channel for Answer {
    fn next_event(&mut self) -> io::Result<Event> {
        let mut host = connect_to_host(PORT)?;
        let failed = |error| io::Error::other(format!("{error:?}"));
        ToHost::Ready(b"test".to_vec())
            .write(&mut host)
            .map_err(failed)?;
        ToGuest::read(&mut host).map_err(failed)?;
        for _ in 0..STDOUT_BYTES / MAX_OUTPUT {
            ToHost::Stdout(vec![b'o'; MAX_OUTPUT])
                .write(&mut host)
                .map_err(failed)?;
        }
        ToHost::Stderr(b"to stderr".to_vec())
            .write(&mut host)
            .map_err(failed)?;
        ToHost::Exited(7).write(&mut host).map_err(failed)?;
        while ToGuest::read(&mut host).map_err(failed)?.is_some() {}
        Ok(Event::PowerOff)
    }
}

fn main() {
    let _ = guest::run(&mut Linux, &mut Answer);
}
