//! Per-call aliases for evidence only; source text and thread IDs stay intact.
use super::*;

pub(super) struct References(Vec<String>);
impl References {
    pub(super) fn new(ids: impl IntoIterator<Item = String>) -> Self {
        Self(
            ids.into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    }
    pub(super) fn encode(&self, value: &mut Value) {
        self.visit(value, false, false);
    }
    pub(super) fn decode(&self, value: &mut Value) {
        self.visit(value, true, false);
    }
    fn visit(&self, value: &mut Value, decode: bool, reference: bool) {
        match value {
            Value::Object(fields) => {
                for (key, child) in fields {
                    self.visit(
                        child,
                        decode,
                        matches!(
                            key.as_str(),
                            "id" | "evidence"
                                | "select"
                                | "selected"
                                | "own"
                                | "allowed_evidence"
                                | "fragments"
                                | "enum"
                        ),
                    );
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.visit(item, decode, reference);
                }
            }
            _ if reference => {
                if decode {
                    let number = value
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| value.as_u64().map(|n| n.to_string()));
                    if let Some(id) = number
                        .as_ref()
                        .and_then(|s| {
                            s.parse::<usize>()
                                .ok()
                                .filter(|n| *n > 0 && n.to_string() == *s)
                        })
                        .and_then(|n| self.0.get(n - 1))
                    {
                        *value = json!(id);
                    } else if let Some(number) = value.as_u64() {
                        // Keep unknown IDs invalid, allowing optional links to be
                        // discarded by their existing validator instead of panicking.
                        *value = json!(number.to_string());
                    }
                } else if let Some(n) = value
                    .as_str()
                    .and_then(|id| self.0.iter().position(|s| s == id))
                {
                    *value = json!((n + 1).to_string());
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aliases_round_trip_without_rewriting_source_text_or_routing() {
        let refs = References::new(["doc:L2".into(), "doc:L1".into()]);
        let original = json!({"originals":[{"id":"doc:L1","text":"doc:L1"}],"select":["doc:L2"],"aspects":[{"evidence":["doc:L1"]}],"need":["thread"],"schema":{"enum":["doc:L2"]}});
        let mut v = original.clone();
        refs.encode(&mut v);
        assert_eq!(v["originals"][0]["id"], "1");
        assert_eq!(v["originals"][0]["text"], "doc:L1");
        assert_eq!(v["select"], json!(["2"]));
        refs.decode(&mut v);
        assert_eq!(v, original);
        let mut bad = json!({"evidence":[0,"01","999",2]});
        refs.decode(&mut bad);
        assert_eq!(bad["evidence"], json!(["0", "01", "999", "doc:L2"]));
        let other = References::new(["other:L1".into()]);
        let mut v = json!({"evidence":["1"]});
        other.decode(&mut v);
        assert_eq!(v["evidence"], json!(["other:L1"]));
    }
}
