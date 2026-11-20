//! Diagnostic codes that the PDF reader worker's host gives a read (tasks 34c, 34d). docs/CONTRACTS.md lists them with the
//! reader's other codes. The in-process reader (`designllm-sources`) names the same codes for the same limits.

/// The worker process ended at its stack limit (Windows exit status `STATUS_STACK_OVERFLOW`), a resource limit like
/// [`PDF_MEMORY_LIMIT`]: the request fails with `UnsupportedOperation`, only the worker ends, and no page is named.
pub const PDF_STACK_LIMIT: &str = "PDF_STACK_LIMIT";
/// The worker process exceeded its committed memory cap (`UnsupportedOperation`).
pub const PDF_MEMORY_LIMIT: &str = "PDF_MEMORY_LIMIT";
/// The read ran past its time limit, or past the request's work limit in the reader (`UnsupportedOperation`).
pub const PDF_REQUEST_BUDGET: &str = "PDF_REQUEST_BUDGET";
/// The worker crashed otherwise, or its reply was missing or broken (`InvalidArgument`).
pub const PDF_INTERNAL_PANIC: &str = "PDF_INTERNAL_PANIC";
/// The asset is over the supported PDF size (`InvalidArgument`).
pub const PDF_TOO_LARGE: &str = "PDF_TOO_LARGE";
