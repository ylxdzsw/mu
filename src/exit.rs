use std::fmt;

/// An error that carries a specific process exit code.
///
/// `main` downcasts to this to map well-known failure classes to the exit
/// codes documented in SPEC §11 ("Exit codes"). Errors without an `ExitError`
/// fall back to the general error code `1`.
#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub message: String,
}

impl ExitError {
    /// A `--session <id>` (or `-c`) that does not resolve in the active scope.
    /// Exit code `2`, matching "session busy" as a session-addressing failure.
    pub fn session_not_found(id: &str) -> anyhow::Error {
        anyhow::Error::new(ExitError {
            code: 2,
            message: format!("session not found in active scope: {id}"),
        })
    }
}

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitError {}
