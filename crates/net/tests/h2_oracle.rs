//! The `h2` crate is the oracle for the proxy's own HTTP/2. A test can
//! drive an `h2` client and server over an in-memory pipe, so it can put our
//! code on either end.

use std::collections::BTreeMap;

use http::Request;
use net::h2::{Connection, Event, Frame, Reader, Writer};
use net::hpack::Header;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

#[test]
fn two_requests_on_one_connection_reach_the_server() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let seen = runtime.block_on(async {
        let (client_io, server_io) = duplex(64 * 1024);

        let server = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server_io)
                .await
                .expect("server handshake");
            let mut seen = Vec::new();
            while let Some(stream) = connection.accept().await {
                let (request, mut respond) = stream.expect("accept stream");
                let authority = request.uri().authority().map(|a| a.as_str().to_owned());
                let path = request
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str().to_owned());
                seen.push((respond.stream_id().as_u32(), authority, path));
                let response = http::Response::new(());
                respond
                    .send_response(response, true)
                    .expect("send response");
            }
            seen
        });

        let (client, connection) = h2::client::handshake(client_io)
            .await
            .expect("client handshake");
        let driver = tokio::spawn(async move {
            connection.await.expect("client connection");
        });

        let mut client = client.ready().await.expect("client ready");
        let mut responses = Vec::new();
        for path in ["/one", "/two?x=1"] {
            let request = Request::get(format!("https://example.test{path}"))
                .body(())
                .expect("request");
            let (response, _) = client.send_request(request, true).expect("send request");
            responses.push(response);
            client = client.ready().await.expect("client ready");
        }
        for response in responses {
            let response = response.await.expect("response");
            assert_eq!(response.status(), http::StatusCode::OK);
        }
        drop(client);
        driver.await.expect("client driver");
        server.await.expect("server")
    });

    assert_eq!(
        seen,
        [
            (1, Some("example.test".to_owned()), Some("/one".to_owned())),
            (
                3,
                Some("example.test".to_owned()),
                Some("/two?x=1".to_owned())
            ),
        ]
    );
}

const MAX_CONCURRENT_STREAMS: u16 = 0x3;
const LIST_LIMIT: usize = 1 << 20;
const MAX_FRAME_SIZE: usize = 1 << 14;
/// Bodies longer than the initial window of 65,535, each way.
const BODY: usize = 200_000;
const REQUESTS: usize = 5;
/// The client's SETTINGS_INITIAL_WINDOW_SIZE, which the server's DATA
/// keeps to.
const CLIENT_WINDOW: u32 = 30_000;

/// What the server read of one request.
#[derive(Debug)]
struct Seen {
    stream: u32,
    path: Vec<u8>,
    body: Vec<u8>,
}

/// A server of our `Reader`, `Writer`, and `Connection`, which answers
/// each request with its own body. With `plant`, it never applies the
/// client's SETTINGS_INITIAL_WINDOW_SIZE, so its DATA is past the client's
/// window.
async fn echo_server(mut io: DuplexStream, plant: bool) -> Vec<Seen> {
    let mut reader = Reader::client(LIST_LIMIT);
    let mut writer = Writer::new();
    let mut connection = Connection::new();
    let ours = vec![(MAX_CONCURRENT_STREAMS, 2)];
    let mut out = Vec::new();
    writer.write(&Frame::Settings(ours.clone()), &mut out);
    io.write_all(&out).await.expect("write");
    let mut seen: Vec<Seen> = Vec::new();
    // What is left to send of each answer.
    let mut answers: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let count = io.read(&mut buffer).await.expect("read");
        if count == 0 {
            break;
        }
        reader.push(&buffer[..count]);
        let mut out = Vec::new();
        while let Some(frame) = reader.frame().expect("our reader") {
            let frame = match frame {
                Frame::Settings(settings) if plant => Frame::Settings(
                    (settings.into_iter())
                        .filter(|&(id, _)| id != 0x4)
                        .collect(),
                ),
                frame => frame,
            };
            match connection.receive(frame).expect("our connection") {
                Some(Event::Request {
                    stream,
                    headers,
                    end_stream,
                }) => {
                    let path = headers.iter().find(|header| header.name == b":path");
                    seen.push(Seen {
                        stream,
                        path: path.expect(":path").value.clone(),
                        body: Vec::new(),
                    });
                    assert!(!end_stream);
                }
                Some(Event::Data {
                    stream,
                    data,
                    end_stream,
                }) => {
                    let request = seen.iter_mut().find(|seen| seen.stream == stream);
                    let request = request.expect("a request");
                    request.body.extend(&data);
                    connection.consumed(stream, data.len());
                    if end_stream {
                        let response = Frame::Headers {
                            stream,
                            headers: vec![Header {
                                name: b":status".to_vec(),
                                value: b"200".to_vec(),
                                never_indexed: false,
                            }],
                            priority: None,
                            end_stream: false,
                        };
                        writer.write(&response, &mut out);
                        connection.send(&response);
                        answers.insert(stream, request.body.clone());
                    }
                }
                Some(Event::Reset { stream, .. }) => {
                    answers.remove(&stream);
                }
                Some(Event::Connection(Frame::Settings(settings))) => {
                    writer.write(&Frame::SettingsAck, &mut out);
                    writer.apply(&settings);
                }
                Some(Event::Connection(Frame::SettingsAck)) => {
                    reader.apply(&ours);
                    connection.apply(&ours);
                }
                Some(Event::Connection(Frame::Ping { ack: false, data })) => {
                    writer.write(&Frame::Ping { ack: true, data }, &mut out);
                }
                _ => {}
            }
        }
        for update in connection.updates() {
            writer.write(&update, &mut out);
        }
        for (&stream, rest) in &mut answers {
            loop {
                let length = rest.len().min(MAX_FRAME_SIZE);
                let length = length.min(connection.window(stream));
                if length == 0 && !rest.is_empty() {
                    break;
                }
                let data = Frame::Data {
                    stream,
                    data: rest.drain(..length).collect(),
                    padding: None,
                    end_stream: rest.is_empty(),
                };
                writer.write(&data, &mut out);
                connection.send(&data);
                if rest.is_empty() {
                    break;
                }
            }
        }
        answers.retain(|_, rest| !rest.is_empty());
        io.write_all(&out).await.expect("write");
    }
    seen
}

/// An answer the client read back, or why it couldn't.
type Answer = Result<Vec<u8>, h2::Error>;

/// Each body that `REQUESTS` concurrent requests from an `h2` client send
/// to `echo_server`, what the server read, and each answer.
fn echo(plant: bool) -> (Vec<Vec<u8>>, Vec<Seen>, Vec<Answer>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (client_io, server_io) = duplex(64 * 1024);
        let server = tokio::spawn(echo_server(server_io, plant));
        let (client, connection) = (h2::client::Builder::new())
            .initial_window_size(CLIENT_WINDOW)
            .handshake::<_, &'static [u8]>(client_io)
            .await
            .expect("client handshake");
        let driver = tokio::spawn(async move {
            connection.await.ok();
        });
        let bodies: Vec<Vec<u8>> = (0..REQUESTS)
            .map(|request| (0..BODY).map(|at| (at * 7 + request) as u8).collect())
            .collect();
        let mut tasks = Vec::new();
        for (request, body) in bodies.iter().enumerate() {
            let client = client.clone();
            let body: &'static [u8] = body.clone().leak();
            tasks.push(tokio::spawn(async move {
                let mut client = client.ready().await?;
                let request = Request::post(format!("https://example.test/{request}"))
                    .body(())
                    .expect("request");
                let (response, mut send) = client.send_request(request, false)?;
                send.send_data(body, true)?;
                let mut body = response.await?.into_body();
                let mut answer = Vec::new();
                while let Some(chunk) = body.data().await {
                    let chunk = chunk?;
                    body.flow_control().release_capacity(chunk.len())?;
                    answer.extend(&chunk[..]);
                }
                Ok(answer)
            }));
        }
        let mut answers = Vec::new();
        for task in tasks {
            answers.push(task.await.expect("request task"));
        }
        drop(client);
        driver.await.expect("client driver");
        let seen = server.await.expect("server");
        (bodies, seen, answers)
    })
}

#[test]
fn an_h2_clients_concurrent_bodies_arrive_whole_and_in_order() {
    let (bodies, mut seen, answers) = echo(false);
    // The client opens its streams in the order its tasks run.
    let streams: Vec<u32> = seen.iter().map(|seen| seen.stream).collect();
    assert_eq!(streams, [1, 3, 5, 7, 9]);
    seen.sort_by_key(|seen| seen.path.clone());
    assert_eq!(seen.len(), REQUESTS);
    for (request, (seen, body)) in seen.iter().zip(&bodies).enumerate() {
        assert_eq!(seen.path, format!("/{request}").into_bytes());
        assert!(seen.body == *body, "body {request} differs");
    }
    for (answer, body) in answers.into_iter().zip(&bodies) {
        let answer = answer.expect("an answer");
        assert!(answer == *body, "an answer differs");
    }
}

#[test]
fn a_planted_window_error_fails_the_h2_client() {
    let (_, _, answers) = echo(true);
    let refused = (answers.iter())
        .filter_map(|answer| answer.as_ref().err())
        .map(|error| error.reason())
        .collect::<Vec<_>>();
    assert!(
        refused.contains(&Some(h2::Reason::FLOW_CONTROL_ERROR)),
        "{refused:?}"
    );
}
