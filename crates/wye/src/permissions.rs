//! Model permissions: the request's `model` is checked before anything is
//! sent upstream.
//!
//! [`hold_until_model`] reads the request body only until its top-level
//! `model` value has been read. Claude Code sends `model` first, so usually
//! only the first chunk waits. The held bytes count against a process-wide
//! budget (`server.max_held_request_bytes`). When the model is allowed,
//! [`RequestBody`] replays the held chunks and streams the rest, and keeps
//! checking it: a second top-level `model` key fails the body, which aborts
//! the upstream request instead of letting the provider read the other model.

use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use gateway_core::policy::{PolicyError, PolicyScanner};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};

use crate::{
    gauge::{Gauge, Held},
    upstream::BoxError,
};

/// Why a request was refused while its model was read.
#[derive(Debug)]
pub(crate) enum HoldError {
    /// The body's `model` cannot be checked.
    Model(PolicyError),
    /// The process-wide hold budget is used up.
    Overloaded,
    /// The body exceeded the request size limit while being held.
    TooLarge,
    /// The client's body failed (for example, the connection dropped).
    Body,
}

/// Chunks held in order. The first is stored inline: Claude Code sends
/// `model` first, so most holds keep one chunk and allocate no queue.
#[derive(Default)]
struct Chunks {
    first: Option<Bytes>,
    rest: VecDeque<Bytes>,
}

impl Chunks {
    fn push(&mut self, chunk: Bytes) {
        if self.is_empty() {
            self.first = Some(chunk);
        } else {
            self.rest.push_back(chunk);
        }
    }

    fn pop(&mut self) -> Option<Bytes> {
        self.first.take().or_else(|| self.rest.pop_front())
    }

    fn is_empty(&self) -> bool {
        self.first.is_none() && self.rest.is_empty()
    }

    fn bytes(&self) -> u64 {
        self.first
            .iter()
            .chain(&self.rest)
            .map(|c| c.len() as u64)
            .sum()
    }
}

/// What a held request carries until its body is fully sent.
struct Hold {
    chunks: Chunks,
    /// Bytes of `chunks`, counted against the hold budget.
    budget: Held,
    scanner: PolicyScanner,
    /// The whole body is in `chunks`.
    ended: bool,
    /// The request's `max_tokens` ceiling, checked again as the rest streams.
    token_limit: Option<u64>,
}

/// A `max_tokens` above the request's ceiling, found after forwarding began.
#[derive(Debug)]
pub(crate) struct OverTokenLimit {
    pub(crate) requested: u64,
    pub(crate) limit: u64,
}

impl std::fmt::Display for OverTokenLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "max_tokens {} exceeds this credential's limit of {}",
            self.requested, self.limit
        )
    }
}

impl std::error::Error for OverTokenLimit {}

/// The start of a request body, read until its model was known.
///
/// Boxed, and kept boxed inside [`RequestBody`]: hyper's client channel
/// stores 32 requests inline per upstream connection, so every byte of the
/// body type costs 32 bytes per connection (on the plain HTTP/1.1 path,
/// per stream), restricted or not.
pub(crate) struct HeldStart(Box<Hold>);

impl HeldStart {
    /// The model named by the request.
    pub(crate) fn model(&self) -> Option<&str> {
        self.0.scanner.model()
    }

    /// The request's `max_tokens`, if it was read before the hold ended.
    pub(crate) fn max_tokens(&self) -> Option<u64> {
        self.0.scanner.max_tokens()
    }
}

/// Reads `body` until its model is known (or it ends), holding what it read.
///
/// Cancel safety: dropping the future drops the held chunks and releases
/// their budget.
pub(crate) async fn hold_until_model<B>(
    body: &mut B,
    budget: &Gauge,
    max_held: usize,
    max_body: usize,
    token_limit: Option<u64>,
) -> Result<HeldStart, HoldError>
where
    B: Body<Data = Bytes> + Unpin,
{
    let mut hold = Box::new(Hold {
        chunks: Chunks::default(),
        budget: budget.hold_none(),
        scanner: PolicyScanner::new(),
        ended: false,
        token_limit,
    });
    let mut total = 0usize;
    while hold.scanner.model().is_none() {
        let Some(frame) = body.frame().await else {
            hold.ended = true;
            hold.scanner.finish().map_err(HoldError::Model)?;
            break;
        };
        // Request trailers carry nothing a provider reads; only data counts.
        let Ok(data) = frame.map_err(|_| HoldError::Body)?.into_data() else {
            continue;
        };
        total = total.saturating_add(data.len());
        if total > max_body {
            return Err(HoldError::TooLarge);
        }
        if !hold.budget.try_grow(data.len(), max_held) {
            return Err(HoldError::Overloaded);
        }
        hold.scanner.feed(&data);
        hold.chunks.push(data);
        if let Some(error) = hold.scanner.error() {
            return Err(HoldError::Model(error));
        }
    }
    Ok(HeldStart(hold))
}

/// The request body sent upstream: held chunks first, then the rest of the
/// client's body, checked for a repeated `model` when permissions apply.
pub(crate) struct RequestBody<B> {
    /// `None` once the client's body has ended or failed the check.
    inner: Option<B>,
    hold: Option<Box<Hold>>,
}

impl<B> RequestBody<B> {
    /// The client's body unchanged: no permissions apply.
    pub(crate) fn new(inner: B) -> Self {
        Self {
            inner: Some(inner),
            hold: None,
        }
    }

    /// `held` followed by the rest of `inner`.
    pub(crate) fn after_hold(held: HeldStart, inner: B) -> Self {
        Self {
            inner: (!held.0.ended).then_some(inner),
            hold: Some(held.0),
        }
    }
}

impl<B> Body for RequestBody<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let Some(hold) = this.hold.as_mut()
            && let Some(chunk) = hold.chunks.pop()
        {
            hold.budget.release(chunk.len());
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(inner).poll_frame(cx);
        if let (Poll::Ready(Some(Ok(frame))), Some(hold)) = (&polled, this.hold.as_mut())
            && let Some(data) = frame.data_ref()
        {
            hold.scanner.feed(data);
            // Either failure stops the upstream request mid-body: the provider
            // never sees a complete request that breaks the policy.
            if let Some(error) = hold.scanner.error() {
                this.inner = None;
                return Poll::Ready(Some(Err(Box::new(error))));
            }
            if let (Some(limit), Some(requested)) = (hold.token_limit, hold.scanner.max_tokens())
                && requested > limit
            {
                this.inner = None;
                return Poll::Ready(Some(Err(Box::new(OverTokenLimit { requested, limit }))));
            }
        }
        polled.map(|frame| frame.map(|f| f.map_err(Into::into)))
    }

    fn is_end_stream(&self) -> bool {
        self.hold.as_ref().is_none_or(|h| h.chunks.is_empty())
            && self.inner.as_ref().is_none_or(B::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        let held = self.hold.as_ref().map_or(0, |h| h.chunks.bytes());
        let rest = self
            .inner
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), B::size_hint);
        let mut hint = SizeHint::new();
        hint.set_lower(rest.lower().saturating_add(held));
        if let Some(upper) = rest.upper() {
            hint.set_upper(upper.saturating_add(held));
        }
        hint
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

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
        let held = hold_until_model(&mut client, &budget, 1 << 20, 1 << 20, None)
            .await
            .unwrap();
        assert_eq!(held.model(), Some("m"));
        assert_eq!(budget.current(), parts[0].len());
        let body = RequestBody::after_hold(held, client);
        assert_eq!(body.size_hint().lower(), parts[0].len() as u64);
        assert_eq!(collect(body).await.unwrap(), parts.concat());
        assert_eq!(budget.current(), 0);
    }

    #[tokio::test]
    async fn a_repeated_model_later_fails_the_body() {
        let budget = Gauge::default();
        let mut client = chunks(&[r#"{"model":"allowed","#, r#""model":"other"}"#]);
        let held = hold_until_model(&mut client, &budget, 1 << 20, 1 << 20, None)
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
        let held = hold_until_model(&mut client, &Gauge::default(), 1 << 20, 1 << 20, Some(4000))
            .await
            .unwrap();
        assert_eq!(held.max_tokens(), None, "not read before the hold ended");
        let error = collect(RequestBody::after_hold(held, client))
            .await
            .unwrap_err();
        let over = error.downcast_ref::<OverTokenLimit>().unwrap();
        assert_eq!((over.requested, over.limit), (9000, 4000));
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
        let error = hold_until_model(&mut client, &budget, 100, 1 << 20, None)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, HoldError::Overloaded));
        assert_eq!(budget.current(), 90);

        let mut client = chunks(&[r#"{"messages":[]}"#]);
        let error = hold_until_model(&mut client, &Gauge::default(), 100, 1 << 20, None)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, HoldError::Model(PolicyError::Missing)));

        let mut client = chunks(&[r#"{"messages":[1,2,3,4], "#, r#""model":"m"}"#]);
        let error = hold_until_model(&mut client, &Gauge::default(), 100, 10, None)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, HoldError::TooLarge));
    }
}
