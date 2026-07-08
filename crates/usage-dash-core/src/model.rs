//! The usage data model now lives in the wasm-safe leaf crate
//! [`usage_dash_model`] so the WASM render plugin and the native host share the
//! exact same serde types. This module re-exports it verbatim; all existing
//! `crate::model::…` paths (sources, stats, render) keep resolving unchanged.

pub use usage_dash_model::*;
