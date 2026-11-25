//! The document password, redacted in `Debug`.

use std::fmt;

/// A password for an encrypted input. Used once to open the file, never
/// logged and never stored.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The password itself. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::Password;

    #[test]
    fn debug_is_redacted() {
        let password = Password::new("hunter2");
        assert_eq!(format!("{password:?}"), "Password(***)");
        assert_eq!(password.expose(), "hunter2");
    }
}
