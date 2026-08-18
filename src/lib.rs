//! Web Audio API extension for `deno_core`.
//!
//! This crate is in its Phase 0 compatibility spike. It currently exports an
//! empty extension that declares its ordering dependencies on `deno_webidl`
//! and `deno_web`; no Web Audio globals are installed yet.
//!
//! This project is not affiliated with, endorsed by, or maintained by
//! Deno Land Inc.

#![warn(missing_docs)]

deno_core::extension!(deno_audio, deps = [deno_webidl, deno_web],);
