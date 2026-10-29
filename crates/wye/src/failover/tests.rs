#![allow(clippy::unwrap_used)]

use http_body_util::BodyExt;

use super::*;
use crate::test_body::Client;

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
    let (body, handback) = FailoverBody::kept(Client::of(&["ab", "cd", "ef"]), &held, ROOMY);
    assert_eq!(collect(body).await, "abcdef");
    let resend = handback.await.unwrap();
    assert_eq!(held.current(), 6);
    let (again, next) = FailoverBody::resume(resend, ROOMY);
    assert_eq!(collect(again).await, "abcdef");
    // Once the request is answered, nothing waits for the hand-back: the
    // last attempt's kept bytes are released.
    drop(next);
    assert_eq!(held.current(), 0);
}

#[tokio::test]
async fn a_partly_sent_body_resumes_where_it_stopped() {
    let held = Gauge::default();
    let (mut body, handback) = FailoverBody::kept(Client::of(&["ab", "cd", "ef"]), &held, ROOMY);
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first, "ab");
    drop(body);
    let resend = handback.await.unwrap();
    let (again, _next) = FailoverBody::resume(resend, ROOMY);
    assert_eq!(collect(again).await, "abcdef", "replayed, then read on");
}

#[tokio::test]
async fn a_body_beyond_either_bound_is_not_kept() {
    let held = Gauge::default();
    let small = Bounds {
        max_body: 3,
        max_held: 1 << 20,
    };
    let (body, handback) = FailoverBody::kept(Client::of(&["ab", "cd"]), &held, small);
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
    let (body, handback) = FailoverBody::kept(Client::of(&["ab"]), &busy, process);
    assert_eq!(collect(body).await, "ab");
    assert!(handback.await.is_err(), "the process-wide bound is full");
}

#[tokio::test]
async fn a_body_sent_once_hands_nothing_back() {
    let body = FailoverBody::once(Client::of(&["ab"]));
    assert_eq!(body.size_hint().lower(), 0);
    assert_eq!(collect(body).await, "ab");
}

proptest::proptest! {
    /// Wherever the body is split into chunks and wherever an attempt stops
    /// reading, the next attempt sends exactly the client's bytes, once.
    #[test]
    fn a_resumed_body_is_the_original(
        body in proptest::collection::vec(proptest::num::u8::ANY, 0..300),
        cuts in proptest::collection::vec(0usize..300, 0..8),
        stop_after in 0usize..10,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c % (body.len() + 1)).collect();
            cuts.sort_unstable();
            let mut chunks = VecDeque::new();
            let mut at = 0;
            for cut in cuts.into_iter().chain([body.len()]) {
                if cut > at {
                    chunks.push_back(Bytes::copy_from_slice(&body[at..cut]));
                    at = cut;
                }
            }
            let held = Gauge::default();
            let (mut first, handback) = FailoverBody::kept(Client(chunks), &held, ROOMY);
            for _ in 0..stop_after {
                if first.frame().await.is_none() {
                    break;
                }
            }
            drop(first);
            let (again, next) = FailoverBody::resume(handback.await.unwrap(), ROOMY);
            assert_eq!(collect(again).await, body);
            drop(next);
            assert_eq!(held.current(), 0);
        });
    }
}

/// Every way to cut the body into up to three chunks, handed back after
/// every number of sent frames: rewriting the model id either yields the
/// body with only the model changed, or, when the value straddles the sent
/// bytes and the unread rest, declines.
#[tokio::test]
async fn a_handed_back_body_takes_a_new_model_id_wherever_it_is() {
    use gateway_core::api::Api;

    use crate::request_body::hold_until_model;

    const BODY: &str = r#"{"model":"claude-opus-5-5","messages":[]}"#;
    let start = BODY.find("claude").unwrap() as u64;
    let span = start..start + "claude-opus-5-5".len() as u64;
    let value = Bytes::from_static(b"anthropic.claude-opus-5-5");
    let want = BODY.replace("claude-opus", "anthropic.claude-opus");
    let mut rewritten = 0;
    for i in 0..=BODY.len() {
        for j in i..=BODY.len() {
            let parts = [&BODY[..i], &BODY[i..j], &BODY[j..]];
            let parts: VecDeque<Bytes> = parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .map(|p| Bytes::copy_from_slice(p.as_bytes()))
                .collect();
            for sent_frames in 0..=parts.len() {
                let gauge = Gauge::default();
                let mut client = Client(parts.clone());
                let held = hold_until_model(&mut client, Api::Messages, &gauge, 1 << 20, None)
                    .await
                    .unwrap();
                let body = RequestBody::after_hold(held, client);
                let (mut body, handback) = FailoverBody::kept(body, &gauge, ROOMY);
                let mut sent = 0u64;
                for _ in 0..sent_frames {
                    if let Some(frame) = body.frame().await {
                        sent += frame.unwrap().into_data().unwrap().len() as u64;
                    }
                }
                drop(body);
                let mut resend = handback.await.unwrap();
                let straddles = span.start < sent && sent < span.end;
                assert_eq!(resend.splice(&span, value.clone()), !straddles, "{parts:?}");
                let (again, next) = FailoverBody::resume(resend, ROOMY);
                let got = collect(again).await;
                drop(next);
                assert_eq!(gauge.current(), 0, "every kept byte released");
                if straddles {
                    assert_eq!(got, BODY);
                } else {
                    assert_eq!(got, want, "{parts:?} after {sent_frames} frames");
                    rewritten += 1;
                }
            }
        }
    }
    assert!(rewritten > 0);
}

#[tokio::test]
async fn a_hand_back_says_whether_the_whole_body_was_delivered() {
    let held = Gauge::default();
    let (body, handback) = FailoverBody::kept(Client::of(&["ab", "cd"]), &held, ROOMY);
    assert_eq!(collect(body).await, "abcd");
    let resend = handback.await.unwrap();
    assert!(resend.sent_whole());

    // Resent whole: the replay and the end both delivered.
    let (again, handback) = FailoverBody::resume(resend, ROOMY);
    assert_eq!(collect(again).await, "abcd");
    assert!(handback.await.unwrap().sent_whole());

    let (mut body, handback) = FailoverBody::kept(Client::of(&["ab", "cd"]), &held, ROOMY);
    body.frame().await.unwrap().unwrap();
    drop(body);
    let resend = handback.await.unwrap();
    assert!(!resend.sent_whole(), "cut after the first chunk");

    // A resend dropped during its replay did not deliver the whole body.
    let (mut again, handback) = FailoverBody::resume(resend, ROOMY);
    again.frame().await.unwrap().unwrap();
    drop(again);
    assert!(!handback.await.unwrap().sent_whole());

    // Trailers are the last frame: hyper polls no further, and the body was
    // delivered whole.
    let chunked = Client::of(&["ab", "cd"])
        .with_trailers(std::future::ready(Some(Ok(http::HeaderMap::new()))));
    let (mut body, handback) = FailoverBody::kept(chunked, &held, ROOMY);
    while !body.frame().await.unwrap().unwrap().is_trailers() {}
    drop(body);
    assert!(handback.await.unwrap().sent_whole());
}
