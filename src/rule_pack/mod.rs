#![forbid(unsafe_code)]

mod compiled;
mod model;
mod source;
mod strict_json;
mod validation;

pub use compiled::{CompiledPack, ProgramEntry, RuleRecord, compile};
pub use model::{
    Action, CommandMatcher, ENGINE_ABI_VERSION, LinePattern, OutputPolicy, PackManifest,
    PatternCase, PatternKind, PatternTrim, Reducer, Rule, SOURCE_SCHEMA_VERSION, Transform,
};
pub use source::SourcePack;
pub use strict_json::from_slice as decode_json;
pub use validation::{
    MAX_COMPILED_BYTES, MAX_RULES, MAX_SOURCE_BYTES, MAX_SOURCE_FILE_BYTES,
    MAX_STREAM_MEMORY_BYTES, stream_memory_bound, validate_manifest, validate_rule, validate_rules,
};
