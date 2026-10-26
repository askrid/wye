#![allow(clippy::unwrap_used)]

use gateway_core::api::Api;

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

fn chunks(parts: &[&'static str]) -> Client {
    Client(
        parts
            .iter()
            .map(|p| Bytes::from_static(p.as_bytes()))
            .collect(),
    )
}

async fn collect(body: RequestBody<Client>) -> Result<Bytes, BoxError> {
    body.collect().await.map(|c| c.to_bytes())
}

#[tokio::test]
async fn holds_only_until_the_model_then_replays_everything() {
    let budget = Gauge::default();
    let parts = [r#"{"model":"m","#, r#""messages":[]"#, "}"];
    let mut client = chunks(&parts);
    let held = hold_until_model(&mut client, Api::Messages, &budget, 1 << 20, 1 << 20, None)
        .await
        .unwrap();
    assert_eq!(held.model(), Some("m"));
    assert_eq!(budget.current(), parts[0].len());
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
                let mut client = chunks(&parts);
                let mut held =
                    hold_until_model(&mut client, Api::Messages, &budget, 1 << 20, 1 << 20, None)
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

#[tokio::test]
async fn a_repeated_model_later_fails_the_body() {
    let budget = Gauge::default();
    let mut client = chunks(&[r#"{"model":"allowed","#, r#""model":"other"}"#]);
    let held = hold_until_model(&mut client, Api::Messages, &budget, 1 << 20, 1 << 20, None)
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
    let mut client = chunks(&[r#"{"model":"m","messages":[],"#, r#""max_tokens":9000}"#]);
    let held = hold_until_model(
        &mut client,
        Api::Messages,
        &Gauge::default(),
        1 << 20,
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
async fn refuses_beyond_the_budget_or_without_a_model() {
    let budget = Gauge::default();
    let _other = {
        let mut other = budget.hold_none();
        assert!(other.try_grow(90, 100));
        other
    };
    let mut client = chunks(&[r#"{"messages":[], "#, r#""model":"m"}"#]);
    let error = hold_until_model(&mut client, Api::Messages, &budget, 100, 1 << 20, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HoldError::Overloaded));
    assert_eq!(budget.current(), 90);

    let mut client = chunks(&[r#"{"messages":[]}"#]);
    let error = hold_until_model(
        &mut client,
        Api::Messages,
        &Gauge::default(),
        100,
        1 << 20,
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(error, HoldError::Model(PolicyError::Missing)));

    let mut client = chunks(&[r#"{"messages":[1,2,3,4], "#, r#""model":"m"}"#]);
    let error = hold_until_model(&mut client, Api::Messages, &Gauge::default(), 100, 10, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HoldError::TooLarge));
}

#[tokio::test]
async fn responses_need_max_output_tokens_under_a_ceiling() {
    // Absent: refused when the body ends, so the request is never complete.
    let mut client = chunks(&[r#"{"model":"m","input":[]"#, r#","stream":true}"#]);
    let held = hold_until_model(
        &mut client,
        Api::Responses,
        &Gauge::default(),
        1 << 20,
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
    let mut client = chunks(&body);
    let held = hold_until_model(
        &mut client,
        Api::Responses,
        &Gauge::default(),
        1 << 20,
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
    let mut client = chunks(&[r#"{"model":"m","#, r#""max_tokens":10}"#]);
    let held = hold_until_model(
        &mut client,
        Api::Responses,
        &Gauge::default(),
        1 << 20,
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
