//! `aurel-core`: shared Rust foundation for AUREL.
//!
//! Phase 0 exposes only the smallest real API: the crate version.
//! Future phases (agent engine, tools, model providers, config, sessions)
//! will extend this crate incrementally. No placeholder modules are created
//! until they have a real implementation behind them.

/// Returns the `aurel-core` crate version (e.g. `"0.1.0"`).
///
/// The CLI builds its `--version` output from this function so there is a
/// single source of truth for the version string.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty(), "version must not be empty");
    }

    #[test]
    fn version_matches_package_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
