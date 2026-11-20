//! designllm-figures: paper figures (diagrams, data plots, composite panels, image panels) on the frozen
//! v1 contracts.
//!
//! Public entry points: [`FigureEngine::default`], [`FigureEngine::with_pptx_exporter`], and the
//! `EngineAdapter` implementation. Output goes figure -> common `RenderScene` -> `RenderProvider::encode`
//! (SVG, PDF, PNG) or the PPTX scene exporter supplied by the application.
//!
//! Data is never repaired: source text is kept, missing values stay gaps, ambiguous and malformed cells
//! block, and every difference from the source asset must be explained by a recorded transform.

pub mod axis;
pub mod dataset;
pub mod decimal;
pub mod diag;
pub mod digest;
pub mod engine;
pub mod evidence;
pub mod image;
pub mod layout;
pub mod native;
pub mod ops;
pub mod plot;
pub mod table;
pub mod validate;
mod guard;
mod math;
mod service;

pub use dataset::{ColumnDecl, classify_cell, import as import_dataset};
pub use engine::{ENGINE_ID, FigureEngine, PptxSceneExporter};
pub use native::{FORMAT as NATIVE_FORMAT, MIME as NATIVE_MIME};
pub use service::{edit_dataset, import_csv, import_csv_declared};
