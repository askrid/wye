//! A usage record's `upgrade_issues`: which fields each upgrade-check target
//! would reject, reported and never acted on.
//!
//! The request body's findings, left in an
//! [`UpgradeSlot`](crate::scanned_body::UpgradeSlot) as it was sent
//! ([`crate::scanned_body`]), are matched against each target here, by the
//! writer task, off the request path.

use gateway_core::upgrade::{self, Findings};
use serde::{Serialize, Serializer, ser::SerializeMap};

/// A usage record's `upgrade_issues`: for each target that would reject the
/// request, the rules it breaks. An empty object: checked, nothing found.
pub(super) struct UpgradeIssues<'a> {
    pub(super) findings: Findings,
    pub(super) targets: &'a [Box<str>],
}

impl UpgradeIssues<'_> {
    /// `(target, rule)` for every issue.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&str, &'static str)> {
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
