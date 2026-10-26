#![allow(clippy::unwrap_used)]

use http_body_util::BodyExt;

use super::*;

/// A client body that yields its chunks, one per poll.
struct Client(VecDeque<Bytes>);

impl Body for Client {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(self.0.pop_front().map(|c| Ok(Frame::data(c))))
    }
    fn is_end_stream(&self) -> bool {
        self.0.is_empty()
    }
}

fn client(parts: &[&'static str]) -> Client {
    Client(
        parts
            .iter()
            .map(|p| Bytes::from_static(p.as_bytes()))
            .collect(),
    )
}

const ROOMY: Bounds = Bounds {
    max_body: 1 << 20,
    max_held: 1 << 20,
};

async fn collect<B: Body<Data = Bytes> + Unpin>(body: FailoverBody<B>) -> Bytes
where
    B::Error: std::fmt::Debug,
{
    body.collect().await.unwrap().to_bytes()
}

#[tokio::test]
async fn a_sent_body_comes_back_whole() {
    let held = Gauge::default();
    let (body, handback) = FailoverBody::kept(client(&["ab", "cd", "ef"]), &held, ROOMY);
    assert_eq!(collect(body).await, "abcdef");
    let unsent = handback.await.unwrap();
    assert_eq!(held.current(), 6);
    let (again, next) = FailoverBody::resume(unsent, ROOMY);
    assert_eq!(collect(again).await, "abcdef");
    // Once the request is answered, nothing waits for the hand-back: the
    // last attempt's kept bytes are released.
    drop(next);
    assert_eq!(held.current(), 0);
}

#[tokio::test]
async fn a_partly_sent_body_resumes_where_it_stopped() {
    let held = Gauge::default();
    let (mut body, handback) = FailoverBody::kept(client(&["ab", "cd", "ef"]), &held, ROOMY);
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first, "ab");
    drop(body);
    let unsent = handback.await.unwrap();
    let (again, _next) = FailoverBody::resume(unsent, ROOMY);
    assert_eq!(collect(again).await, "abcdef", "replayed, then read on");
}

#[tokio::test]
async fn a_body_beyond_either_bound_is_not_kept() {
    let held = Gauge::default();
    let small = Bounds {
        max_body: 3,
        max_held: 1 << 20,
    };
    let (body, handback) = FailoverBody::kept(client(&["ab", "cd"]), &held, small);
    assert_eq!(collect(body).await, "abcd", "forwarded all the same");
    assert!(handback.await.is_err());
    assert_eq!(held.current(), 0);

    let busy = Gauge::default();
    let _others = {
        let mut h = busy.hold_none();
        assert!(h.try_grow(9, 10));
        h
    };
    let process = Bounds {
        max_body: 1 << 20,
        max_held: 10,
    };
    let (body, handback) = FailoverBody::kept(client(&["ab"]), &busy, process);
    assert_eq!(collect(body).await, "ab");
    assert!(handback.await.is_err(), "the process-wide bound is full");
}

#[tokio::test]
async fn a_body_sent_once_hands_nothing_back() {
    let body = FailoverBody::once(client(&["ab"]));
    assert_eq!(body.size_hint().lower(), 0);
    assert_eq!(collect(body).await, "ab");
}
