//! `GET /v1/models`: the configured model catalog, filtered by the caller's
//! model permissions, in the Anthropic list shape Claude Code's model picker
//! reads.
//!
//! Each entry is serialized once at startup and entries the configured
//! ceiling excludes are dropped then, so a request only narrows by its
//! credential's `models` claim and concatenates bytes.

use bytes::{BufMut, Bytes, BytesMut};
use gateway_core::{
    config::Discovery,
    policy::{self, ModelPattern},
};

struct Entry {
    id: Box<str>,
    /// The entry's JSON object.
    json: Box<[u8]>,
    /// The id as a JSON string, for `first_id` and `last_id`.
    id_json: Box<[u8]>,
}

/// The catalog after the configured ceiling, ready to serve.
pub(crate) struct ModelCatalog {
    entries: Vec<Entry>,
}

impl ModelCatalog {
    pub(crate) fn new(discovery: &Discovery, ceiling: Option<&[ModelPattern]>) -> Self {
        let entries = discovery
            .models
            .iter()
            .filter(|m| policy::permitted(ceiling, None, &m.id))
            .map(|m| {
                let mut object = serde_json::Map::new();
                object.insert("type".into(), "model".into());
                object.insert("id".into(), m.id.as_str().into());
                if let Some(name) = &m.display_name {
                    object.insert("display_name".into(), name.as_str().into());
                }
                if let Some(description) = &m.description {
                    object.insert("description".into(), description.as_str().into());
                }
                Entry {
                    id: m.id.as_str().into(),
                    json: serde_json::to_vec(&object).unwrap_or_default().into(),
                    id_json: serde_json::to_vec(&m.id).unwrap_or_default().into(),
                }
            })
            .collect();
        Self { entries }
    }

    /// The list response for a caller whose credential grants `granted`
    /// (`None`: no narrowing). One page: `has_more` is always false.
    pub(crate) fn list(&self, granted: Option<&[ModelPattern]>) -> Bytes {
        let mut visible = self
            .entries
            .iter()
            .filter(|e| policy::permitted(None, granted, &e.id))
            .peekable();
        let mut buf = BytesMut::with_capacity(256);
        buf.put_slice(br#"{"data":["#);
        let first = visible.peek().map(|e| &*e.id_json);
        let mut last = None;
        for (i, entry) in visible.enumerate() {
            if i > 0 {
                buf.put_u8(b',');
            }
            buf.put_slice(&entry.json);
            last = Some(&*entry.id_json);
        }
        buf.put_slice(br#"],"has_more":false,"first_id":"#);
        buf.put_slice(first.unwrap_or(b"null"));
        buf.put_slice(br#","last_id":"#);
        buf.put_slice(last.unwrap_or(b"null"));
        buf.put_u8(b'}');
        buf.freeze()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use gateway_core::config::DiscoveredModel;

    use super::*;

    fn discovery(ids: &[&str]) -> Discovery {
        Discovery {
            models: ids
                .iter()
                .map(|id| DiscoveredModel {
                    id: (*id).into(),
                    display_name: (*id == "claude-sonnet-4-6").then(|| "Sonnet \"4.6\"".into()),
                    description: None,
                })
                .collect(),
        }
    }

    fn patterns(list: &[&str]) -> Vec<ModelPattern> {
        list.iter()
            .map(|p| ModelPattern::parse(p).unwrap())
            .collect()
    }

    fn parse(bytes: Bytes) -> serde_json::Value {
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn lists_what_both_ceiling_and_credential_allow() {
        let ceiling = patterns(&["claude-*"]);
        let catalog = ModelCatalog::new(
            &discovery(&["claude-sonnet-4-6", "gpt-5", "claude-haiku-4-5"]),
            Some(&ceiling),
        );

        let all = parse(catalog.list(None));
        let ids: Vec<_> = all["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["claude-sonnet-4-6", "claude-haiku-4-5"]);
        assert_eq!(all["data"][0]["type"], "model");
        assert_eq!(all["data"][0]["display_name"], "Sonnet \"4.6\"");
        assert!(all["data"][1].get("display_name").is_none());
        assert_eq!(all["has_more"], false);
        assert_eq!(all["first_id"], "claude-sonnet-4-6");
        assert_eq!(all["last_id"], "claude-haiku-4-5");

        let haiku = parse(catalog.list(Some(&patterns(&["claude-haiku-*"]))));
        assert_eq!(haiku["data"].as_array().unwrap().len(), 1);
        assert_eq!(haiku["first_id"], "claude-haiku-4-5");

        let none = parse(catalog.list(Some(&patterns(&["claude-opus-*"]))));
        assert_eq!(none["data"].as_array().unwrap().len(), 0);
        assert!(none["first_id"].is_null() && none["last_id"].is_null());
    }
}
