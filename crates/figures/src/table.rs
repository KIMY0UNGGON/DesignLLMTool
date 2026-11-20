//! Delimited text reader (CSV/TSV) for figure datasets. Every cell is kept as its raw source text.
//! Structural problems are errors. Cell meaning is decided later by `dataset`.

/// One data record with its 1-based physical line number in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub line: usize,
    pub cells: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub delimiter: char,
    pub header: Vec<String>,
    pub records: Vec<Record>,
}

/// Parses UTF-8 CSV or TSV.
/// The delimiter is TAB when the header line contains a TAB and no comma, otherwise comma.
/// A leading UTF-8 BOM is removed. Blank lines are skipped. Quoted fields use doubled quotes ("")
/// as escapes and may not span lines.
pub fn parse(bytes: &[u8]) -> Result<Table, String> {
    parse_inner(bytes, None)
}

/// Uses the source MIME delimiter instead of guessing from header punctuation.
pub fn parse_delimited(bytes: &[u8], delimiter: char) -> Result<Table, String> {
    if delimiter != ',' && delimiter != '\t' { return Err("delimiter must be comma or tab".into()); }
    parse_inner(bytes, Some(delimiter))
}

fn parse_inner(bytes: &[u8], explicit: Option<char>) -> Result<Table, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("source is not UTF-8: {e}"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<(usize, &str)> = text
        .split('\n')
        .enumerate()
        .map(|(i, l)| (i + 1, l.strip_suffix('\r').unwrap_or(l)))
        .filter(|(_, l)| !l.is_empty())
        .collect();
    let &(first_line, first) = lines.first().ok_or_else(|| "source has no header line".to_owned())?;
    let delimiter = explicit.unwrap_or_else(|| if first.contains('\t') && !first.contains(',') { '\t' } else { ',' });
    let header = split_record(first, delimiter, first_line)?;
    for (i, name) in header.iter().enumerate() {
        if name.trim().is_empty() {
            return Err(format!("line {first_line}: header column {} is empty", i + 1));
        }
        if header[..i].contains(name) {
            return Err(format!("line {first_line}: duplicate header \"{name}\""));
        }
    }
    let mut records = Vec::new();
    for &(line, raw) in lines.iter().skip(1) {
        let cells = split_record(raw, delimiter, line)?;
        if cells.len() != header.len() {
            return Err(format!("line {line}: expected {} fields, found {}", header.len(), cells.len()));
        }
        records.push(Record { line, cells });
    }
    Ok(Table { delimiter, header, records })
}

fn split_record(line: &str, delimiter: char, line_no: usize) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        let mut field = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err(format!("line {line_no}: unterminated quoted field")),
                    Some('"') => {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            field.push('"');
                        } else {
                            break;
                        }
                    }
                    Some(c) => field.push(c),
                }
            }
            match chars.next() {
                None => {
                    fields.push(field);
                    return Ok(fields);
                }
                Some(c) if c == delimiter => fields.push(field),
                Some(_) => return Err(format!("line {line_no}: text after closing quote")),
            }
        } else {
            loop {
                match chars.next() {
                    None => {
                        fields.push(field);
                        return Ok(fields);
                    }
                    Some(c) if c == delimiter => {
                        fields.push(field);
                        break;
                    }
                    Some(c) => field.push(c),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_raw_cells_and_quoted_delimiters() {
        let t = parse("condition,mean\nG1,\"1,234\"\n".as_bytes()).unwrap();
        assert_eq!(t.records[0].cells, vec!["G1".to_owned(), "1,234".to_owned()]);
        assert_eq!(t.records[0].line, 2);
    }

    #[test]
    fn tab_delimiter_only_when_no_comma_in_header() {
        let t = parse("a\tb\n1\t2\n".as_bytes()).unwrap();
        assert_eq!(t.delimiter, '\t');
        assert_eq!(t.header, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn rejects_bad_structure_instead_of_repairing() {
        assert!(parse("a,b\n1\n".as_bytes()).unwrap_err().contains("expected 2 fields, found 1"));
        assert!(parse("a,a\n1,2\n".as_bytes()).unwrap_err().contains("duplicate header"));
        assert!(parse("a,b\n\"1,2\n".as_bytes()).unwrap_err().contains("unterminated"));
        assert!(parse(&[0xff, 0xfe, 0x61][..]).unwrap_err().contains("not UTF-8"));
    }

    #[test]
    fn bom_and_crlf_are_handled_and_blank_lines_skipped() {
        let t = parse("\u{feff}a,b\r\n\r\n1,2\r\n".as_bytes()).unwrap();
        assert_eq!(t.header[0], "a");
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.records[0].line, 3);
    }
}
