//! Compile-time identity shared by the CLI, diagnostics, and daemon log.
pub const IDENTITY: &str = concat!(
    "ltd ",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("LTD_BUILD_DETAILS"),
    ")"
);
