//! `GET /v1/models`: the configured model catalog, filtered by the caller's
//! model permissions, in the Anthropic list shape Claude Code's model picker
//! reads.
//!
//! Each entry is serialized once at startup and entries the configured
//! ceiling excludes are dropped then, so a request only narrows by its
//! credential's `models` claim and concatenates bytes. Aliases follow the
//! models; each is listed when the model it resolves to for the caller is
//! permitted, since rollout rules make that per subject.

use bytes::{BufMut, Bytes, BytesMut};
use gateway_core::{
    alias::{Alias, Aliases},
    config::Discovery,
    policy::{self, ModelPattern},
};

struct Entry {
    /// The model checked against permissions; for an alias, resolved per caller.
    id: Box<str>,
    alias: Option<Alias>,
    /// The entry's JSON object.
    json: Box<[u8]>,
    /// The id as a JSON string, for `first_id` and `last_id`.
    id_json: Box<[u8]>,
}

/// The catalog after the configured ceiling, ready to serve.
pub(crate) struct ModelCatalog {
    entries: Vec<Entry>,
    /// Applied per request to aliases, whose model depends on the caller.
    ceiling: Option<Box<[ModelPattern]>>,
}

impl ModelCatalog {
    pub(crate) fn new(
        discovery: &Discovery,
        aliases: Option<&Aliases>,
        ceiling: Option<&[ModelPattern]>,
    ) -> Self {
        let mut entries: Vec<Entry> = discovery
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
                    alias: None,
                    json: serde_json::to_vec(&object).unwrap_or_default().into(),
                    id_json: serde_json::to_vec(&m.id).unwrap_or_default().into(),
                }
            })
            .collect();
        for alias in aliases.into_iter().flat_map(Aliases::iter) {
            let id = serde_json::to_vec(&**alias.name()).unwrap_or_default();
            let mut json = br#"{"type":"model","id":"#.to_vec();
            json.extend_from_slice(&id);
            json.push(b'}');
            entries.push(Entry {
                id: (**alias.name()).into(),
                alias: Some(alias.clone()),
                json: json.into(),
                id_json: id.into(),
            });
        }
        Self {
            entries,
            ceiling: ceiling.map(Into::into),
        }
    }

    /// The list response for a caller whose credential grants `granted`
    /// (`None`: no narrowing) and names `subject`. One page: `has_more` is
    /// always false.
    pub(crate) fn list(&self, granted: Option<&[ModelPattern]>, subject: Option<&str>) -> Bytes {
        let mut visible = self
            .entries
            .iter()
            .filter(|e| match &e.alias {
                None => policy::permitted(None, granted, &e.id),
                Some(alias) => {
                    policy::permitted(self.ceiling.as_deref(), granted, alias.model_for(subject))
                }
            })
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
            None,
            Some(&ceiling),
        );

        let all = parse(catalog.list(None, None));
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

        let haiku = parse(catalog.list(Some(&patterns(&["claude-haiku-*"])), None));
        assert_eq!(haiku["data"].as_array().unwrap().len(), 1);
        assert_eq!(haiku["first_id"], "claude-haiku-4-5");

        let none = parse(catalog.list(Some(&patterns(&["claude-opus-*"])), None));
        assert_eq!(none["data"].as_array().unwrap().len(), 0);
        assert!(none["first_id"].is_null() && none["last_id"].is_null());
    }

    #[test]
    fn aliases_are_listed_when_their_model_for_the_caller_is_permitted() {
        let ceiling = patterns(&["claude-haiku-*", "claude-sonnet-*"]);
        let rollout = (vec!["canary".to_string()], "claude-haiku-5-5".to_string());
        let aliases = Aliases::new(vec![
            Alias::new("coding", "claude-opus-5-5", [rollout]).unwrap(),
            Alias::new("fast", "claude-sonnet-5-5", []).unwrap(),
        ])
        .unwrap();
        let catalog = ModelCatalog::new(
            &discovery(&["claude-haiku-4-5"]),
            Some(&aliases),
            Some(&ceiling),
        );
        let ids = |subject| {
            parse(catalog.list(None, subject))["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        // `coding` is Opus, outside the ceiling, except for the canary subject.
        assert_eq!(ids(None), ["claude-haiku-4-5", "fast"]);
        assert_eq!(ids(Some("canary")), ["claude-haiku-4-5", "coding", "fast"]);
    }
}
