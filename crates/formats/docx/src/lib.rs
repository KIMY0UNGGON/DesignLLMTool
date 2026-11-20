//! Native DOCX engine (designllm-docx).
//!
//! The original package is an immutable asset. The flow model comes from `word/document.xml`, and each
//! native record ties a model node to its original XML. An edit is written back as byte-range changes to the
//! original parts: unchanged blocks are copied verbatim, and unknown XML is never regenerated.
//! `verify_saved` re-parses the written package and compares it with the model before a save is published.
//!
//! Entry point: [`DocxEngine`] (`EngineAdapter`, kind `Docx`).

#![forbid(unsafe_code)]

pub mod diag;
pub mod engine;
pub mod guard;
pub mod model;
pub mod ops;
pub mod parse;
pub mod png;
pub mod props;
pub mod styles;
pub mod template;
pub mod tree;
pub mod units;
pub mod verify;
pub mod write;

pub use engine::{DocxEngine, ENGINE_ID};
pub use template::DOCX_MIME;
