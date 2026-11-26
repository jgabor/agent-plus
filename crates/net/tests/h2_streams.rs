//! HTTP/2 streams and flow control, by RFC 9113. Each test of a refused
//! frame names the section that refuses it. The connection is the proxy's
//! side as a server, and each frame comes from the client.

use net::h2::{Connection, Error, Event, Frame, Refused};
use net::hpack::Header;

const MAX_CONCURRENT_STREAMS: u16 = 0x3;
const INITIAL_WINDOW_SIZE: u16 = 0x4;

/// The window of a new connection and its streams, by section 6.9.2.
const WINDOW: usize = 65_535;
const MAX_WINDOW: u32 = (1 << 31) - 1;

// Error codes, by section 7.
const PROTOCOL_ERROR: u32 = 0x1;
const FLOW_CONTROL_ERROR: u32 = 0x3;
const STREAM_CLOSED: u32 = 0x5;
const REFUSED_STREAM: u32 = 0x7;

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.into(),
        value: value.into(),
        never_indexed: false,
    }
}

fn request_headers() -> Vec<Header> {
    vec![
        header(":method", "POST"),
        header(":scheme", "https"),
        header(":authority", "example.test"),
        header(":path", "/"),
    ]
}

fn headers(stream: u32, end_stream: bool) -> Frame {
    Frame::Headers {
        stream,
        headers: request_headers(),
        priority: None,
        end_stream,
    }
}

fn data(stream: u32, length: usize, end_stream: bool) -> Frame {
    Frame::Data {
        stream,
        data: vec![b'd'; length],
        padding: None,
        end_stream,
    }
}

fn window_update(stream: u32, increment: u32) -> Frame {
    Frame::WindowUpdate { stream, increment }
}

fn reset(stream: u32) -> Frame {
    Frame::RstStream { stream, code: 0x8 }
}

fn priority(stream: u32) -> Frame {
    Frame::Priority {
        stream,
        priority: net::h2::Priority {
            exclusive: false,
            dependency: 0,
            weight: 15,
        },
    }
}

fn response(stream: u32, end_stream: bool) -> Frame {
    Frame::Headers {
        stream,
        headers: vec![header(":status", "200")],
        priority: None,
        end_stream,
    }
}

/// A connection that has accepted each of `frames`.
fn after(frames: &[Frame]) -> Connection {
    let mut connection = Connection::new();
    for frame in frames {
        connection.receive(frame.clone()).expect("accepted");
    }
    connection
}

fn stream_error(stream: u32, error: Error) -> Result<Option<Event>, Refused> {
    Err(Refused::Stream { stream, error })
}

fn connection_error(error: Error) -> Result<Option<Event>, Refused> {
    Err(Refused::Connection(error))
}

// Section 5.1, stream states.

#[test]
fn headers_open_a_stream_with_a_request_5_1() {
    let mut connection = Connection::new();
    assert_eq!(
        connection.receive(headers(1, false)),
        Ok(Some(Event::Request {
            stream: 1,
            headers: request_headers(),
            end_stream: false,
        }))
    );
    assert_eq!(
        connection.receive(data(1, 3, false)),
        Ok(Some(Event::Data {
            stream: 1,
            data: b"ddd".to_vec(),
            end_stream: false,
        }))
    );
    let trailers = vec![header("x-sum", "3")];
    let frame = Frame::Headers {
        stream: 1,
        headers: trailers.clone(),
        priority: None,
        end_stream: true,
    };
    assert_eq!(
        connection.receive(frame),
        Ok(Some(Event::Trailers {
            stream: 1,
            headers: trailers,
            end_stream: true,
        }))
    );
}

#[test]
fn a_frame_on_an_idle_stream_is_a_connection_error_5_1() {
    for frame in [data(1, 1, false), window_update(1, 1), reset(1)] {
        let mut connection = Connection::new();
        assert_eq!(
            connection.receive(frame.clone()),
            connection_error(Error::Idle),
            "{frame:?}"
        );
    }
    assert_eq!(Error::Idle.code(), PROTOCOL_ERROR);
    // A server's stream is idle too, because this server never pushes.
    let mut connection = after(&[headers(1, false)]);
    assert_eq!(
        connection.receive(data(2, 1, false)),
        connection_error(Error::Idle)
    );
}

#[test]
fn priority_on_an_idle_stream_opens_nothing_5_1() {
    let mut connection = Connection::new();
    assert_eq!(connection.receive(priority(5)), Ok(None));
    // Stream 5 is still idle, so stream 3 can open, and stream 5 after it.
    assert!(connection.receive(headers(3, false)).is_ok());
    assert!(connection.receive(headers(5, false)).is_ok());
}

#[test]
fn a_frame_after_end_stream_is_a_stream_error_5_1() {
    for frame in [data(1, 1, false), headers(1, true)] {
        let mut connection = after(&[headers(1, true)]);
        assert_eq!(
            connection.receive(frame.clone()),
            stream_error(1, Error::StreamClosed),
            "{frame:?}"
        );
        // The connection closed the stream, and discards what comes after.
        assert_eq!(connection.receive(data(1, 1, true)), Ok(None));
        assert_eq!(connection.receive(headers(3, false)).map(|_| ()), Ok(()));
    }
    assert_eq!(Error::StreamClosed.code(), STREAM_CLOSED);
}

#[test]
fn half_closed_remote_takes_window_update_priority_and_reset_5_1() {
    let mut connection = after(&[headers(1, true)]);
    assert_eq!(connection.receive(window_update(1, 10)), Ok(None));
    assert_eq!(connection.receive(priority(1)), Ok(None));
    assert_eq!(
        connection.receive(reset(1)),
        Ok(Some(Event::Reset {
            stream: 1,
            code: 0x8
        }))
    );
}

#[test]
fn data_on_a_closed_stream_is_a_connection_error_5_1() {
    // Closed by END_STREAM both ways.
    let mut connection = after(&[headers(1, true)]);
    assert!(connection.send(&response(1, true)));
    // Frames that may cross the one that closed the stream are ignored.
    assert_eq!(connection.receive(window_update(1, 10)), Ok(None));
    assert_eq!(connection.receive(reset(1)), Ok(None));
    assert_eq!(connection.receive(priority(1)), Ok(None));
    assert_eq!(
        connection.receive(data(1, 1, false)),
        connection_error(Error::StreamClosed)
    );
    // Closed by the client's RST_STREAM.
    let mut connection = after(&[headers(1, false), reset(1)]);
    assert_eq!(
        connection.receive(data(1, 1, false)),
        connection_error(Error::StreamClosed)
    );
}

#[test]
fn a_stream_the_server_reset_discards_what_follows_5_1() {
    let mut connection = after(&[headers(1, false)]);
    assert!(connection.send(&Frame::RstStream { stream: 1, code: 0 }));
    assert_eq!(connection.receive(data(1, 1000, false)), Ok(None));
    assert_eq!(connection.receive(headers(1, true)), Ok(None));
    assert_eq!(connection.receive(window_update(1, 10)), Ok(None));
    assert_eq!(connection.receive(reset(1)), Ok(None));
    // Its DATA still counts toward the connection's window, and the
    // connection gives that back itself, because nobody consumes it.
    assert_eq!(connection.receive(data(1, WINDOW - 1000, false)), Ok(None));
    assert_eq!(connection.updates(), vec![window_update(0, WINDOW as u32)]);
}

#[test]
fn the_client_resetting_a_stream_closes_it_5_1() {
    let mut connection = after(&[headers(1, false)]);
    assert_eq!(
        connection.receive(reset(1)),
        Ok(Some(Event::Reset {
            stream: 1,
            code: 0x8
        }))
    );
    assert_eq!(connection.window(1), 0);
    assert!(!connection.send(&response(1, false)));
}

#[test]
fn a_frame_for_a_stream_that_closed_is_not_sent_5_1() {
    let mut connection = after(&[headers(1, true), reset(1)]);
    assert!(!connection.send(&response(1, false)));
    assert!(!connection.send(&data(1, 0, true)));
    assert!(!connection.send(&Frame::RstStream { stream: 1, code: 0 }));
    // Nothing more once the server has ended the stream.
    let mut connection = after(&[headers(1, false)]);
    assert!(connection.send(&response(1, true)));
    assert!(!connection.send(&data(1, 0, true)));
    assert!(!connection.send(&response(1, true)));
    // An idle stream takes nothing.
    assert!(!connection.send(&response(3, false)));
    // An open stream takes them, and the last one closes it.
    let mut connection = after(&[headers(1, true)]);
    assert!(connection.send(&response(1, false)));
    assert!(connection.send(&data(1, 10, true)));
    assert!(!connection.send(&response(1, false)));
}

// Section 5.1.1, stream identifiers.

#[test]
fn an_even_stream_id_is_a_connection_error_5_1_1() {
    let mut connection = Connection::new();
    assert_eq!(
        connection.receive(headers(2, false)),
        connection_error(Error::StreamId)
    );
    assert_eq!(Error::StreamId.code(), PROTOCOL_ERROR);
}

#[test]
fn a_stream_id_not_above_the_last_is_a_connection_error_5_1_1() {
    // Below the last.
    let mut connection = after(&[headers(5, false)]);
    assert_eq!(
        connection.receive(headers(3, false)),
        connection_error(Error::StreamId)
    );
    // The last, once it closed.
    let mut connection = after(&[headers(1, true)]);
    assert!(connection.send(&response(1, true)));
    assert_eq!(
        connection.receive(headers(1, false)),
        connection_error(Error::StreamId)
    );
}

#[test]
fn a_skipped_stream_id_is_closed_5_1_1() {
    let mut connection = after(&[headers(5, false)]);
    assert_eq!(
        connection.receive(data(3, 1, false)),
        connection_error(Error::StreamClosed)
    );
}

// Section 5.1.2, stream concurrency.

#[test]
fn a_stream_past_max_concurrent_streams_is_refused_5_1_2() {
    let mut connection = after(&[headers(1, false)]);
    // The limit holds once the client acknowledges it.
    connection.apply(&[(MAX_CONCURRENT_STREAMS, 2)]);
    assert!(connection.receive(headers(3, true)).is_ok());
    assert_eq!(
        connection.receive(headers(5, false)),
        stream_error(5, Error::RefusedStream)
    );
    assert_eq!(Error::RefusedStream.code(), REFUSED_STREAM);
    // The refused stream is closed, and its DATA is discarded.
    assert_eq!(connection.receive(data(5, 10, true)), Ok(None));
    // A half-closed stream counts, until it closes.
    assert!(connection.send(&response(3, true)));
    assert!(connection.receive(headers(7, false)).is_ok());
    assert_eq!(
        connection.receive(headers(9, false)),
        stream_error(9, Error::RefusedStream)
    );
}

#[test]
fn there_is_no_stream_limit_until_one_is_acknowledged_5_1_2() {
    let mut connection = Connection::new();
    for stream in (1..400).step_by(2) {
        assert!(connection.receive(headers(stream, false)).is_ok());
    }
}

// Section 6.9, WINDOW_UPDATE.

#[test]
fn an_increment_of_0_is_an_error_6_9() {
    let mut connection = after(&[headers(1, false)]);
    assert_eq!(
        connection.receive(window_update(1, 0)),
        stream_error(1, Error::WindowUpdate)
    );
    assert_eq!(
        connection.receive(window_update(0, 0)),
        connection_error(Error::WindowUpdate)
    );
    assert_eq!(Error::WindowUpdate.code(), PROTOCOL_ERROR);
}

#[test]
fn a_window_past_2_to_the_31_is_an_error_6_9_1() {
    let mut connection = after(&[headers(1, false)]);
    let past = MAX_WINDOW - WINDOW as u32 + 1;
    assert_eq!(
        connection.receive(window_update(1, past)),
        stream_error(1, Error::FlowControl)
    );
    assert_eq!(
        connection.receive(window_update(0, past)),
        connection_error(Error::FlowControl)
    );
    assert_eq!(Error::FlowControl.code(), FLOW_CONTROL_ERROR);
    // Up to the maximum is fine.
    let mut connection = after(&[headers(1, false)]);
    let most = MAX_WINDOW - WINDOW as u32;
    assert_eq!(connection.receive(window_update(1, most)), Ok(None));
    assert_eq!(connection.receive(window_update(0, most)), Ok(None));
    assert_eq!(connection.window(1), MAX_WINDOW as usize);
}

#[test]
fn data_past_the_stream_window_is_a_stream_error_6_9_1() {
    let mut connection = after(&[headers(1, false), headers(3, false)]);
    connection.apply(&[(INITIAL_WINDOW_SIZE, 40_000)]);
    assert!(connection.receive(data(1, 39_990, false)).is_ok());
    assert_eq!(
        connection.receive(data(1, 11, false)),
        stream_error(1, Error::FlowControl)
    );
    // The refused DATA still took the connection's window, by section 6.9:
    // 25,534 bytes are left, so 25,535 on another stream are past it.
    assert_eq!(
        connection.receive(data(3, 25_535, false)),
        connection_error(Error::FlowControl)
    );
}

#[test]
fn data_past_the_connection_window_is_a_connection_error_6_9_1() {
    let mut connection = after(&[headers(1, false), headers(3, false)]);
    assert!(connection.receive(data(1, WINDOW - 5, false)).is_ok());
    assert_eq!(
        connection.receive(data(3, 6, false)),
        connection_error(Error::FlowControl)
    );
    // After a connection error, every frame is refused.
    assert_eq!(
        connection.receive(priority(3)),
        connection_error(Error::FlowControl)
    );
}

#[test]
fn padding_counts_toward_the_window_6_9_1() {
    let mut connection = after(&[headers(1, false)]);
    connection.apply(&[(INITIAL_WINDOW_SIZE, 1000)]);
    let padded = Frame::Data {
        stream: 1,
        data: vec![b'd'; 900],
        padding: Some(98),
        end_stream: false,
    };
    // 99 bytes of padding and Pad Length leave 1 byte.
    assert!(connection.receive(padded).is_ok());
    assert!(connection.receive(data(1, 1, false)).is_ok());
    assert_eq!(
        connection.receive(data(1, 1, false)),
        stream_error(1, Error::FlowControl)
    );
}

#[test]
fn data_takes_the_window_the_client_gave_6_9_1() {
    let mut connection = after(&[headers(1, false)]);
    assert_eq!(connection.window(1), WINDOW);
    assert!(connection.send(&response(1, false)));
    assert!(connection.send(&data(1, 1000, false)));
    assert_eq!(connection.window(1), WINDOW - 1000);
    // The connection's window is the smaller one now.
    assert!(connection.receive(window_update(1, 500)).is_ok());
    assert_eq!(connection.window(1), WINDOW - 1000);
    assert!(connection.receive(headers(3, false)).is_ok());
    assert_eq!(connection.window(3), WINDOW - 1000);
    assert!(connection.receive(window_update(0, 2000)).is_ok());
    assert_eq!(connection.window(1), WINDOW - 500);
    assert_eq!(connection.window(3), WINDOW);
}

#[test]
#[should_panic(expected = "window")]
fn data_past_the_clients_window_is_never_sent_6_9_1() {
    let mut connection = after(&[headers(1, false)]);
    assert!(connection.send(&response(1, false)));
    assert!(connection.send(&data(1, WINDOW + 1, false)));
}

#[test]
fn an_empty_data_with_end_stream_needs_no_window_6_9_1() {
    let mut connection = after(&[headers(1, true)]);
    assert!(connection.send(&response(1, false)));
    assert!(connection.send(&data(1, WINDOW, false)));
    assert_eq!(connection.window(1), 0);
    assert!(connection.send(&data(1, 0, true)));
}

#[test]
fn an_empty_data_with_end_stream_is_taken_past_the_window_6_9_1() {
    let mut connection = after(&[headers(1, false), data(1, 1000, false)]);
    // The window is 100 - 1000 now.
    connection.apply(&[(INITIAL_WINDOW_SIZE, 100)]);
    assert_eq!(
        connection.receive(data(1, 0, true)),
        Ok(Some(Event::Data {
            stream: 1,
            data: Vec::new(),
            end_stream: true,
        }))
    );
}

#[test]
fn consumed_data_comes_back_as_window_updates_6_9_1() {
    let mut connection = after(&[headers(1, false)]);
    assert!(connection.receive(data(1, 40_000, false)).is_ok());
    // Nothing is consumed yet.
    assert_eq!(connection.updates(), vec![]);
    // Under half the window is too small to send.
    connection.consumed(1, 30_000);
    assert_eq!(connection.updates(), vec![]);
    connection.consumed(1, 10_000);
    assert_eq!(
        connection.updates(),
        vec![window_update(0, 40_000), window_update(1, 40_000)]
    );
    assert_eq!(connection.updates(), vec![]);
    // The client can use what came back.
    assert!(connection.receive(data(1, WINDOW, false)).is_ok());
}

#[test]
fn padding_comes_back_without_being_consumed_6_9_1() {
    let mut connection = after(&[headers(1, false)]);
    let padded = Frame::Data {
        stream: 1,
        data: vec![b'd'; 100],
        padding: Some(255),
        end_stream: false,
    };
    // Padding and Pad Length take 256 bytes of each frame's 356.
    for _ in 0..100 {
        assert!(connection.receive(padded.clone()).is_ok());
    }
    assert_eq!(connection.updates(), vec![]);
    connection.consumed(1, 100 * 100);
    assert_eq!(
        connection.updates(),
        vec![window_update(0, 100 * 356), window_update(1, 100 * 356)]
    );
}

#[test]
fn a_stream_the_client_ended_gets_no_window_update_6_9_1() {
    let mut connection = after(&[headers(1, false)]);
    assert!(connection.receive(data(1, 40_000, true)).is_ok());
    connection.consumed(1, 40_000);
    assert_eq!(connection.updates(), vec![window_update(0, 40_000)]);
}

// Section 6.9.2, the initial window size.

#[test]
fn the_clients_initial_window_size_changes_each_open_stream_6_9_2() {
    let mut connection = after(&[headers(1, false)]);
    assert!(connection.send(&response(1, false)));
    assert!(connection.send(&data(1, 1000, false)));
    let settings = vec![(INITIAL_WINDOW_SIZE, 500)];
    assert_eq!(
        connection.receive(Frame::Settings(settings.clone())),
        Ok(Some(Event::Connection(Frame::Settings(settings))))
    );
    // The window is 500 - 1000 now, so nothing can be sent.
    assert_eq!(connection.window(1), 0);
    assert!(connection.receive(window_update(1, 600)).is_ok());
    assert_eq!(connection.window(1), 100);
    // A new stream starts with the new size.
    assert!(connection.receive(headers(3, false)).is_ok());
    assert_eq!(connection.window(3), 500);
}

#[test]
fn an_initial_window_size_that_takes_a_window_past_2_to_the_31_is_an_error_6_9_2() {
    let mut connection = after(&[headers(1, false), window_update(1, 1000)]);
    let settings = Frame::Settings(vec![(INITIAL_WINDOW_SIZE, MAX_WINDOW - 999)]);
    assert_eq!(
        connection.receive(settings),
        connection_error(Error::FlowControl)
    );
    // Up to the maximum is fine.
    let mut connection = after(&[headers(1, false), window_update(1, 1000)]);
    let settings = Frame::Settings(vec![(INITIAL_WINDOW_SIZE, MAX_WINDOW - 1000)]);
    assert!(connection.receive(settings).is_ok());
}

#[test]
fn our_initial_window_size_changes_each_open_stream_once_acknowledged_6_9_2() {
    let mut connection = after(&[headers(1, false)]);
    assert!(connection.receive(data(1, 1000, false)).is_ok());
    connection.apply(&[(INITIAL_WINDOW_SIZE, 1500)]);
    // 500 bytes are left on stream 1.
    assert_eq!(
        connection.receive(data(1, 501, false)),
        stream_error(1, Error::FlowControl)
    );
    // A new stream starts with the new size.
    assert!(connection.receive(headers(3, false)).is_ok());
    assert!(connection.receive(data(3, 1500, false)).is_ok());
    // A smaller size can leave a window below 0, so the client waits.
    assert!(connection.receive(headers(5, false)).is_ok());
    assert!(connection.receive(data(5, 1000, false)).is_ok());
    connection.apply(&[(INITIAL_WINDOW_SIZE, 100)]);
    assert_eq!(
        connection.receive(data(5, 1, false)),
        stream_error(5, Error::FlowControl)
    );
    // Half the new size is enough to send an update.
    assert!(connection.receive(headers(7, false)).is_ok());
    assert!(connection.receive(data(7, 60, false)).is_ok());
    connection.consumed(7, 60);
    assert!(connection.updates().contains(&window_update(7, 60)));
}

// A connection that watches frames pass to a server that is another
// program.

/// A watching connection that has accepted each of `frames`.
fn watched(frames: &[Frame]) -> Connection {
    let mut connection = Connection::watching();
    for frame in frames {
        connection.receive(frame.clone()).expect("accepted");
    }
    connection
}

#[test]
fn a_watching_connection_counts_no_window() {
    // DATA past every window the server started with, which only the
    // server knows it opened.
    let mut connection = watched(&[headers(1, false)]);
    assert_eq!(
        connection.receive(data(1, 4 * WINDOW, true)),
        Ok(Some(Event::Data {
            stream: 1,
            data: vec![b'd'; 4 * WINDOW],
            end_stream: true,
        }))
    );
    // Updates past 2^31 - 1, since what the server sent passes unseen.
    let mut connection = watched(&[headers(1, false)]);
    for stream in [0, 1] {
        assert_eq!(
            connection.receive(window_update(stream, MAX_WINDOW)),
            Ok(None)
        );
        assert_eq!(
            connection.receive(window_update(stream, MAX_WINDOW)),
            Ok(None)
        );
    }
    let settings = Frame::Settings(vec![(INITIAL_WINDOW_SIZE, MAX_WINDOW)]);
    assert!(connection.receive(settings).is_ok());
    assert!(connection.updates().is_empty());
}

#[test]
fn a_watching_connection_still_refuses_an_update_of_0_6_9() {
    let mut connection = watched(&[headers(1, false)]);
    assert_eq!(
        connection.receive(window_update(1, 0)),
        stream_error(1, Error::WindowUpdate)
    );
    assert_eq!(
        connection.receive(window_update(0, 0)),
        connection_error(Error::WindowUpdate)
    );
}

#[test]
fn a_watched_stream_closes_once_the_client_ends_it() {
    let mut connection = watched(&[headers(1, false), data(1, 3, true)]);
    // The client can still give the server window for its answer.
    assert_eq!(connection.receive(window_update(1, 100)), Ok(None));
    assert_eq!(connection.receive(reset(1)), Ok(None));
    assert_eq!(
        connection.receive(data(1, 1, false)),
        connection_error(Error::StreamClosed)
    );
    // A request that ends with its headers closes too.
    let mut connection = watched(&[headers(1, true)]);
    assert_eq!(
        connection.receive(headers(1, true)),
        connection_error(Error::StreamId)
    );
}
