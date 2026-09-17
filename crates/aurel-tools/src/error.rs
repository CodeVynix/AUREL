//! Typed tool failures. Every fallible tool operation returns
//! [`ToolError`]: no panics on bad input, no collapsed "something failed"
//! variant. Messages name the offending path; paths are caller-supplied
//! workspace locations, never secrets.

use std::fmt;

/// What went wrong inside a read-only tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// The path string itself is unusable (empty, NUL byte, bad depth…).
    InvalidPath(String),
    /// The resolved target escapes the workspace root (traversal, absolute
    /// path elsewhere, or a symlink pointing out). Never probed further.
    OutsideWorkspace { path: String },
    /// Nothing exists at the resolved location.
    NotFound { path: String },
    /// Exists but is not a regular file.
    NotFile { path: String },
    /// Exists but is not a directory.
    NotDirectory { path: String },
    /// Content is binary (NUL byte), not readable text.
    BinaryFile { path: String },
    /// A result would exceed a configured bound.
    TooLarge { path: String, limit: String },
    /// Cooperative cancellation observed mid-walk.
    Cancelled,
    /// Filesystem I/O failure with the OS message attached.
    Io { path: String, message: String },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::InvalidPath(message) => write!(f, "error: invalid path: {message}"),
            ToolError::OutsideWorkspace { path } => {
                write!(f, "error: path escapes the workspace: '{path}'")
            }
            ToolError::NotFound { path } => write!(f, "error: no such file or directory: '{path}'"),
            ToolError::NotFile { path } => write!(f, "error: not a file: '{path}'"),
            ToolError::NotDirectory { path } => write!(f, "error: not a directory: '{path}'"),
            ToolError::BinaryFile { path } => write!(f, "error: not readable text: '{path}'"),
            ToolError::TooLarge { path, limit } => {
                write!(f, "error: '{path}' exceeds the {limit} limit")
            }
            ToolError::Cancelled => write!(f, "error: tool cancelled"),
            ToolError::Io { path, message } => {
                write!(f, "error: cannot access '{path}': {message}")
            }
        }
    }
}

impl std::error::Error for ToolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_path() {
        let text = ToolError::OutsideWorkspace {
            path: "../evil".into(),
        }
        .to_string();
        assert!(text.contains("../evil"), "got: {text:?}");
        assert!(text.starts_with("error:"));
    }
}
