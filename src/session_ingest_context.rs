//! Bounded topic discovery for session extraction. Selection never edits storage.
use super::*;

const TOPIC_CHARS: usize = 24_000;
const TOPIC_COUNT: usize = 8;
const CATALOG_PAGE: usize = 32;
const EVIDENCE_CHARS: usize = 8_000;
pub(super) const MAX_CALLS: usize = 8;
pub(super) const INSTRUCTIONS: &str = r#"Selective context: topics contains ONLY loaded topics, not all stored memory. topic_catalog is a paginated discovery preview, never full evidence. Before changing an existing topic, load its full claims. Request context by returning {"summary":"","updates":[],"context_request":{"topics":["existing-key"],"catalog_page":0}}. topics REPLACES the loaded selection; include previously selected keys if still needed. Use the current page number unless browsing another catalog page. At most 8 topics per request, with a 24000-character target; a single oversized topic is allowed intact. A context request makes no memory changes. For a final patch return the usual summary and updates plus context_request=null. Preserve all unrelated facts and global summary information; do not treat unloaded topics or omitted evidence previews as absent. Before creating a new topic, check the catalog for an existing equivalent; browse unviewed pages if needed. viewed_pages lists pages already examined during this batch. On ambiguity request context instead of guessing. At most 8 total calls share the ingestion deadline."#;

pub(super) struct Context {
    pub selective: bool,
    keys: BTreeSet<String>,
    catalog: Vec<String>,
    page: usize,
    viewed_pages: BTreeSet<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    topics: Vec<String>,
    catalog_page: usize,
}
fn topic_size(topic: &Topic) -> usize {
    // Count the serialized representation, including IDs, links and provenance.
    serde_json::to_string(topic)
        .expect("serializable topic")
        .chars()
        .count()
}
impl Context {
    pub fn new(state: &State, events: &[Event]) -> Self {
        let selective = state.topics.len() > TOPIC_COUNT
            || state.topics.values().map(topic_size).sum::<usize>() > TOPIC_CHARS;
        let words: BTreeSet<String> = state
            .recent
            .iter()
            .chain(events)
            .flat_map(|e| {
                e.text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.chars().count() > 2)
                    .map(str::to_lowercase)
            })
            .collect();
        let mut ranked: Vec<_> = state
            .topics
            .values()
            .map(|t| {
                let text = format!(
                    "{} {} {}",
                    t.key,
                    t.title.chars().take(160).collect::<String>(),
                    t.memory.chars().take(2000).collect::<String>()
                )
                .to_lowercase();
                let topic_words: BTreeSet<_> = text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .collect();
                let score = topic_words.iter().filter(|w| words.contains(**w)).count();
                (score, t.key.clone())
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        Self {
            selective,
            keys: if selective {
                BTreeSet::new()
            } else {
                state.topics.keys().cloned().collect()
            },
            catalog: ranked.into_iter().map(|(_, k)| k).collect(),
            page: 0,
            viewed_pages: BTreeSet::from([0]),
        }
    }
    pub fn input(&self, state: &State, events: &[Event]) -> (Value, BTreeMap<String, Event>) {
        let topics: BTreeMap<_, _> = self.keys.iter().map(|k| (k, &state.topics[k])).collect();
        let source_ids: BTreeSet<_> = topics
            .values()
            .flat_map(|t| &t.claims)
            .flat_map(|c| &c.sources)
            .collect();
        let mut supplied: BTreeMap<_, _> = source_ids
            .iter()
            .filter_map(|id| state.evidence.get(*id).map(|e| (e.id.clone(), e.clone())))
            .collect();
        for e in state.recent.iter().chain(events) {
            supplied.insert(e.id.clone(), e.clone());
        }
        let new_ids: BTreeSet<_> = events.iter().map(|e| &e.id).collect();
        let mut remaining = EVIDENCE_CHARS;
        let mut evidence = Vec::new();
        // Preserve immediate conversational context before older source previews.
        let mut ordered: Vec<_> = state.recent.iter().rev().map(|e| e.id.clone()).collect();
        ordered.extend(source_ids.into_iter().cloned());
        let mut included = BTreeSet::new();
        for id in ordered {
            if new_ids.contains(&id) || !included.insert(id.clone()) {
                continue;
            }
            let Some(e) = supplied.get(&id) else {
                continue;
            };
            if remaining == 0 || evidence.len() == 32 {
                break;
            }
            let text: String = e.text.chars().take(remaining.min(2000)).collect();
            remaining -= text.chars().count();
            evidence.push(json!({"id":e.id,"kind":e.kind,"timestamp":e.timestamp,"truncated":text.chars().count()<e.text.chars().count(),"text":text}));
        }
        let mut input = json!({"evidence_scope":{
            "memory_scope":"current_session_only","project_inventory_provided":false,"conversation_complete":false,
            "topics_complete":!self.selective || self.keys.len()==state.topics.len(),
            "empty_topics_mean":if state.topics.is_empty() {"no previously imported topics for this session"} else {"no topics loaded; stored topics may exist"},
            "previous_evidence_complete":evidence.len()==supplied.keys().filter(|id| !new_ids.contains(id)).count() && evidence.iter().all(|e|e["truncated"]==false)
        },"current_summary":state.summary.chars().take(2000).collect::<String>(),"summary_truncated":state.summary.chars().count()>2000,"topics":topics,"previous_evidence":evidence,"new_events":events});
        if self.selective {
            let entries: Vec<_> = self.catalog.iter().skip(self.page*CATALOG_PAGE).take(CATALOG_PAGE).map(|key| {
                let t=&state.topics[key];
                json!({"key":key,"title":t.title.chars().take(80).collect::<String>(),"preview":t.memory.chars().take(160).collect::<String>(),"claim_count":t.claims.len(),"context_chars":topic_size(t)})
            }).collect();
            input["topic_catalog"] = json!({"page":self.page,"viewed_pages":self.viewed_pages,"pages":self.catalog.len().div_ceil(CATALOG_PAGE),"total_topics":self.catalog.len(),"entries":entries});
        }
        (input, supplied)
    }
    pub fn response_schema(&self, ids: &BTreeSet<String>) -> Value {
        let mut value = schema(ids);
        if self.selective {
            value["properties"]["context_request"] = json!({"anyOf":[{"type":"null"},{"type":"object","additionalProperties":false,"required":["topics","catalog_page"],"properties":{"topics":{"type":"array","items":{"type":"string"}},"catalog_page":{"type":"integer","minimum":0}}}]});
            value["required"]
                .as_array_mut()
                .unwrap()
                .push(json!("context_request"));
        }
        value
    }
    /// Returns None for discovery, or a patch after checking loaded-topic boundaries.
    pub fn response(&mut self, state: &State, text: &str) -> Result<Option<Patch>> {
        if !self.selective {
            return parse_patch(state, text).map(Some);
        }
        let mut value: Value = serde_json::from_str(text)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| AppError::new("expected session response object"))?;
        if let Some(request) = object.remove("context_request").filter(|v| !v.is_null()) {
            if object.len() != 2
                || object.get("summary") != Some(&json!(""))
                || object.get("updates") != Some(&json!([]))
            {
                return Err(AppError::new(
                    "context request cannot include memory changes",
                ));
            }
            let request: Request = serde_json::from_value(request)?;
            let keys: BTreeSet<_> = request.topics.iter().cloned().collect();
            if keys.len() != request.topics.len()
                || keys.len() > TOPIC_COUNT
                || keys.iter().any(|k| !state.topics.contains_key(k))
                || request.catalog_page >= self.catalog.len().div_ceil(CATALOG_PAGE)
            {
                return Err(AppError::new("invalid session context request: unknown/duplicate topics, too many topics or catalog page out of range"));
            }
            if keys.len() > 1
                && keys
                    .iter()
                    .map(|k| topic_size(&state.topics[k]))
                    .sum::<usize>()
                    > TOPIC_CHARS
            {
                return Err(AppError::new("session context selection exceeds 24000 characters; request fewer topics or one oversized topic"));
            }
            if keys == self.keys && request.catalog_page == self.page {
                return Err(AppError::new("session context request made no progress"));
            }
            self.keys = keys;
            self.page = request.catalog_page;
            self.viewed_pages.insert(self.page);
            return Ok(None);
        }
        let updates = object
            .get("updates")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::new("expected session updates array"))?;
        for update in updates {
            if let Some(key) = update["key"].as_str() {
                if state.topics.contains_key(key) && !self.keys.contains(key) {
                    return Err(AppError::new(format!(
                        "topic {key} is not loaded; request its context before updating it"
                    )));
                }
            }
        }
        parse_patch(state, &value.to_string()).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(count: usize) -> State {
        let mut s = State::new("test");
        for i in 0..count {
            let key = format!("topic-{i:03}");
            s.topics.insert(
                key.clone(),
                Topic {
                    key,
                    title: format!("Title {i}"),
                    memory: format!("Summary {i}"),
                    claims: vec![],
                    related: vec![],
                },
            );
        }
        s
    }
    fn request(keys: &[&str], page: usize) -> String {
        json!({"summary":"","updates":[],"context_request":{"topics":keys,"catalog_page":page}})
            .to_string()
    }
    #[test]
    fn catalogs_are_paged_and_requests_are_validated_without_mutation() {
        let s = state(70);
        let mut c = Context::new(&s, &[]);
        let (input, _) = c.input(&s, &[]);
        assert_eq!(
            input["topic_catalog"]["entries"].as_array().unwrap().len(),
            32
        );
        assert_eq!(input["topic_catalog"]["pages"], 3);
        assert!(c.response(&s, &request(&[], 2)).unwrap().is_none());
        let (input, _) = c.input(&s, &[]);
        assert_eq!(
            input["topic_catalog"]["entries"].as_array().unwrap().len(),
            6
        );
        for r in [
            request(&["missing"], 0),
            request(&["topic-000", "topic-000"], 0),
            request(&[], 3),
            request(&[], 2),
        ] {
            assert!(c.response(&s, &r).is_err());
            assert_eq!(c.page, 2);
            assert!(c.keys.is_empty());
        }
        assert!(c
            .response(&s, &request(&["topic-069"], 2))
            .unwrap()
            .is_none());
        assert_eq!(c.input(&s, &[]).0["topics"].as_object().unwrap().len(), 1);
    }
    #[test]
    fn one_large_topic_loads_intact_but_many_large_topics_do_not_accumulate() {
        let mut s = state(2);
        for topic in s.topics.values_mut() {
            topic.memory = "я".repeat(25000);
        }
        let mut c = Context::new(&s, &[]);
        assert!(c.selective);
        assert!(c
            .response(&s, &request(&["topic-000", "topic-001"], 0))
            .is_err());
        c.response(&s, &request(&["topic-000"], 0)).unwrap();
        assert_eq!(
            c.input(&s, &[]).0["topics"]["topic-000"]["memory"],
            "я".repeat(25000)
        );
        c.response(&s, &request(&["topic-001"], 0)).unwrap();
        let input = c.input(&s, &[]).0;
        assert!(input["topics"].get("topic-000").is_none());
        assert_eq!(input["topics"].as_object().unwrap().len(), 1);
    }
    #[test]
    fn hidden_evidence_is_excluded_and_visible_previews_have_a_total_budget() {
        let mut s = state(9);
        for i in 0..9 {
            let id = format!("source-{i}");
            s.evidence.insert(
                id.clone(),
                Event {
                    id: id.clone(),
                    timestamp: "now".into(),
                    kind: "user".into(),
                    text: "я".repeat(5000),
                    file: "rollout".into(),
                    line: 1,
                },
            );
            s.topics
                .get_mut(&format!("topic-{i:03}"))
                .unwrap()
                .claims
                .push(Claim {
                    id: format!("claim-{i}"),
                    change_reason: String::new(),
                    replaces: vec![],
                    kind: "requirement".into(),
                    status: "requested".into(),
                    text: "Rule".into(),
                    sources: vec![id],
                });
        }
        let mut c = Context::new(&s, &[]);
        assert!(c.input(&s, &[]).1.is_empty());
        c.response(
            &s,
            &request(
                &[
                    "topic-000",
                    "topic-001",
                    "topic-002",
                    "topic-003",
                    "topic-004",
                ],
                0,
            ),
        )
        .unwrap();
        let (input, supplied) = c.input(&s, &[]);
        assert_eq!(supplied.len(), 5);
        assert!(!supplied.contains_key("source-8"));
        let previews = input["previous_evidence"].as_array().unwrap();
        assert_eq!(
            previews
                .iter()
                .map(|e| e["text"].as_str().unwrap().chars().count())
                .sum::<usize>(),
            EVIDENCE_CHARS
        );
        assert_eq!(input["evidence_scope"]["previous_evidence_complete"], false);
    }
}
