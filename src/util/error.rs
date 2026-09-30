use super::*;

#[derive(Clone, Debug, Serialize)]
pub struct ErrorDiagnostic {
    pub argument: String,
    pub received: String,
    pub allowed_values: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_value: Option<String>,
}

#[derive(Debug)]
pub struct AppError {
    pub msg: String,
    pub hint: Option<String>,
    pub details: Box<ErrorDetails>,
}

#[derive(Debug, Default)]
pub struct ErrorDetails {
    pub diagnostic: Option<Box<ErrorDiagnostic>>,
    pub retry: bool,
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppErrorKind {
    General,
    StorageContention,
    StorageCorrupt,
}

impl AppError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            hint: None,
            details: Box::default(),
        }
    }

    pub fn with_hint(msg: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            hint: Some(hint.into()),
            details: Box::default(),
        }
    }

    pub fn invalid_value(
        argument: &str,
        received: &str,
        allowed_values: &[&str],
        hint: impl Into<String>,
    ) -> Self {
        Self::invalid_value_with_message(
            format!(
                "invalid {argument} '{received}'; expected {}",
                allowed_values.join(", ")
            ),
            argument,
            received,
            allowed_values,
            hint,
        )
    }

    pub fn invalid_value_with_message(
        message: impl Into<String>,
        argument: &str,
        received: &str,
        allowed_values: &[&str],
        hint: impl Into<String>,
    ) -> Self {
        let suggested_value = closest_value(received, allowed_values).map(str::to_string);
        Self {
            msg: message.into(),
            hint: Some(hint.into()),
            details: Box::new(ErrorDetails {
                diagnostic: Some(Box::new(ErrorDiagnostic {
                    argument: argument.to_string(),
                    received: received.to_string(),
                    allowed_values: allowed_values
                        .iter()
                        .map(|value| (*value).to_string())
                        .collect(),
                    suggested_value,
                })),
                retry: false,
                extra: None,
            }),
        }
    }

    pub fn with_retry(mut self) -> Self {
        self.details.retry = true;
        self
    }

    pub fn with_extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.details
            .extra
            .get_or_insert_with(serde_json::Map::new)
            .insert(key.into(), value);
        self
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(value: rusqlite::Error) -> Self {
        use rusqlite::ffi::ErrorCode;

        let kind = match &value {
            rusqlite::Error::SqliteFailure(error, _) => match error.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                    AppErrorKind::StorageContention
                }
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                    AppErrorKind::StorageCorrupt
                }
                _ => AppErrorKind::General,
            },
            _ => AppErrorKind::General,
        };
        let contention = kind == AppErrorKind::StorageContention;
        Self {
            msg: if contention {
                "derived index is busy; retry the command".to_string()
            } else {
                value.to_string()
            },
            hint: contention
                .then(|| "another climemory process is updating the rebuildable index".to_string()),
            details: Box::default(),
        }
    }
}
