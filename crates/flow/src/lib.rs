//! Shared flow-document layout, semantic edits and content digest (designllm-flow).
//!
//! Entry points (docs/WORKER_INTERFACES.md):
//! - [`layout_flow`] lays out a [`FlowDocument`] into paged [`RenderScene`]s using the
//!   context's RenderProvider for shaping (shape once; primitives carry the shaped glyphs).
//! - [`apply_flow_operations`] applies typed operations to the semantic model, all or nothing.
//!   Native byte edits of existing files belong to the format adapters.
//! - [`content_digest`] hashes content (text, atoms, structure, image assets) but not design.
//! - [`guard`] refuses contract v1.2 content by name (P0): used by [`apply_flow_operations`],
//!   [`layout_flow`] and [`validate_document`], and by format adapters (HWP) before their own edits.
//!
//! Helpers for format adapters: [`LogicalParagraph`] (grapheme <-> run byte mapping with
//! protected atoms), node lookup in [`tree`], [`validate_document`].
pub mod design_rules;
mod digest;
pub mod guard;
mod layout;
pub mod line_spacing;
mod linebreak;
mod ops;
pub mod page_select;
mod text;
mod text_links;
pub mod tree;

pub use designllm_contracts::{FlowDocument, RenderScene};
pub use digest::{block_value, content_digest, CONTENT_DIGEST_VERSION};
pub use layout::{face_identity, grapheme_bounds, is_page_break_carrier, layout_flow, layout_flow_with_carriers, layout_flow_with_links, layout_flow_with_options, scene_font_fingerprint, LayoutOptions, CELL_PADDING_X, CELL_PADDING_Y, DEFAULT_TAB_STOP, LAYOUT_ENGINE, LAYOUT_VERSION};
pub use text_links::{grapheme_span, OBJECT_REPLACEMENT_BYTES, TextLinkSpan};

/// Test support shared by the format crates' tests: the hyperlink targets their link tests run through each format.
/// Not a format rule and not for adapter code.
#[doc(hidden)]
pub mod test_support {
    pub use crate::text_links::{longest_accepted_link_target, refused_link_targets};
}
pub use linebreak::{break_between, is_space};
pub use ops::{apply_flow_operations, valid_color, validate_paragraph_format};
pub use text::{grapheme_count, is_forced_break, logical_text, validate_edit_text, LogicalGrapheme, LogicalParagraph, RunPiece, OBJECT_REPLACEMENT};
pub use tree::{find_node, find_paragraph, validate_document, FlowNode};
