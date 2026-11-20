//! Shared OPC/ZIP package layer for the DesignLLMTool format engines (PPTX, DOCX, HWPX).
//!
//! - [`Package`]: a ZIP container that rejects unsafe input (duplicate or traversal names,
//!   encrypted or oversized entries, compression bombs). Unchanged entries are written back
//!   from their original compressed bytes.
//! - [`xml`]: byte-span patch helpers. An edit replaces only the byte ranges it names, and
//!   every other byte, including unknown subtrees and comments, is copied verbatim.
//! - [`opc`]: content types, relationship parts, and structural verification.
//!
//! This crate interprets no document semantics. Each format engine owns its model.

#![forbid(unsafe_code)]

pub mod opc;
pub mod package;
pub mod parts;
pub mod xml;

pub use designllm_contracts::{DltError, DltResult};
pub use package::{is_cfb_container, Limits, Package};
