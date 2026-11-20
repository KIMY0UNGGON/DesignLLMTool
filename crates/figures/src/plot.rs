//! Plot binding. A PlotSpec is checked against the dataset it names. Every row and cell is classified
//! as valid, missing, ambiguous, or malformed. Missing values stay gaps. Ambiguous and malformed values
//! block. Nothing is averaged, clipped, interpolated, or filled in.

use designllm_contracts::{
    CellState, DataCell, DataRow, Dataset, Diagnostic, ErrorValues, FigureDocument, FigurePanel, PlotDisplayMode, PlotErrorKind, PlotSpec,
};

use crate::axis::{self, ResolvedAxis, Scale};
use crate::decimal::{Dec, Exact};
use crate::diag::{self, code};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bar,
    Line,
    Scatter,
}

#[derive(Debug, Clone, PartialEq)]
pub enum XVal {
    Category(String),
    Number(Exact),
    /// An empty numeric x cell. The point is not drawn and nothing is estimated.
    Missing,
}

#[derive(Debug, Clone)]
pub struct ErrBar {
    pub lower: Exact,
    pub upper: Exact,
    /// Source text of the error, for example "0.10 ± 0.01".
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct YVal {
    pub mean: Exact,
    pub mean_text: String,
    pub err: Option<ErrBar>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub row_id: String,
    pub x: XVal,
    pub x_text: String,
    pub n_text: Option<String>,
    /// One entry per y series. `None` is a gap.
    pub ys: Vec<Option<YVal>>,
}

#[derive(Debug, Clone)]
pub struct PlotData {
    pub kind: Kind,
    pub x_numeric: bool,
    /// Series names, which are the y column ids.
    pub series: Vec<String>,
    pub rows: Vec<Row>,
    pub error: Option<PlotErrorKind>,
    pub confidence_level: Option<String>,
    pub y: ResolvedAxis,
    pub x_axis: Option<ResolvedAxis>,
    pub y_caption: String,
    pub x_caption: String,
    pub dataset_id: String,
}

pub struct PlotBuild {
    pub data: Option<PlotData>,
    pub diagnostics: Vec<Diagnostic>,
}

fn caption(label: &str, unit: &str) -> String {
    if label.trim().is_empty() {
        format!("({unit})")
    } else {
        format!("{label} ({unit})")
    }
}

fn parse_bound(text: &str, what: &str, d: &mut Vec<Diagnostic>, pid: &str) -> Option<Exact> {
    match Dec::parse(text) {
        Ok(v) => Some(v.exact()),
        Err(e) => {
            d.push(diag::blocking(code::AXIS_RANGE_INVALID, Some(pid), format!("{what} \"{text}\": {}", e.reason)));
            None
        }
    }
}

/// Some(None): automatic. Some(Some): explicit. None: invalid (diagnostic pushed).
fn explicit_range(min: &Option<String>, max: &Option<String>, pid: &str, d: &mut Vec<Diagnostic>) -> Option<Option<(Exact, Exact)>> {
    match (min, max) {
        (None, None) => Some(None),
        (Some(a), Some(b)) => {
            let lo = parse_bound(a, "minimum", d, pid);
            let hi = parse_bound(b, "maximum", d, pid);
            Some(Some((lo?, hi?)))
        }
        _ => {
            d.push(diag::blocking(
                code::AXIS_RANGE_PARTIAL,
                Some(pid),
                "set both minimum and maximum, or neither for an automatic range",
            ));
            None
        }
    }
}

/// Parses a valid numeric cell. Callers have already classified the cell.
fn number_of(cell: &DataCell) -> Option<Exact> {
    crate::decimal::Dec::parse(&cell.original).ok().map(|d| d.exact())
}

/// Reads one error magnitude. Negative magnitudes, and missing, ambiguous and malformed cells, are blocking.
fn magnitude(cell: &DataCell, node: &str, column: &str, d: &mut Vec<Diagnostic>) -> Option<(Exact, String)> {
    match cell.state {
        CellState::Valid => {
            let dec = Dec::parse(&cell.original).ok()?;
            if dec.neg && dec.coef != 0 {
                d.push(diag::blocking(
                    code::NEGATIVE_ERROR,
                    Some(node),
                    format!("error magnitude \"{}\" in column \"{column}\" is negative", cell.original),
                ));
                return None;
            }
            Some((dec.exact(), cell.original.clone()))
        }
        CellState::Missing => {
            d.push(diag::blocking(code::ERROR_MISSING, Some(node), format!("error column \"{column}\" is empty: nothing is estimated")));
            None
        }
        CellState::Ambiguous => {
            d.push(diag::blocking(code::DATA_AMBIGUOUS, Some(node), format!("error \"{}\" in \"{column}\" is ambiguous", cell.original)));
            None
        }
        CellState::Malformed => {
            d.push(diag::blocking(code::DATA_MALFORMED, Some(node), format!("error \"{}\" in \"{column}\" is malformed", cell.original)));
            None
        }
    }
}

/// Error columns, by dataset column index.
enum ErrCols {
    Sym(usize),
    Asym(usize, usize),
    Bounds(usize, usize),
}

fn col_of(ds: &Dataset, id: &str) -> Option<usize> {
    ds.columns.iter().position(|c| c.id == id)
}

fn cell_at<'a>(row: &'a DataRow, col_id: &str) -> &'a DataCell {
    &row.cells[col_id]
}

fn widen(lo: &mut Option<Exact>, hi: &mut Option<Exact>, v: Exact) {
    *lo = Some(match *lo {
        None => v,
        Some(x) if v < x => v,
        Some(x) => x,
    });
    *hi = Some(match *hi {
        None => v,
        Some(x) if v > x => v,
        Some(x) => x,
    });
}

/// Builds plot data and resolves the axes of one plot panel.
/// Any blocking diagnostic leaves `data` as None.
pub fn build(doc: &FigureDocument, panel: &FigurePanel, spec: &PlotSpec) -> PlotBuild {
    let pid = panel.id.as_str();
    let mut d: Vec<Diagnostic> = Vec::new();
    let finish = |data: Option<PlotData>, diagnostics: Vec<Diagnostic>| {
        let data = if diagnostics.iter().any(|x| x.blocking) { None } else { data };
        PlotBuild { data, diagnostics }
    };

    let kind = match spec.kind.as_str() {
        "bar" => Kind::Bar,
        "line" => Kind::Line,
        "scatter" => Kind::Scatter,
        other => {
            d.push(diag::blocking(
                code::PLOT_KIND_UNSUPPORTED,
                Some(pid),
                format!("plot kind \"{other}\" is not supported; use bar, line or scatter"),
            ));
            return finish(None, d);
        }
    };
    let Some(ds) = doc.datasets.iter().find(|x| x.id == spec.dataset_ref.dataset_id) else {
        d.push(diag::blocking(code::DATASET_NOT_FOUND, Some(pid), format!("dataset \"{}\" not found", spec.dataset_ref.dataset_id)));
        return finish(None, d);
    };
    if ds.version_hash != spec.dataset_ref.version_hash {
        d.push(diag::blocking(
            code::DATASET_REF_STALE,
            Some(pid),
            format!("plot is bound to dataset version {} but the current version is {}", spec.dataset_ref.version_hash, ds.version_hash),
        ));
        return finish(None, d);
    }
    let shape_ok = ds.rows.iter().all(|r| r.cells.len() == ds.columns.len() && ds.columns.iter().all(|c| r.cells.contains_key(&c.id)));
    if !shape_ok {
        d.push(diag::blocking(code::DATASET_ROW_SHAPE, Some(pid), "dataset rows do not match its columns"));
        return finish(None, d);
    }

    // Columns.
    let Some(xi) = col_of(ds, &spec.x_column) else {
        d.push(diag::blocking(code::COLUMN_NOT_FOUND, Some(pid), format!("x column \"{}\" not found", spec.x_column)));
        return finish(None, d);
    };
    let x_type = ds.columns[xi].data_type.clone();
    let x_ok = match kind {
        Kind::Bar => x_type == "category",
        Kind::Scatter => x_type == "numeric",
        Kind::Line => x_type == "category" || x_type == "numeric",
    };
    if !x_ok {
        d.push(diag::blocking(
            code::COLUMN_TYPE_INVALID,
            Some(pid),
            format!("x column \"{}\" has type \"{x_type}\", which this plot kind cannot use", spec.x_column),
        ));
        return finish(None, d);
    }
    let x_numeric = x_type == "numeric";

    if spec.y_columns.is_empty() {
        d.push(diag::blocking(code::COLUMN_REQUIRED, Some(pid), "at least one y column is required"));
        return finish(None, d);
    }
    let mut yi: Vec<usize> = Vec::new();
    for name in &spec.y_columns {
        let Some(i) = col_of(ds, name) else {
            d.push(diag::blocking(code::COLUMN_NOT_FOUND, Some(pid), format!("y column \"{name}\" not found")));
            return finish(None, d);
        };
        if ds.columns[i].data_type != "numeric" || *name == spec.x_column || yi.contains(&i) {
            d.push(diag::blocking(code::COLUMN_TYPE_INVALID, Some(pid), format!("y column \"{name}\" must be a distinct numeric column")));
            return finish(None, d);
        }
        yi.push(i);
    }

    // Axes.
    let Some(y_scale) = Scale::parse(&spec.y_axis.scale) else {
        d.push(diag::blocking(code::AXIS_SCALE_INVALID, Some(pid), format!("y scale \"{}\" is not linear or log10", spec.y_axis.scale)));
        return finish(None, d);
    };
    let y_unit = match spec.y_axis.unit.as_deref().map(str::trim) {
        Some(u) if !u.is_empty() => u.to_owned(),
        _ => {
            d.push(diag::blocking(code::UNIT_REQUIRED, Some(pid), "the y axis unit is required; units are never inferred"));
            return finish(None, d);
        }
    };
    let Some(y_explicit) = explicit_range(&spec.y_axis.minimum, &spec.y_axis.maximum, pid, &mut d) else {
        return finish(None, d);
    };
    let y_step = match spec.y_axis.tick_step.as_deref() {
        None => None,
        Some(t) => match Dec::parse(t) {
            Ok(v) => Some(v.exact()),
            Err(e) => {
                d.push(diag::blocking(code::TICK_STEP_INVALID, Some(pid), format!("tick step \"{t}\": {}", e.reason)));
                return finish(None, d);
            }
        },
    };

    // The numeric x axis has its own scale, range, unit and step. A category x axis takes none of these.
    let mut x_spec: Option<(Scale, Option<(Exact, Exact)>, Option<Exact>, String)> = None;
    if x_numeric {
        let Some(xs) = Scale::parse(&spec.x_axis.scale) else {
            d.push(diag::blocking(code::AXIS_SCALE_INVALID, Some(pid), format!("x scale \"{}\" is not linear or log10", spec.x_axis.scale)));
            return finish(None, d);
        };
        let x_unit = match spec.x_axis.unit.as_deref().map(str::trim) {
            Some(u) if !u.is_empty() => u.to_owned(),
            _ => {
                d.push(diag::blocking(code::UNIT_REQUIRED, Some(pid), "the numeric x axis unit is required"));
                return finish(None, d);
            }
        };
        let Some(xe) = explicit_range(&spec.x_axis.minimum, &spec.x_axis.maximum, pid, &mut d) else {
            return finish(None, d);
        };
        let xstep = match spec.x_axis.tick_step.as_deref() {
            None => None,
            Some(t) => match Dec::parse(t) {
                Ok(v) => Some(v.exact()),
                Err(e) => {
                    d.push(diag::blocking(code::TICK_STEP_INVALID, Some(pid), format!("x tick step \"{t}\": {}", e.reason)));
                    return finish(None, d);
                }
            },
        };
        x_spec = Some((xs, xe, xstep, x_unit));
    } else if spec.x_axis.minimum.is_some() || spec.x_axis.maximum.is_some() || spec.x_axis.tick_step.is_some() {
        d.push(diag::blocking(code::AXIS_NOT_APPLICABLE, Some(pid), "a category x axis takes no minimum, maximum or tick step"));
        return finish(None, d);
    }

    // Error specification.
    let mut error_kind: Option<PlotErrorKind> = None;
    let mut err_cols: Option<ErrCols> = None;
    let mut n_col: Option<usize> = None;
    let mut confidence: Option<String> = None;
    // The error key states the statistic of the error bars. Without error data there is nothing to state, so the key
    // is not drawn. The plot is still drawn, and the change is not refused.
    if spec.display == Some(PlotDisplayMode::ErrorKey) && spec.error.is_none() {
        d.push(diag::info(code::ERROR_KEY_NOT_DRAWN, Some(pid), "no error data; the error key is not drawn"));
    }
    if let Some(es) = &spec.error {
        if yi.len() != 1 {
            d.push(diag::blocking(
                code::ERROR_SERIES_AMBIGUOUS,
                Some(pid),
                "error bars apply to exactly one y series; split the series or remove the error",
            ));
            return finish(None, d);
        }
        let names: Vec<String> = match (es.kind, &es.values) {
            (PlotErrorKind::Range, ErrorValues::Bounds { lower_column, upper_column }) => vec![lower_column.clone(), upper_column.clone()],
            (PlotErrorKind::Sd | PlotErrorKind::Sem | PlotErrorKind::Ci, ErrorValues::Symmetric { column }) => vec![column.clone()],
            (PlotErrorKind::Sd | PlotErrorKind::Sem | PlotErrorKind::Ci, ErrorValues::Asymmetric { minus_column, plus_column }) => {
                vec![minus_column.clone(), plus_column.clone()]
            }
            _ => {
                d.push(diag::blocking(
                    code::ERROR_KIND_VALUES_MISMATCH,
                    Some(pid),
                    "range errors need bounds columns; sd, sem and ci need a symmetric or asymmetric amount",
                ));
                return finish(None, d);
            }
        };
        let mut idx = Vec::new();
        for name in &names {
            match col_of(ds, name) {
                None => {
                    d.push(diag::blocking(code::COLUMN_NOT_FOUND, Some(pid), format!("error column \"{name}\" not found")));
                    return finish(None, d);
                }
                Some(i) if ds.columns[i].data_type != "numeric" || i == xi || i == yi[0] => {
                    d.push(diag::blocking(code::COLUMN_TYPE_INVALID, Some(pid), format!("error column \"{name}\" must be a numeric column")));
                    return finish(None, d);
                }
                Some(i) => idx.push(i),
            }
        }
        err_cols = Some(match &es.values {
            ErrorValues::Bounds { .. } => ErrCols::Bounds(idx[0], idx[1]),
            ErrorValues::Asymmetric { .. } => ErrCols::Asym(idx[0], idx[1]),
            ErrorValues::Symmetric { .. } => ErrCols::Sym(idx[0]),
        });
        error_kind = Some(es.kind);
        match (es.kind, es.confidence_level.as_deref()) {
            (PlotErrorKind::Ci, None) => {
                d.push(diag::blocking(code::CI_LEVEL_REQUIRED, Some(pid), "a confidence interval needs its confidence level"));
                return finish(None, d);
            }
            (PlotErrorKind::Ci, Some(t)) => match Dec::parse(t) {
                Ok(l) if l.exact().is_positive() && l.exact() < Exact::int(1) => confidence = Some(t.to_owned()),
                _ => {
                    d.push(diag::blocking(code::CI_LEVEL_INVALID, Some(pid), format!("confidence level \"{t}\" must be strictly between 0 and 1")));
                    return finish(None, d);
                }
            },
            (_, Some(_)) => {
                d.push(diag::blocking(code::CI_LEVEL_NOT_APPLICABLE, Some(pid), "confidence level is only meaningful for ci"));
                return finish(None, d);
            }
            _ => {}
        }
        match &es.sample_count_column {
            Some(name) => match col_of(ds, name) {
                None => {
                    d.push(diag::blocking(code::COLUMN_NOT_FOUND, Some(pid), format!("sample count column \"{name}\" not found")));
                    return finish(None, d);
                }
                Some(i) if ds.columns[i].data_type != "count" => {
                    d.push(diag::blocking(code::COLUMN_TYPE_INVALID, Some(pid), format!("sample count column \"{name}\" must have type count")));
                    return finish(None, d);
                }
                Some(i) => n_col = Some(i),
            },
            None if es.kind == PlotErrorKind::Sem => {
                d.push(diag::blocking(code::N_REQUIRED, Some(pid), "sem needs a provided sample count column; n is never estimated"));
            }
            None => {}
        }
    }

    // Rows.
    let log = y_scale == Scale::Log10;
    let mut rows: Vec<Row> = Vec::with_capacity(ds.rows.len());
    let (mut y_lo, mut y_hi, mut x_lo, mut x_hi): (Option<Exact>, Option<Exact>, Option<Exact>, Option<Exact>) = (None, None, None, None);
    for row in &ds.rows {
        let node = format!("{pid}/{}", row.id);
        let x_cell = cell_at(row, &ds.columns[xi].id);
        let (x, x_text) = if x_numeric {
            match x_cell.state {
                CellState::Valid => match number_of(x_cell) {
                    Some(v) => {
                        if matches!(x_spec, Some((Scale::Log10, _, _, _))) && !v.is_positive() {
                            d.push(diag::blocking(
                                code::LOG_DOMAIN_VALUE,
                                Some(node.as_str()),
                                format!("x value \"{}\" is outside the log domain; not clipped", x_cell.original),
                            ));
                        }
                        widen(&mut x_lo, &mut x_hi, v);
                        (XVal::Number(v), x_cell.original.clone())
                    }
                    None => {
                        d.push(diag::blocking(code::DATA_MALFORMED, Some(node.as_str()), format!("x value \"{}\" is malformed", x_cell.original)));
                        (XVal::Missing, x_cell.original.clone())
                    }
                },
                CellState::Missing => {
                    d.push(diag::info(code::X_VALUE_MISSING, Some(node.as_str()), "x value is empty: the point is not drawn"));
                    (XVal::Missing, x_cell.original.clone())
                }
                CellState::Ambiguous => {
                    d.push(diag::blocking(code::DATA_AMBIGUOUS, Some(node.as_str()), format!("x value \"{}\" is ambiguous", x_cell.original)));
                    (XVal::Missing, x_cell.original.clone())
                }
                CellState::Malformed => {
                    d.push(diag::blocking(code::DATA_MALFORMED, Some(node.as_str()), format!("x value \"{}\" is malformed", x_cell.original)));
                    (XVal::Missing, x_cell.original.clone())
                }
            }
        } else {
            match x_cell.state {
                CellState::Valid => (XVal::Category(x_cell.original.clone()), x_cell.original.clone()),
                _ => {
                    d.push(diag::blocking(code::CATEGORY_MISSING, Some(node.as_str()), "category cell is empty; a category is required"));
                    (XVal::Missing, x_cell.original.clone())
                }
            }
        };

        let n_text = match n_col {
            None => None,
            Some(nc) => {
                let c = cell_at(row, &ds.columns[nc].id);
                match c.state {
                    CellState::Valid => Some(c.original.clone()),
                    CellState::Missing => {
                        if error_kind == Some(PlotErrorKind::Sem) {
                            d.push(diag::blocking(code::N_REQUIRED, Some(node.as_str()), "sem needs a sample count; n is never estimated"));
                        } else {
                            d.push(diag::info(code::N_NOT_PROVIDED, Some(node.as_str()), "sample count not provided; not estimated"));
                        }
                        None
                    }
                    _ => {
                        d.push(diag::blocking(
                            code::N_INVALID,
                            Some(node.as_str()),
                            format!("sample count \"{}\" must be a positive integer; not estimated", c.original),
                        ));
                        None
                    }
                }
            }
        };

        let mut ys: Vec<Option<YVal>> = Vec::with_capacity(yi.len());
        for (si, &ci) in yi.iter().enumerate() {
            let c = cell_at(row, &ds.columns[ci].id);
            let mean = match c.state {
                CellState::Valid => match number_of(c) {
                    Some(v) => Some(v),
                    None => {
                        d.push(diag::blocking(code::DATA_MALFORMED, Some(node.as_str()), format!("value \"{}\" is malformed", c.original)));
                        None
                    }
                },
                CellState::Missing => {
                    d.push(diag::info(code::MISSING_VALUE, Some(node.as_str()), "value is empty: not drawn, not treated as 0, not estimated"));
                    None
                }
                CellState::Ambiguous => {
                    d.push(diag::blocking(code::DATA_AMBIGUOUS, Some(node.as_str()), format!("value \"{}\" is ambiguous", c.original)));
                    None
                }
                CellState::Malformed => {
                    d.push(diag::blocking(code::DATA_MALFORMED, Some(node.as_str()), format!("value \"{}\" is malformed", c.original)));
                    None
                }
            };
            // Error bars belong to the single series (checked above).
            let mut err: Option<ErrBar> = None;
            if si == 0 {
                match &err_cols {
                    None => {}
                    Some(ErrCols::Sym(a)) => {
                        let amount = magnitude(cell_at(row, &ds.columns[*a].id), &node, &ds.columns[*a].id, &mut d);
                        if let (Some((amt, amt_text)), Some(m)) = (amount, mean) {
                            if let (Some(lo), Some(hi)) = (m.sub(amt), m.add(amt)) {
                                err = Some(ErrBar { lower: lo, upper: hi, label: format!("{} ± {}", c.original, amt_text) });
                            }
                        }
                    }
                    Some(ErrCols::Asym(a, b)) => {
                        let minus = magnitude(cell_at(row, &ds.columns[*a].id), &node, &ds.columns[*a].id, &mut d);
                        let plus = magnitude(cell_at(row, &ds.columns[*b].id), &node, &ds.columns[*b].id, &mut d);
                        if let (Some((mi, mi_text)), Some((pl, pl_text)), Some(m)) = (minus, plus, mean) {
                            if let (Some(lo), Some(hi)) = (m.sub(mi), m.add(pl)) {
                                err = Some(ErrBar { lower: lo, upper: hi, label: format!("{} +{}/-{}", c.original, pl_text, mi_text) });
                            }
                        }
                    }
                    Some(ErrCols::Bounds(a, b)) => {
                        let lo_c = cell_at(row, &ds.columns[*a].id);
                        let hi_c = cell_at(row, &ds.columns[*b].id);
                        let lo_v = bound_value(lo_c, "lower", &node, &mut d);
                        let hi_v = bound_value(hi_c, "upper", &node, &mut d);
                        if let (Some(l), Some(h)) = (lo_v, hi_v) {
                            if l > h {
                                d.push(diag::blocking(code::BOUNDS_ORDER, Some(node.as_str()), "lower bound is greater than upper bound"));
                            }
                            if let Some(m) = mean {
                                if m < l || m > h {
                                    d.push(diag::blocking(code::CENTER_OUTSIDE_BOUNDS, Some(node.as_str()), "value is outside its lower and upper bounds"));
                                }
                                err = Some(ErrBar { lower: l, upper: h, label: format!("{} [{}, {}]", c.original, lo_c.original, hi_c.original) });
                            }
                        }
                    }
                }
            }
            if let Some(m) = mean {
                if log && !m.is_positive() {
                    d.push(diag::blocking(
                        code::LOG_DOMAIN_VALUE,
                        Some(node.as_str()),
                        format!("value \"{}\" is outside the log domain; not clipped", c.original),
                    ));
                }
                if let Some(e) = &err {
                    if log && !e.lower.is_positive() {
                        d.push(diag::blocking(
                            code::LOG_DOMAIN_ERROR_BOUND,
                            Some(node.as_str()),
                            "lower error bound is not greater than 0 on a log axis; not clipped",
                        ));
                    }
                    widen(&mut y_lo, &mut y_hi, e.lower);
                    widen(&mut y_lo, &mut y_hi, e.upper);
                }
                widen(&mut y_lo, &mut y_hi, m);
            }
            ys.push(mean.map(|m| YVal { mean: m, mean_text: c.original.clone(), err }));
        }
        rows.push(Row { row_id: row.id.clone(), x, x_text, n_text, ys });
    }

    // Duplicate categories are kept, reported, and never averaged.
    if !x_numeric {
        let mut seen: Vec<(String, usize)> = Vec::new();
        for r in &rows {
            if let XVal::Category(c) = &r.x {
                match seen.iter_mut().find(|(k, _)| k == c) {
                    Some(entry) => entry.1 += 1,
                    None => seen.push((c.clone(), 1)),
                }
            }
        }
        for (c, n) in seen.iter().filter(|(_, n)| *n > 1) {
            d.push(diag::blocking(
                code::DUPLICATE_SERIES_ROW,
                Some(pid),
                format!("category \"{c}\" appears {n} times: rows are kept, nothing is averaged; an explicit split key is required"),
            ));
        }
    }

    let (Some(lo), Some(hi)) = (y_lo, y_hi) else {
        d.push(diag::blocking(code::NO_PLOTTABLE_DATA, Some(pid), "no plottable values in this plot"));
        return finish(None, d);
    };
    let y = match axis::resolve(y_scale, y_explicit, y_step, (lo, hi), kind == Kind::Bar) {
        Ok(a) => a,
        Err((c, m)) => {
            d.push(diag::blocking(c, Some(pid), m));
            return finish(None, d);
        }
    };
    if y_explicit.is_some() {
        for r in &rows {
            for v in r.ys.iter().flatten() {
                let mut values = vec![v.mean];
                if let Some(e) = &v.err {
                    values.push(e.lower);
                    values.push(e.upper);
                }
                if values.iter().any(|x| *x < y.min || *x > y.max) {
                    let node = format!("{pid}/{}", r.row_id);
                    d.push(diag::blocking(
                        code::VALUE_OUTSIDE_AXIS_RANGE,
                        Some(node.as_str()),
                        format!("value \"{}\" or its error is outside the y axis range; not clipped", v.mean_text),
                    ));
                }
            }
        }
    }

    let x_axis = match &x_spec {
        None => None,
        Some((xs_ref, xe_ref, xstep_ref, _)) => {
            let (xs, xe, xstep) = (*xs_ref, *xe_ref, *xstep_ref);
            let (Some(xlo), Some(xhi)) = (x_lo, x_hi) else {
                d.push(diag::blocking(code::NO_PLOTTABLE_DATA, Some(pid), "no plottable x values in this plot"));
                return finish(None, d);
            };
            match axis::resolve(xs, xe, xstep, (xlo, xhi), false) {
                Ok(a) => {
                    for r in &rows {
                        if let XVal::Number(v) = r.x {
                            if v < a.min || v > a.max {
                                let node = format!("{pid}/{}", r.row_id);
                                d.push(diag::blocking(
                                    code::VALUE_OUTSIDE_AXIS_RANGE,
                                    Some(node.as_str()),
                                    format!("x value \"{}\" is outside the x axis range; not clipped", r.x_text),
                                ));
                            }
                        }
                    }
                    Some(a)
                }
                Err((c, m)) => {
                    d.push(diag::blocking(c, Some(pid), m));
                    return finish(None, d);
                }
            }
        }
    };

    let x_caption = match &x_spec {
        Some((_, _, _, unit)) => caption(&spec.x_axis.label, unit),
        None => spec.x_axis.label.clone(),
    };
    let data = PlotData {
        kind,
        x_numeric,
        series: yi.iter().map(|&i| ds.columns[i].id.clone()).collect(),
        rows,
        error: error_kind,
        confidence_level: confidence,
        y,
        x_axis,
        y_caption: caption(&spec.y_axis.label, &y_unit),
        x_caption,
        dataset_id: ds.id.clone(),
    };
    finish(Some(data), d)
}

fn bound_value(cell: &DataCell, which: &str, node: &str, d: &mut Vec<Diagnostic>) -> Option<Exact> {
    match cell.state {
        CellState::Valid => number_of(cell),
        CellState::Missing => {
            d.push(diag::blocking(code::ERROR_MISSING, Some(node), format!("{which} bound is empty: nothing is estimated")));
            None
        }
        CellState::Ambiguous => {
            d.push(diag::blocking(code::DATA_AMBIGUOUS, Some(node), format!("{which} bound \"{}\" is ambiguous", cell.original)));
            None
        }
        CellState::Malformed => {
            d.push(diag::blocking(code::DATA_MALFORMED, Some(node), format!("{which} bound \"{}\" is malformed", cell.original)));
            None
        }
    }
}
