use serde::Serialize;

pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Debug, Serialize)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

impl Failure {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into().chars().take(4096).collect() }
    }

    pub fn io(error: std::io::Error) -> Self {
        let code = match error.kind() {
            std::io::ErrorKind::NotFound => "target_missing",
            std::io::ErrorKind::PermissionDenied => "permission",
            std::io::ErrorKind::AlreadyExists => "conflict",
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted => "transient",
            _ => "unavailable",
        };
        Self::new(code, error.to_string())
    }

    pub fn backend(error: opendal::Error) -> Self {
        use opendal::ErrorKind;
        let code = match error.kind() {
            ErrorKind::NotFound => "object_missing",
            ErrorKind::PermissionDenied => "permission",
            ErrorKind::ConfigInvalid => "not_configured",
            ErrorKind::AlreadyExists | ErrorKind::ConditionNotMatch | ErrorKind::Conflict => {
                "conflict"
            }
            ErrorKind::RangeNotSatisfied => "integrity",
            ErrorKind::IsADirectory | ErrorKind::NotADirectory | ErrorKind::IsSameFile => {
                "invalid_request"
            }
            ErrorKind::RateLimited => "transient",
            _ if error.is_temporary() => "transient",
            _ => "unavailable",
        };
        Self::new(code, format!("OpenDAL: {error}"))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Failure {}
