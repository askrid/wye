//! Upgrade checks on forwarded request bodies: which fields each target
//! model would reject, reported in usage records and never acted on.
//!
//! An [`UpgradeScanner`] reads the request bytes as they are sent
//! ([`crate::scanned_body`]) and leaves its findings in an [`UpgradeSlot`].
//! The writer task turns them into `upgrade_issues` per target, off the
//! request path.

use gateway_core::upgrade::{self, Findings, UpgradeScanner};
use serde::{Serialize, Serializer, ser::SerializeMap};

use crate::scanned_body::{BodyScanner, Slot};

/// Upgrade-check findings of one request body ([`Slot`]).
pub(crate) type UpgradeSlot = Slot<Findings>;

impl BodyScanner for UpgradeScanner {
    type Output = Findings;

    fn feed(&mut self, chunk: &[u8]) {
        UpgradeScanner::feed(self, chunk);
    }

    fn finish(self) -> Option<Findings> {
        Some(self.findings())
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
