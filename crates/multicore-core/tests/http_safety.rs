use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

use multicore_core::{FetchError, HttpClient, MAX_CONFIG_BYTES, ReqwestHttpClient, UA_MIHOMO};

fn serve_once(response: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    (format!("http://{address}/subscription"), handle)
}

fn read_request(stream: &mut TcpStream) {
    let mut request = [0_u8; 4096];
    let _ = stream.read(&mut request).unwrap();
}

#[tokio::test]
async fn default_http_client_never_follows_redirects() {
    let redirect_target = TcpListener::bind("127.0.0.1:0").unwrap();
    redirect_target.set_nonblocking(true).unwrap();
    let target = redirect_target.local_addr().unwrap();
    let (url, server) = serve_once(format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{target}/must-not-receive-secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    ));

    let response = ReqwestHttpClient::default()
        .get(&url, UA_MIHOMO)
        .await
        .unwrap();
    server.join().unwrap();
    assert_eq!(response.status, 302);
    assert!(redirect_target.accept().is_err());
}

#[tokio::test]
async fn massive_response_with_dishonest_oversized_length_is_rejected() {
    let (url, server) = serve_once(format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        MAX_CONFIG_BYTES + 1
    ));

    let result = ReqwestHttpClient::default()
        .get(&url, multicore_core::UA_NATIVE)
        .await;
    server.join().unwrap();
    assert_eq!(result, Err(FetchError::TooLarge));
}

#[tokio::test]
async fn massive_stream_without_content_length_is_still_capped_at_32_mib() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .unwrap();
        let chunk = [b'x'; 64 * 1024];
        for _ in 0..=(MAX_CONFIG_BYTES / chunk.len()) {
            if stream.write_all(&chunk).is_err() {
                break;
            }
        }
    });

    let url = format!("http://{address}/subscription");
    let result = ReqwestHttpClient::default()
        .get(&url, multicore_core::UA_NATIVE)
        .await;
    server.join().unwrap();
    assert_eq!(result, Err(FetchError::TooLarge));
}
