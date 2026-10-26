//! The commands besides `wye serve`. Each runs once and exits; none runs
//! inside the server.
//!
//! - `explain`: the server's decisions for one request, offline
//! - `policy_test`: sample requests checked against expected decisions
//! - `policy_diff`: what a config change widens, narrows, or changes
//! - `report`: a usage-log summary per session, request class, and prompt
//! - `wrapper`: `wye claude` and `wye codex`, an agent pointed at a gateway
//! - `probe`: a one-shot health check client
//! - `doctor`: a config's secrets, files, and endpoints checked before serving
//! - `init`: a minimal working config
//! - `token`: offline key generation and credential issuance

mod doctor;
mod explain;
mod init;
mod policy_diff;
mod policy_test;
mod probe;
mod report;
pub mod token;
mod wrapper;

pub use doctor::doctor;
pub use explain::{Credential, Explanation, explain};
pub use init::{InitOptions, init};
pub use policy_diff::{PolicyDiff, policy_diff};
pub use policy_test::policy_test;
pub use probe::probe;
pub use report::report;
pub use wrapper::{Agent, Launch, claude, codex};
