//! The page range of an export (`쪽 범위`): what the export popover's field and the `pages` argument of `file.export`
//! say, counted from 1 (`1, 3-5`), as the 0-based `page_indices` of the export request.

use std::fmt;

use designllm_contracts::ArtifactKind;

/// Why a page range is refused. The text is shown under the popover's field, or returned as the error of `file.export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageRangeError {
    Empty,
    NotNumber,
    Malformed,
    Zero,
    ReversedSpan { start: u64, end: u64 },
    OutOfRange { count: usize },
    Unordered { page: u64 },
    Repeated { page: u64 },
}

impl fmt::Display for PageRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Empty => f.write_str("쪽 번호를 입력하세요"),
            Self::NotNumber => f.write_str("쪽 번호는 숫자, 쉼표(,), 하이픈(-)으로 적으세요"),
            Self::Malformed => f.write_str("쪽 번호 형식이 맞지 않습니다 (예: 1, 3-5)"),
            Self::Zero => f.write_str("쪽 번호는 1부터 시작합니다"),
            Self::ReversedSpan { start, end } => write!(f, "{start}-{end}은 거꾸로 된 범위입니다"),
            Self::OutOfRange { count } => write!(f, "이 문서는 {count}쪽입니다"),
            Self::Unordered { page } => write!(f, "{page}쪽은 앞에 적은 쪽보다 앞입니다. 작은 쪽부터 적으세요"),
            Self::Repeated { page } => write!(f, "{page}쪽이 두 번 적혔습니다"),
        }
    }
}

/// Whether an export of `kind` in `format` (lower case) applies a page range, that is, the engine writes only the pages that
/// `page_indices` names. Read from each engine: Canvas `pdf` (`scene_from`) and `html` (`export_html`); Slides `pdf`
/// (`select_pages`), while its `pptx` refuses `page_indices`; Word, HWPX and HWP `pdf` (`select_pages`). PNG and SVG write
/// one page, WebCode refuses page indices and Figure has one page, so none of them offers a range.
pub(super) fn range_supported(kind: ArtifactKind, format: &str) -> bool {
    match kind {
        ArtifactKind::Canvas => matches!(format, "pdf" | "html"),
        ArtifactKind::Slides | ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp => format == "pdf",
        ArtifactKind::WebCode | ArtifactKind::Figure => false,
    }
}

/// The characters a page list may carry in place of the ASCII comma and hyphen: the full-width and typographic forms that
/// the input methods produce, the wave dashes (U+301C, U+FF5E) among them. They read as the ASCII characters.
fn ascii_separator(c: char) -> char {
    match c {
        '\u{FF0C}' | '\u{3001}' => ',',
        '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FF0D}' | '\u{301C}' | '\u{FF5E}' => '-',
        other => other,
    }
}

/// Parses `text` for a document of `count` pages into the 0-based indices of the pages, in ascending order. Items are
/// separated by commas; an item is one page or a span `a-b` (spaces allowed). A page named twice, a span that runs
/// backwards and an item that comes before the item preceding it are refused, as are empty items, non-digits and pages past `count`.
pub(super) fn parse_page_range(text: &str, count: usize) -> Result<Vec<u32>, PageRangeError> {
    let normalized: String = text.chars().map(ascii_separator).collect();
    let text = normalized.trim();
    if text.is_empty() {
        return Err(PageRangeError::Empty);
    }
    // The spans taken so far, ascending and apart from each other.
    let mut spans: Vec<(u64, u64)> = Vec::new();
    for item in text.split(',') {
        let (start, end) = parse_item(item, count)?;
        let last = spans.last().map_or(0, |span| span.1);
        if start <= last {
            let repeated = spans
                .iter()
                .filter(|&&(first, final_page)| start <= final_page && first <= end)
                .map(|&(first, _)| first.max(start))
                .min();
            return Err(match repeated {
                Some(page) => PageRangeError::Repeated { page },
                None => PageRangeError::Unordered { page: start },
            });
        }
        spans.push((start, end));
    }
    Ok(spans.iter().flat_map(|&(start, end)| (start..=end).map(|page| (page - 1) as u32)).collect())
}

/// One item: a page `n` or a span `a-b`, as page numbers counted from 1 that lie inside the document.
fn parse_item(item: &str, count: usize) -> Result<(u64, u64), PageRangeError> {
    let mut parts = item.split('-');
    let start = parse_page(parts.next().unwrap_or_default())?;
    let end = match parts.next() {
        Some(token) => parse_page(token)?,
        None => start,
    };
    if parts.next().is_some() {
        return Err(PageRangeError::Malformed);
    }
    if end < start {
        return Err(PageRangeError::ReversedSpan { start, end });
    }
    if start == 0 {
        return Err(PageRangeError::Zero);
    }
    if end > count as u64 {
        return Err(PageRangeError::OutOfRange { count });
    }
    Ok((start, end))
}

/// A page number token. A number too long for `u64` is past any document, so it maps to `u64::MAX`.
fn parse_page(token: &str) -> Result<u64, PageRangeError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(PageRangeError::Malformed);
    }
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PageRangeError::NotNumber);
    }
    Ok(token.parse::<u64>().unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
