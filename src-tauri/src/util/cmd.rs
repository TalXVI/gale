use std::result::Result as StdResult;

use serde::Serialize;
use serde_json::json;

#[derive(Debug)]
pub struct CommandError(eyre::Error);

impl Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let obj = json!({
            "message": format!("{:#}", self.0),
            "detail": format!("{:#}", self.0)
        });

        obj.serialize(serializer)
    }
}

impl<T> From<T> for CommandError
where
    T: Into<eyre::Report>,
{
    fn from(value: T) -> Self {
        Self(value.into())
    }
}

pub type Result<T> = StdResult<T, CommandError>;

#[cfg(test)]
mod tests {
    #[derive(Debug)]
    struct ErrorWithPrivateDebug;

    impl std::fmt::Display for ErrorWithPrivateDebug {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("public message")
        }
    }
    impl std::error::Error for ErrorWithPrivateDebug {}

    #[test]
    fn security_ipc_errors_do_not_serialize_arbitrary_debug_data() {
        let error = super::CommandError::from(ErrorWithPrivateDebug);
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("public message"));
        assert!(!json.contains("ErrorWithPrivateDebug"));
    }
}
