//! Merge drivers. Each driver is a pure function of `(base, left, right)`;
//! every host must return the same result for the same input, which the
//! contract tests pin.

pub mod record;
pub mod text;

/// Identifies a driver and its version, recorded in every conflict and in
/// the metadata of every automatically merged commit.
pub const TEXT_DRIVER_V1: &str = "merge.text.v1";
pub const RECORD_DRIVER_V2: &str = "merge.record.v2";
