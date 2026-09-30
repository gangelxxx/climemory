//! Append-only, context-scoped public aliases. Canonical receipts never use these.
use crate::util::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const CAPACITY: usize = 512;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Aliases {
    next: u64,
    entries: BTreeMap<String, String>,
}

impl Default for Aliases {
    fn default() -> Self {
        Self {
            next: 1,
            entries: BTreeMap::new(),
        }
    }
}

fn canonical(id: &str) -> bool {
    id.len() == 17
        && id.starts_with('e')
        && id.as_bytes()[1..]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

impl Aliases {
    pub(super) fn validate(&self) -> Result<()> {
        if self.entries.len() > CAPACITY
            || self.next != self.entries.len() as u64 + 1
            || !self.entries.keys().all(|id| canonical(id))
        {
            return Err(AppError::new(
                "invalid context evidence aliases; start a new topic without @context",
            ));
        }
        let expected: BTreeSet<_> = (1..=self.entries.len()).map(|n| format!("e{n}")).collect();
        if self.entries.values().cloned().collect::<BTreeSet<_>>() != expected {
            return Err(AppError::new(
                "invalid context evidence aliases; start a new topic without @context",
            ));
        }
        Ok(())
    }

    /// Reserve every reference from the FULL rendered reply, before summary pruning.
    pub(super) fn reserve(&mut self, full: &Value) -> Result<()> {
        self.validate()?;
        let mut ids = BTreeSet::new();
        visit(full, &mut |id| {
            if canonical(id) {
                ids.insert(id.to_owned());
            }
        });
        for id in ids {
            if self.entries.len() == CAPACITY {
                break;
            }
            if !self.entries.contains_key(&id) {
                self.entries.insert(id, format!("e{}", self.next));
                self.next += 1;
            }
        }
        Ok(())
    }

    pub(super) fn project(&self, output: &mut Value) {
        // This is deliberately a field-directed walk, never a text substitution.
        if let Some(rows) = output.get_mut("evidence").and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(id) = row.get_mut("ref") {
                    self.replace(id);
                }
            }
        }
        for field in ["aspects", "conflicts"] {
            if let Some(rows) = output.get_mut(field).and_then(Value::as_array_mut) {
                for row in rows {
                    if let Some(ids) = row.get_mut("evidence").and_then(Value::as_array_mut) {
                        for id in ids {
                            self.replace(id);
                        }
                    }
                }
            }
        }
        if let Some(ids) = output
            .get_mut("reused_evidence")
            .and_then(Value::as_array_mut)
        {
            for id in ids {
                self.replace(id);
            }
        }
    }

    fn replace(&self, value: &mut Value) {
        if let Some(alias) = value.as_str().and_then(|id| self.entries.get(id)) {
            *value = json!(alias);
        }
    }
}

fn visit(value: &Value, visitor: &mut impl FnMut(&str)) {
    for row in value["evidence"].as_array().into_iter().flatten() {
        if let Some(id) = row["ref"].as_str() {
            visitor(id);
        }
    }
    for field in ["aspects", "conflicts"] {
        for row in value[field].as_array().into_iter().flatten() {
            for id in row["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                visitor(id);
            }
        }
    }
    for id in value["reused_evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        visitor(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(n: usize) -> String {
        format!("e{n:016x}")
    }
    #[test]
    fn persistence_and_partial_conflict_projection_preserve_literals_and_addresses() {
        let mut aliases = Aliases::default();
        let mut value = json!({"status":"partial","evidence":[{"ref":id(2),"quote":id(2),"source":"rules.md","line":7}],"aspects":[{"evidence":[id(2)],"question":id(2)}],"conflicts":[{"evidence":[id(1)]}],"reused_evidence":[id(2)]});
        aliases.reserve(&value).unwrap();
        let aliases: Aliases =
            serde_json::from_str(&serde_json::to_string(&aliases).unwrap()).unwrap();
        aliases.validate().unwrap();
        aliases.project(&mut value);
        assert_eq!(value["evidence"][0]["ref"], "e2");
        assert_eq!(value["evidence"][0]["quote"], id(2));
        assert_eq!(value["evidence"][0]["line"], 7);
        assert_eq!(value["aspects"][0]["question"], id(2));
        assert_eq!(value["aspects"][0]["evidence"], json!(["e2"]));
        assert_eq!(value["conflicts"][0]["evidence"], json!(["e1"]));
        assert_eq!(value["reused_evidence"], json!(["e2"]));
    }
    #[test]
    fn capacity_never_evicts_or_reuses_an_alias_and_topics_are_independent() {
        let mut a = Aliases::default();
        a.reserve(
            &json!({"evidence":(0..CAPACITY).map(|n|json!({"ref":id(n)})).collect::<Vec<_>>()}),
        )
        .unwrap();
        let before = serde_json::to_value(&a).unwrap();
        let mut next = json!({"evidence":[{"ref":id(CAPACITY)},{"ref":id(0)}]});
        a.reserve(&next).unwrap();
        assert_eq!(serde_json::to_value(&a).unwrap(), before);
        a.project(&mut next);
        assert_eq!(next["evidence"][0]["ref"], id(CAPACITY));
        assert_eq!(next["evidence"][1]["ref"], "e1");
        let mut b = Aliases::default();
        b.reserve(&json!({"evidence":[{"ref":id(CAPACITY)}]}))
            .unwrap();
        assert_eq!(b.entries[&id(CAPACITY)], "e1");
        assert!(!a.entries.contains_key(&id(CAPACITY)));
    }
    #[test]
    fn corrupt_state_is_rejected_without_resetting_numbers() {
        let oversized = Aliases {
            next: (CAPACITY + 2) as u64,
            entries: (0..=CAPACITY)
                .map(|n| (id(n), format!("e{}", n + 1)))
                .collect(),
        };
        assert!(oversized.validate().is_err());
        for bad in [
            json!({"next":1,"entries":{id(1):"e1"}}),
            json!({"next":3,"entries":{id(1):"e1",id(2):"e1"}}),
            json!({"next":2,"entries":{"notcanonical":"e1"}}),
            json!({"next":2,"entries":{id(1):"e2"}}),
        ] {
            let mut a: Aliases = serde_json::from_value(bad.clone()).unwrap();
            assert!(a.validate().is_err());
            assert!(a.reserve(&json!({})).is_err());
            assert_eq!(serde_json::to_value(a).unwrap(), bad);
        }
    }
}
