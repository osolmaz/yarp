#![forbid(unsafe_code)]
// Reason: adapter dispatch and metric conversions are clearer with direct, source-shaped code.
#![allow(
    clippy::cast_precision_loss,
    clippy::format_collect,
    clippy::large_stack_arrays,
    clippy::missing_errors_doc,
    clippy::naive_bytecount,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

pub mod adapters;
pub mod benchmark;
pub mod ceiling;
pub mod database;
pub mod error;
pub mod keys;
pub mod model;
pub mod private_fs;
pub mod sink;
pub mod stream;
