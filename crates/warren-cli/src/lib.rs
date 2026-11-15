//! Library half of the `warren` command-line client: argument definitions, the
//! config file, output formatting and small parsers.
//!
//! Nothing in this crate root talks to the network, so it builds and tests
//! without a daemon; the binary (`main.rs`) wires these pieces to `warren-sdk`.

pub mod cli;
pub mod config;
pub mod failure;
pub mod output;
pub mod util;

pub use cli::Cli;
pub use config::{FileConfig, Settings};
pub use failure::{EXIT_API, EXIT_OK, EXIT_USAGE, Failure};
