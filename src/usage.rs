//! Reported provider counters only. Missing usage is never estimated as zero.
use serde_json::{json, Value};

#[derive(Default)]
pub(crate) struct Meter {
    pub attempted: bool,
    pub records: usize,
    observations: usize,
    pub usage: Value,
}

const FIELDS: &[&str] = &[
    "input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "cost_usd",
];

impl Meter {
    pub fn observe(&mut self, raw: &Value, cost_usd: bool) {
        self.observations += 1;
        let mut clean = json!({});
        for field in FIELDS {
            let value = if *field == "cost_usd" {
                if cost_usd {
                    raw["cost"]
                        .as_f64()
                        .filter(|v| v.is_finite() && *v >= 0.0)
                        .map(|v| json!(v))
                } else {
                    None
                }
            } else {
                raw[*field].as_u64().map(|v| json!(v))
            };
            clean[*field] = value.unwrap_or(Value::Null);
        }
        for field in ["cached_input_tokens", "cache_write_input_tokens"] {
            if clean[field]
                .as_u64()
                .zip(clean["input_tokens"].as_u64())
                .is_some_and(|(c, i)| c > i)
            {
                clean[field] = Value::Null;
            }
        }
        if clean["reasoning_output_tokens"]
            .as_u64()
            .zip(clean["output_tokens"].as_u64())
            .is_some_and(|(r, o)| r > o)
        {
            clean["reasoning_output_tokens"] = Value::Null;
        }
        if FIELDS.iter().any(|f| !clean[*f].is_null()) {
            self.records += 1;
        }
        if self.observations == 1 {
            self.usage = clean;
        } else {
            for field in FIELDS {
                self.usage[*field] = if *field == "cost_usd" {
                    self.usage[*field]
                        .as_f64()
                        .zip(clean[*field].as_f64())
                        .map(|(a, b)| json!(a + b))
                        .unwrap_or(Value::Null)
                } else {
                    self.usage[*field]
                        .as_u64()
                        .zip(clean[*field].as_u64())
                        .and_then(|(a, b)| a.checked_add(b))
                        .map(|v| json!(v))
                        .unwrap_or(Value::Null)
                };
            }
        }
    }
    pub fn codex_event(&mut self, event: &crate::agent_provider::ProviderEvent) {
        if let Ok(value) = serde_json::from_str::<Value>(&event.raw_json) {
            if value["type"] == "turn.completed" {
                self.observe(&value["usage"], false);
            }
        }
    }
    pub fn attach(&self, event: &mut Value) {
        event["usage"] = self.usage.clone();
        event["usage_records"] = json!(self.records);
    }
}

pub(crate) fn summarize(events: &[Value]) -> Value {
    let mut groups = std::collections::BTreeMap::<(String, String, String), Vec<&Value>>::new();
    for event in events {
        let role = match event["event"].as_str() {
            Some("model_call") => "agent",
            Some("classifier_call") => "classifier",
            _ => continue,
        };
        groups
            .entry((
                role.into(),
                event["provider"].as_str().unwrap_or("unknown").into(),
                event["model"].as_str().unwrap_or("unknown").into(),
            ))
            .or_default()
            .push(event);
    }
    json!(groups.into_iter().map(|((role,provider,model),calls)| {
        let mut totals = json!({});
        for field in FIELDS {
            let values: Vec<_> = calls.iter().filter_map(|e| e["usage"][*field].as_number()).collect();
            let sum = if values.is_empty() { Value::Null } else if *field == "cost_usd" { json!(values.iter().filter_map(|v|v.as_f64()).sum::<f64>()) } else {
                values.iter().try_fold(0u64, |a,b| a.checked_add(b.as_u64()?)).map(|v|json!(v)).unwrap_or(Value::Null)
            };
            totals[*field] = json!({"reported":sum,"measured_calls":values.len(),"missing_calls":calls.len()-values.len()});
        }
        json!({"role":role,"provider":provider,"model":model,"attempts":calls.len(),"failed_attempts":calls.iter().filter(|e|e["status"]=="error").count(),"counters":totals})
    }).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_partial_and_invalid_usage_are_not_zero_or_double_counted() {
        let mut m = Meter::default();
        m.observe(&json!({"input_tokens":100,"cached_input_tokens":80,"output_tokens":20,"reasoning_output_tokens":5,"cost":0.1,"secret":"never stored"}),true);
        assert_eq!(m.usage["input_tokens"], 100);
        assert!(m.usage.get("secret").is_none());
        let mut a =
            json!({"event":"model_call","provider":"codex","model":"test","status":"error"});
        m.attach(&mut a);
        let b = json!({"event":"model_call","provider":"codex","model":"test"});
        let s = summarize(&[a, b]);
        assert_eq!(s[0]["counters"]["input_tokens"]["reported"], 100);
        assert_eq!(s[0]["counters"]["input_tokens"]["missing_calls"], 1);
        assert_eq!(s[0]["failed_attempts"], 1);
        m.observe(
            &json!({"input_tokens":7,"cached_input_tokens":8,"output_tokens":-1}),
            false,
        );
        assert_eq!(m.usage["input_tokens"], 107);
        assert!(m.usage["cached_input_tokens"].is_null());
        assert!(m.usage["output_tokens"].is_null());
        m.observe(&Value::Null, false);
        assert!(m.usage["input_tokens"].is_null());
        let mut initially_missing = Meter::default();
        initially_missing.observe(&Value::Null, false);
        initially_missing.observe(&json!({"input_tokens":10}), false);
        assert!(initially_missing.usage["input_tokens"].is_null());
        let empty = summarize(&[json!({"event":"model_call"})]);
        assert!(empty[0]["counters"]["input_tokens"]["reported"].is_null());
    }
}
