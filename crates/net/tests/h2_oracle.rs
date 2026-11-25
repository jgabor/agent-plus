//! The `h2` crate is the oracle for the proxy's own HTTP/2. This test shows
//! that a test can drive an `h2` client and server over an in-memory pipe,
//! so later tests can put our code on either end.

use http::Request;
use tokio::io::duplex;

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
