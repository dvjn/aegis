//! Black-box integration tests: the compiled binary is spawned as a subprocess
//! and driven over real HTTP against a fake upstream running in this process.

#[path = "../shared/mod.rs"]
mod shared;

mod cases;
