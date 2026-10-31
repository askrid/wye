//! Model aliases: a stable name a client asks for, resolved by config to the
//! model that runs, optionally per credential subject for staged rollouts.
//!
//! Resolution is one step: an alias names a model, never another alias, so a
//! config change moves exactly the traffic it says.

use std::sync::Arc;

use crate::policy::{self, ModelPattern};

/// Most aliases in one config: resolution scans them linearly.
pub const MAX_ALIASES: usize = 256;
/// Most `when` rules per alias.
pub const MAX_ALIAS_RULES: usize = 32;

#[derive(Debug, Clone)]
pub struct Alias {
    name: Arc<str>,
    model: Arc<str>,
    when: Box<[Rule]>,
}

/// A rollout rule: credentials whose subject matches get `model`.
#[derive(Debug, Clone)]
struct Rule {
    subjects: Box<[ModelPattern]>,
    model: Arc<str>,
}

/// Why an alias was rejected; the message names the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AliasError(pub &'static str);

impl Alias {
    /// `name` and every model are exact names of 1-256 bytes without `*`;
    /// subjects are exact names or prefixes ending in `*`, as in model
    /// permissions.
    pub fn new(
        name: &str,
        model: &str,
        when: impl IntoIterator<Item = (Vec<String>, String)>,
    ) -> Result<Self, AliasError> {
        if !policy::is_model_name(name) {
            return Err(AliasError(
                "aliases: name must be 1-256 bytes without * or escapes",
            ));
        }
        if !policy::is_model_name(model) {
            return Err(AliasError(
                "aliases: model must be a model name of 1-256 bytes without *",
            ));
        }
        let mut rules = Vec::new();
        for (subjects, model) in when {
            if rules.len() == MAX_ALIAS_RULES {
                return Err(AliasError("aliases.when: at most 32 rules"));
            }
            if !policy::is_model_name(&model) {
                return Err(AliasError(
                    "aliases.when: model must be a model name of 1-256 bytes without *",
                ));
            }
            if subjects.is_empty() {
                return Err(AliasError("aliases.when: subjects must not be empty"));
            }
            let subjects = subjects
                .iter()
                .map(|s| ModelPattern::parse(s))
                .collect::<Option<Box<[_]>>>()
                .ok_or(AliasError(
                    "aliases.when: a subject is a name or a prefix ending in *",
                ))?;
            rules.push(Rule {
                subjects,
                model: model.into(),
            });
        }
        Ok(Self {
            name: name.into(),
            model: model.into(),
            when: rules.into(),
        })
    }

    pub fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// Every model the alias can resolve to.
    pub fn models(&self) -> impl Iterator<Item = &Arc<str>> {
        std::iter::once(&self.model).chain(self.when.iter().map(|rule| &rule.model))
    }

    /// The model for a caller: the first rule whose subjects match, else the
    /// alias's model. Callers without a credential get the alias's model.
    pub fn model_for(&self, subject: Option<&str>) -> &Arc<str> {
        subject
            .and_then(|subject| {
                self.when
                    .iter()
                    .find(|rule| rule.subjects.iter().any(|p| p.matches(subject)))
            })
            .map_or(&self.model, |rule| &rule.model)
    }
}

/// The configured aliases; names are unique.
#[derive(Debug, Clone)]
pub struct Aliases(Box<[Alias]>);

impl Aliases {
    /// Fails on a repeated name, an alias that resolves to another alias's
    /// name (resolution is one step), or more than [`MAX_ALIASES`] aliases.
    pub fn new(aliases: Vec<Alias>) -> Result<Self, AliasError> {
        if aliases.len() > MAX_ALIASES {
            return Err(AliasError("aliases: at most 256 aliases"));
        }
        for (i, alias) in aliases.iter().enumerate() {
            if aliases.iter().take(i).any(|a| a.name == alias.name) {
                return Err(AliasError("aliases: duplicate name"));
            }
            if alias
                .models()
                .any(|model| aliases.iter().any(|a| a.name == *model))
            {
                return Err(AliasError(
                    "aliases: a model must not be another alias's name; resolution is one step",
                ));
            }
        }
        Ok(Self(aliases.into()))
    }

    /// The alias a request names, if `model` is one.
    pub fn get(&self, model: &str) -> Option<&Alias> {
        self.0.iter().find(|a| &*a.name == model)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Alias> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn rule(subjects: &[&str], model: &str) -> (Vec<String>, String) {
        (
            subjects.iter().map(|s| s.to_string()).collect(),
            model.to_string(),
        )
    }

    #[test]
    fn resolves_per_subject_with_the_first_matching_rule() {
        let alias = Alias::new(
            "coding",
            "claude-opus-5-5",
            [
                rule(&["team-canary-*"], "claude-sonnet-5-5"),
                rule(&["team-canary-a", "ci"], "claude-haiku-5-5"),
            ],
        )
        .unwrap();
        assert_eq!(&**alias.model_for(None), "claude-opus-5-5");
        assert_eq!(&**alias.model_for(Some("team-main")), "claude-opus-5-5");
        assert_eq!(
            &**alias.model_for(Some("team-canary-a")),
            "claude-sonnet-5-5"
        );
        assert_eq!(&**alias.model_for(Some("ci")), "claude-haiku-5-5");
    }

    #[test]
    fn rejects_bad_names_rules_and_duplicates() {
        assert!(Alias::new("", "m", []).is_err());
        assert!(Alias::new("a*", "m", []).is_err());
        assert!(Alias::new("a\"b", "m", []).is_err());
        assert!(Alias::new("a", "m*", []).is_err());
        assert!(Alias::new("a", "m", [rule(&[], "n")]).is_err());
        assert!(Alias::new("a", "m", [rule(&["x*y"], "n")]).is_err());
        assert!(Alias::new("a", "m", [rule(&["x"], "n*")]).is_err());
        let a = Alias::new("a", "m", []).unwrap();
        assert!(Aliases::new(vec![a.clone(), a.clone()]).is_err());
        let aliases = Aliases::new(vec![a.clone()]).unwrap();
        assert!(aliases.get("a").is_some());
        assert!(aliases.get("m").is_none());
        // A chain, in its model or a rollout rule's.
        let to_a = Alias::new("b", "a", []).unwrap();
        assert!(Aliases::new(vec![a.clone(), to_a]).is_err());
        let rule_to_a = Alias::new("b", "m", [rule(&["x"], "a")]).unwrap();
        assert!(Aliases::new(vec![a, rule_to_a]).is_err());
    }
}
