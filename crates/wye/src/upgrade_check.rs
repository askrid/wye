//! Upgrade checks on forwarded request bodies: which fields each target
//! model would reject, reported in usage records and never acted on.
//!
//! [`UpgradeBody`] feeds the request bytes to an [`UpgradeScanner`] as hyper
//! sends them upstream, without changing or delaying them, and stores the
//! findings in an [`UpgradeSlot`] when the body ends. The writer task turns
//! them into `upgrade_issues` per target, off the request path.

use std::{
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use bytes::Bytes;
use gateway_core::upgrade::{self, Findings, UpgradeScanner};
use hyper::body::{Body, Frame, SizeHint};
use serde::{Serialize, Serializer, ser::SerializeMap};

/// Findings of one request body, set once the whole body was read. Shared
/// between the request body (in hyper's client) and the usage record.
pub(crate) type UpgradeSlot = Arc<OnceLock<Findings>>;

/// Request body wrapper that scans for upgrade issues when the request is sampled.
pub(crate) struct UpgradeBody<B> {
    inner: B,
    /// Boxed so the wrapper stays small when checks are off.
    scan: Option<(Box<UpgradeScanner>, UpgradeSlot)>,
}

impl<B> UpgradeBody<B> {
    pub(crate) fn new(inner: B, slot: Option<UpgradeSlot>) -> Self {
        Self {
            inner,
            scan: slot.map(|slot| (Box::new(UpgradeScanner::new()), slot)),
        }
    }

    fn finish(&mut self) {
        if let Some((scanner, slot)) = self.scan.take() {
            let _ = slot.set(scanner.findings());
        }
    }
}

impl<B> Body for UpgradeBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        if let Some((scanner, _)) = this.scan.as_mut() {
            match &polled {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        scanner.feed(data);
                    }
                    // With a known length, hyper stops polling once the body
                    // reports its end and never sees `Ready(None)`.
                    if this.inner.is_end_stream() {
                        this.finish();
                    }
                }
                Poll::Ready(None) => this.finish(),
                // An error or a dropped body leaves the slot empty: not reported.
                Poll::Ready(Some(Err(_))) | Poll::Pending => {}
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// `upgrade_issues` of a usage record: for each target that would reject the
/// request, the rules it breaks. An empty object: checked, nothing found.
pub(crate) struct UpgradeIssues<'a> {
    pub(crate) findings: Findings,
    pub(crate) targets: &'a [Box<str>],
}

impl UpgradeIssues<'_> {
    /// `(target, rule)` for every issue.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &'static str)> {
        self.targets.iter().flat_map(|target| {
            upgrade::issues(&self.findings, target).map(move |rule| (&**target, rule.name()))
        })
    }
}

impl Serialize for UpgradeIssues<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for target in self.targets.iter() {
            if upgrade::issues(&self.findings, target).next().is_some() {
                map.serialize_entry(&**target, &Rules(&self.findings, target))?;
            }
        }
        map.end()
    }
}

/// The rules one target breaks, as a JSON array of names.
struct Rules<'a>(&'a Findings, &'a str);

impl Serialize for Rules<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(upgrade::issues(self.0, self.1).map(|r| r.name()))
    }
}
