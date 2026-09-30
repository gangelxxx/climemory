//! Keep untrusted response metadata useful without persisting echoed credentials.
use serde_json::{json, Value};

pub(super) fn redact(response: &mut Value, credential: Option<&str>) {
    let Some(key) = credential.filter(|key| !key.is_empty()) else {
        return;
    };
    for field in ["provider", "id", "model"] {
        if let Some(text) = response
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            response[field] = json!(text.replace(key, "[REDACTED]"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_in_all_response_fields_without_changing_usage() {
        let mut response = json!({"usage":{"input_tokens":321},"provider":"host secret", "id":"secret-id", "model":"model-secret"});
        redact(&mut response, Some("secret"));
        assert_eq!(
            response,
            json!({"usage":{"input_tokens":321},"provider":"host [REDACTED]","id":"[REDACTED]-id","model":"model-[REDACTED]"})
        );
    }

    #[test]
    fn absent_credentials_and_non_string_metadata_are_unchanged() {
        for key in [None, Some(""), Some("secret")] {
            for mut value in [
                json!(null),
                json!({"provider":true,"id":17,"model":null}),
                json!({"provider":"Together","id":"gen-123"}),
            ] {
                let before = value.clone();
                redact(&mut value, key);
                assert_eq!(value, before);
            }
        }
    }
}
