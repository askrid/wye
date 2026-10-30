#![allow(clippy::unwrap_used)]

use gateway_core::api::Api;

use super::*;
use crate::test_body::Client;

async fn collect(body: RequestBody<Client>) -> Result<Bytes, BoxError> {
    body.collect().await.map(|c| c.to_bytes())
}

#[tokio::test]
async fn holds_only_until_the_model_then_replays_everything() {
    let budget = Gauge::default();
    let parts = [r#"{"model":"m","#, r#""messages":[]"#, "}"];
    let mut client = Client::of(&parts);
    let held = hold_until_model(&mut client, Api::Messages, &budget, 1 << 20, None)
        .await
        .unwrap();
    assert_eq!(held.model(), Some("m"));
    assert_eq!(budget.current(), charge(parts[0].len()));
    let body = RequestBody::after_hold(held, client);
    assert_eq!(body.size_hint().lower(), parts[0].len() as u64);
    assert_eq!(collect(body).await.unwrap(), parts.concat());
    assert_eq!(budget.current(), 0);
}

/// Every way to cut the body into up to three chunks: the spliced body
/// differs from the original only in the model's value, and the hold
/// budget ends at zero.
#[tokio::test]
async fn splicing_the_model_keeps_every_other_byte_under_any_chunking() {
    let body = r#"{"model":"coding-default","stream":true,"messages":[]}"#;
    for (to, want_delta) in [
        ("claude-opus-5-5", 1i64),
        ("m", -13),
        ("claude-sonnet-5-5-x", 5),
    ] {
        let want = body.replace("coding-default", to);
        for a in 1..body.len() {
            for b in a..body.len() {
                let parts: Vec<&'static str> = [&body[..a], &body[a..b], &body[b..]]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .map(|p| &*Box::leak(p.to_owned().into_boxed_str()))
                    .collect();
                let budget = Gauge::default();
                let mut client = Client::of(&parts);
                let mut held = hold_until_model(&mut client, Api::Messages, &budget, 1 << 20, None)
                    .await
                    .unwrap();
                assert_eq!(held.splice_model(Bytes::from(to)), Some(want_delta));
                let got = collect(RequestBody::after_hold(held, client))
                    .await
                    .unwrap();
                assert_eq!(got, want, "cuts at {a}, {b}");
                assert_eq!(budget.current(), 0, "cuts at {a}, {b}");
            }
        }
    }
}

#[test]
fn an_empty_value_is_spliced_at_any_cut() {
    let body = r#"{"model":"","stream":true}"#;
    let at = body.find(r#""""#).unwrap() as u64 + 1;
    for cut in 1..body.len() {
        let (head, tail) = body.as_bytes().split_at(cut);
        let chunks = [Bytes::from_static(head), Bytes::from_static(tail)];
        let mut out = Vec::new();
        let prefix = Bytes::from_static(b"anthropic.");
        splice_chunks(chunks.into_iter(), &(at..at), prefix, |c| out.push(c));
        assert_eq!(
            out.concat(),
            br#"{"model":"anthropic.","stream":true}"#,
            "cut at {cut}"
        );
    }
    // The chunks end at the span: failover's sent bytes, before the rest.
    let head = Bytes::from_static(&body.as_bytes()[..at as usize]);
    let mut out = Vec::new();
    let prefix = Bytes::from_static(b"anthropic.");
    splice_chunks(std::iter::once(head), &(at..at), prefix, |c| out.push(c));
    assert_eq!(out.concat(), br#"{"model":"anthropic."#);
}

#[tokio::test]
async fn a_repeated_model_later_fails_the_body() {
    let budget = Gauge::default();
    let mut client = Client::of(&[r#"{"model":"allowed","#, r#""model":"other"}"#]);
    let held = hold_until_model(&mut client, Api::Messages, &budget, 1 << 20, None)
        .await
        .unwrap();
    let error = collect(RequestBody::after_hold(held, client))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<PolicyError>(),
        Some(&PolicyError::Repeated)
    );
}

#[tokio::test]
async fn a_late_max_tokens_above_the_limit_fails_the_body() {
    let mut client = Client::of(&[r#"{"model":"m","messages":[],"#, r#""max_tokens":9000}"#]);
    let held = hold_until_model(
        &mut client,
        Api::Messages,
        &Gauge::default(),
        1 << 20,
        Some(4000),
    )
    .await
    .unwrap();
    assert_eq!(
        held.max_output_tokens(),
        None,
        "not read before the hold ended"
    );
    let error = collect(RequestBody::after_hold(held, client))
        .await
        .unwrap_err();
    let over = error.downcast_ref::<TokenLimitError>().unwrap();
    assert_eq!((over.requested, over.limit), (Some(9000), 4000));
}

#[tokio::test]
async fn trailers_end_the_body_for_a_required_field() {
    let mut client = Client::of(&[r#"{"model":"m","#, r#""input":[]}"#])
        .with_trailers(std::future::ready(Some(Ok(http::HeaderMap::new()))));
    let held = hold_until_model(
        &mut client,
        Api::Responses,
        &Gauge::default(),
        1 << 20,
        Some(4000),
    )
    .await
    .unwrap();
    let mut body = RequestBody::after_hold(held, client);
    // As hyper polls: nothing after the trailers.
    let refused = loop {
        match body.frame().await {
            Some(Ok(frame)) if frame.is_trailers() => break false,
            Some(Ok(_)) => {}
            Some(Err(e)) => break e.is::<TokenLimitError>(),
            None => break false,
        }
    };
    assert!(
        refused,
        "a body without max_output_tokens ended with its trailers"
    );
}

/// A body sent in tiny chunks pins far more memory than its bytes: each
/// chunk counts at least [`MIN_CHUNK_CHARGE`], so the budget runs out after a
/// handful of chunks, not after millions.
#[tokio::test]
async fn tiny_chunks_count_at_least_the_minimum_charge() {
    let budget = Gauge::default();
    let bytes = r#"{"messages":[{"role":"user","content":"hi"}],"model":"m"}"#;
    let parts: Vec<&str> = (0..bytes.len()).map(|i| &bytes[i..=i]).collect();
    let mut client = Client::of(&parts);
    let error = hold_until_model(
        &mut client,
        Api::Messages,
        &budget,
        8 * MIN_CHUNK_CHARGE,
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(error, HoldError::Overloaded));
    assert_eq!(budget.current(), 0);

    // Ordinary chunks count their length: the same budget holds 8 KiB.
    let big = format!(r#"{{"messages":[{{"content":"{}"}}],"#, "x".repeat(4000));
    let mut client = Client(
        [big.clone(), " ".into(), r#""model":"m"}"#.into()]
            .into_iter()
            .map(Bytes::from)
            .collect(),
    );
    let held = hold_until_model(
        &mut client,
        Api::Messages,
        &budget,
        8 * MIN_CHUNK_CHARGE,
        None,
    )
    .await
    .unwrap();
    assert_eq!(held.model(), Some("m"));
    assert_eq!(budget.current(), big.len() + 2 * MIN_CHUNK_CHARGE);
}

/// A client that stops sending before its model is read is refused, and
/// what it sent is released, instead of holding its share for good.
#[tokio::test(start_paused = true)]
async fn a_stalled_hold_is_refused_and_released() {
    let budget = Gauge::default();
    let mut client = crate::test_body::Stalling(Client::of(&[r#"{"messages":["#]));
    let started = tokio::time::Instant::now();
    let error = hold_until_model(&mut client, Api::Messages, &budget, 1 << 20, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HoldError::Stalled));
    assert_eq!(started.elapsed(), HOLD_IDLE_TIMEOUT);
    assert_eq!(budget.current(), 0);
}

#[tokio::test]
async fn refuses_beyond_the_budget_or_without_a_model() {
    let budget = Gauge::default();
    let _other = {
        let mut other = budget.hold_none();
        assert!(other.try_grow(90, 100));
        other
    };
    let mut client = Client::of(&[r#"{"messages":[], "#, r#""model":"m"}"#]);
    let error = hold_until_model(&mut client, Api::Messages, &budget, 100, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HoldError::Overloaded));
    assert_eq!(budget.current(), 90);

    let mut client = Client::of(&[r#"{"messages":[]}"#]);
    let error = hold_until_model(&mut client, Api::Messages, &Gauge::default(), 1 << 20, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HoldError::Model(PolicyError::Missing)));

    let mut client = Limited::new(
        Client::of(&[r#"{"messages":[1,2,3,4], "#, r#""model":"m"}"#]),
        10,
    );
    let error = hold_until_model(&mut client, Api::Messages, &Gauge::default(), 1 << 20, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HoldError::TooLarge));
}

#[tokio::test]
async fn responses_need_max_output_tokens_under_a_ceiling() {
    // Absent: refused when the body ends, so the request is never complete.
    let mut client = Client::of(&[r#"{"model":"m","input":[]"#, r#","stream":true}"#]);
    let held = hold_until_model(
        &mut client,
        Api::Responses,
        &Gauge::default(),
        1 << 20,
        Some(4000),
    )
    .await
    .unwrap();
    let error = collect(RequestBody::after_hold(held, client))
        .await
        .unwrap_err();
    let missing = error.downcast_ref::<TokenLimitError>().unwrap();
    assert_eq!(missing.requested, None);
    assert_eq!(
        missing.to_string(),
        "max_output_tokens is required: this credential's limit is 4000"
    );

    // Present and within the limit: forwarded unchanged.
    let body = [
        r#"{"model":"m","input":[],"#,
        r#""max_output_tokens":1000}"#,
    ];
    let mut client = Client::of(&body);
    let held = hold_until_model(
        &mut client,
        Api::Responses,
        &Gauge::default(),
        1 << 20,
        Some(4000),
    )
    .await
    .unwrap();
    let forwarded = collect(RequestBody::after_hold(held, client))
        .await
        .unwrap();
    assert_eq!(forwarded, body.concat());

    // Anthropic's max_tokens is not the Responses field.
    let mut client = Client::of(&[r#"{"model":"m","#, r#""max_tokens":10}"#]);
    let held = hold_until_model(
        &mut client,
        Api::Responses,
        &Gauge::default(),
        1 << 20,
        Some(4000),
    )
    .await
    .unwrap();
    assert!(
        collect(RequestBody::after_hold(held, client))
            .await
            .is_err()
    );
}
