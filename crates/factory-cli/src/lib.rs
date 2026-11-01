//! The wiring behind the `factory` binary.
//!
//! `factory-cli` is the one place where `factory-core`, `factory-review` and
//! `eval` meet. The library builds the service from the configuration and
//! the environment; `main.rs` only parses arguments and calls in here, and
//! the tests build the same service with fakes.

pub mod install;
pub mod logging;
pub mod wiring;
