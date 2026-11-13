//! Why an upstream gave no usable response, as records name it: one test per
//! cause a local upstream can produce on demand. Each upstream here does one
//! thing wrong, by hand where hyper would not.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::harness::*;

/// A gateway whose one upstream is at `base_url`, trusting `ca`, with
/// `lines` in its entry.
async fn gateway(base_url: &str, ca: Option<&TestCa>, lines: &str) -> Harness {
    let resources = with_upstreams(vec![UpstreamResources {
        auth: UpstreamAuth::api_key(PROVIDER_KEY).unwrap(),
        extra_roots: ca.map(TestCa::ca_cert).into_iter().collect(),
    }]);
    let entry = format!(
        "[[upstreams]]\nbase_url = \"{base_url}\"\nprovider_key = {{ env = \"UNUSED\" }}\n{lines}\n"
    );
    Harness::launch_config(&config("", &entry), resources, Arc::default()).await
}

/// The record of a request Wye answered with `status` itself, because its
/// upstream gave no usable response.
async fn upstream_error(h: &Harness, status: StatusCode) -> serde_json::Value {
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), status);
    read_body(res).await;
    let record = h.usage_record().await;
    assert_eq!(record["outcome"], "upstream_error");
    assert_eq!(record["status"], status.as_u16());
    record
}

/// A plain upstream that reads each request whole (the `{}` body ends it),
/// then hands the connection to `then`.
async fn by_hand<F, Fut>(then: F) -> SocketAddr
where
    F: Fn(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stream.read(&mut buf).await {
                request.extend_from_slice(&buf[..n]);
                if request.ends_with(b"\r\n\r\n{}") {
                    break;
                }
            }
            tokio::spawn(then(stream));
        }
    });
    addr
}

/// A TLS upstream speaking HTTP/2 through h2, resetting each connection's
/// first request's stream with `code` and keeping the connection open.
async fn resetting_streams(ca: Arc<TestCa>, code: h2::Reason) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let ca = Arc::clone(&ca);
            tokio::spawn(async move {
                let tls = ca.accept(stream).await.unwrap();
                let mut connection = h2::server::handshake(tls).await.unwrap();
                if let Some(Ok((_request, mut respond))) = connection.accept().await {
                    respond.send_reset(code);
                }
                while connection.accept().await.is_some() {}
            });
        }
    });
    addr
}

/// A TLS upstream speaking HTTP/2 by hand, because h2's server cannot refuse
/// a stream it has read: it answers the first request's `HEADERS` with a
/// `GOAWAY` of `code` whose last stream id is 0, so the request is one the
/// upstream did not process, and keeps the connection open.
async fn refusing_with_goaway(ca: Arc<TestCa>, code: u32) -> SocketAddr {
    const PREFACE: usize = 24;
    const HEADERS: u8 = 0x1;
    const SETTINGS: u8 = 0x4;
    const ACK: u8 = 0x1;
    const GOAWAY: u8 = 0x7;
    /// A frame header: 24-bit length, type, flags, stream 0.
    fn frame(len: u32, kind: u8, flags: u8) -> Vec<u8> {
        let mut head = len.to_be_bytes()[1..].to_vec();
        head.extend_from_slice(&[kind, flags, 0, 0, 0, 0]);
        head
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let ca = Arc::clone(&ca);
            tokio::spawn(async move {
                let mut tls = ca.accept(stream).await.unwrap();
                tls.write_all(&frame(0, SETTINGS, 0)).await.unwrap();
                let mut preface = [0u8; PREFACE];
                tls.read_exact(&mut preface).await.unwrap();
                let mut head = [0u8; 9];
                while tls.read_exact(&mut head).await.is_ok() {
                    let len = u32::from_be_bytes([0, head[0], head[1], head[2]]);
                    let mut payload = vec![0u8; len as usize];
                    tls.read_exact(&mut payload).await.unwrap();
                    match (head[3], head[4]) {
                        (SETTINGS, 0) => tls.write_all(&frame(0, SETTINGS, ACK)).await.unwrap(),
                        (HEADERS, _) => {
                            let mut goaway = frame(8, GOAWAY, 0);
                            goaway.extend_from_slice(&0u32.to_be_bytes());
                            goaway.extend_from_slice(&code.to_be_bytes());
                            tls.write_all(&goaway).await.unwrap();
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn a_refused_connection_is_connect_refused() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let h = gateway(&format!("http://{addr}"), None, "").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "connect_refused");
    assert!(record.get("h2_error_code").is_none());
}

/// `.invalid` never resolves (RFC 6761), and resolvers answer so locally.
#[tokio::test]
async fn a_host_name_that_does_not_resolve_is_connect_dns() {
    let h = gateway("https://wye-test.invalid", None, "").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "connect_dns");
}

#[tokio::test]
async fn a_stalled_tls_handshake_is_connect_timeout() {
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = silent.local_addr().unwrap();
    let _accepting = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            open.push(stream);
        }
    });
    let h = gateway(&format!("https://{addr}"), None, "connect_timeout = \"1s\"").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "connect_timeout");
}

#[tokio::test]
async fn an_upstream_that_closes_before_its_headers_is_closed() {
    let addr = by_hand(|stream| async move { drop(stream) }).await;
    let h = gateway(&format!("http://{addr}"), None, "").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "closed");
}

#[tokio::test]
async fn an_upstream_that_resets_before_its_headers_is_reset() {
    let addr = by_hand(|stream| async move {
        // A zero linger closes with RST instead of FIN.
        socket2::SockRef::from(&stream)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        drop(stream);
    })
    .await;
    let h = gateway(&format!("http://{addr}"), None, "").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "reset");
}

#[tokio::test]
async fn an_upstream_that_answers_with_no_http_is_protocol() {
    let addr = by_hand(|mut stream| async move {
        let _ = stream.write_all(b"SSH-2.0-not-http\r\n\r\n").await;
        let _ = stream.flush().await;
        tokio::time::sleep(WAIT).await;
    })
    .await;
    let h = gateway(&format!("http://{addr}"), None, "").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "protocol");
}

#[tokio::test]
async fn an_http2_stream_reset_is_stream_reset_with_its_code() {
    let ca = Arc::new(TestCa::generate());
    let addr = resetting_streams(Arc::clone(&ca), h2::Reason::INTERNAL_ERROR).await;
    let h = gateway(&format!("https://{addr}"), Some(&ca), "").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "stream_reset");
    assert_eq!(record["h2_error_code"], "INTERNAL_ERROR");
}

#[tokio::test]
async fn an_http2_goaway_refusing_the_request_is_goaway_with_its_code() {
    let ca = Arc::new(TestCa::generate());
    let addr = refusing_with_goaway(Arc::clone(&ca), 0xb).await;
    let h = gateway(&format!("https://{addr}"), Some(&ca), "").await;
    let record = upstream_error(&h, StatusCode::BAD_GATEWAY).await;
    assert_eq!(record["upstream_error"], "goaway");
    assert_eq!(record["h2_error_code"], "ENHANCE_YOUR_CALM");
}

/// After the headers Wye cannot answer with its own status: the record is
/// `truncated`, with why.
#[tokio::test]
async fn a_response_cut_after_its_headers_is_truncated_and_closed() {
    let addr = by_hand(|mut stream| async move {
        let head = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     transfer-encoding: chunked\r\n\r\n2\r\n{\"\r\n";
        let _ = stream.write_all(head).await;
        drop(stream);
    })
    .await;
    let h = gateway(&format!("http://{addr}"), None, "").await;
    let res = h.send(messages(), "{}").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = tokio::time::timeout(WAIT, res.into_body().collect())
        .await
        .unwrap();
    assert!(body.is_err(), "the cut reaches the client");
    let record = h.usage_record().await;
    assert_eq!(record["outcome"], "truncated");
    assert_eq!(record["upstream_error"], "closed");
}
