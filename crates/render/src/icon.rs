//! Icon rasterizer for the GUI icon cache (GUI_DESIGN §6.5, §17.5). It draws the SVG subset that
//! `design/icons` uses: the root `<svg>` with presentation attributes, `<path>`, and `<defs>` holding
//! `<linearGradient>` with `<stop>`. Strokes use the renderer's stroke model ([`crate::effects`]):
//! the stroke outline is filled, so a stroke of one device pixel keeps its joins and a closed subpath no larger
//! than the stroke width is a filled dot. Anything outside the subset is refused by name
//! (UNSUPPORTED_OPERATION) instead of being skipped.
use crate::effects;
use crate::paint::{parse_color, Rgba};
use designllm_contracts::*;
use std::collections::BTreeMap;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

/// Straight (not premultiplied) RGBA8 pixels, row-major, `width` × `height`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconRaster { pub width: u32, pub height: u32, pub rgba: Vec<u8> }

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconOptions {
    /// Paint of `currentColor`, straight RGBA. Opaque white lets the GUI tint a one-colour icon.
    pub current_color: [u8; 4],
    /// Replaces the root `stroke-width` in user units, as CSS `stroke-width` on the `<svg>` element
    /// does (paths with their own `stroke-width` keep it).
    pub stroke_width: Option<f32>,
}
impl Default for IconOptions { fn default() -> Self { Self { current_color: [255, 255, 255, 255], stroke_width: None } } }

/// Rasterizes `svg` into `width` × `height` device pixels (viewBox scaled uniformly and centred,
/// SVG `xMidYMid meet`). Deterministic; the root `width`/`height` attributes do not change the size.
pub fn rasterize_icon(svg: &str, width: u32, height: u32, options: &IconOptions) -> DltResult<IconRaster> {
    let doc = parse(svg)?;
    let mut pm = Pixmap::new(width, height).ok_or_else(|| DltError::invalid(format!("icon raster {width}x{height} is empty or too large")))?;
    let (vx, vy, vw, vh) = doc.view_box;
    let scale = (width as f32 / vw).min(height as f32 / vh);
    let ts = Transform::from_row(scale, 0.0, 0.0, scale, (width as f32 - vw * scale) / 2.0 - vx * scale, (height as f32 - vh * scale) / 2.0 - vy * scale);
    for p in &doc.paths {
        let props = p.props.inherit(&doc.root);
        let user = user_path(&p.spec)?;
        if let Some(paint) = props.paint(props.fill.as_deref().unwrap_or("black"), props.fill_opacity, &doc.gradients, &user, ts, options)? {
            pm.fill_path(&user, &paint, if props.fill_rule.as_deref() == Some("evenodd") { FillRule::EvenOdd } else { FillRule::Winding }, ts, None);
        }
        if let Some(paint) = props.paint(props.stroke.as_deref().unwrap_or("none"), props.stroke_opacity, &doc.gradients, &user, ts, options)? {
            let width = match (options.stroke_width, p.props.stroke_width) { (_, Some(own)) => own, (Some(w), None) => w, (None, None) => props.stroke_width.unwrap_or(1.0) };
            if width <= 0.0 { continue; }
            // Stroked in device pixels (uniform scale), exactly as the document path strokes Primitive::Path.
            let dash = match props.dash.as_deref() { None | Some("none") => None, Some(d) => Some(StrokeDash::new({ let mut v: Vec<f32> = numbers(d, "stroke-dasharray")?.into_iter().map(|v| v * scale).collect(); if v.len() % 2 == 1 { v.extend(v.clone()); } v }, 0.0).ok_or_else(|| DltError::unsupported(format!("stroke-dasharray '{d}' is not drawn")))?) };
            let stroke = Stroke { width: width * scale, miter_limit: props.miter.unwrap_or(4.0), line_cap: props.cap, line_join: props.join, dash };
            let Some(device) = user.clone().transform(ts) else { continue };
            effects::stroke_as_fill(&mut pm, &device, &stroke, &paint, Transform::identity(), None);
        }
    }
    let rgba = pm.pixels().iter().flat_map(|p| { let c = p.demultiply(); [c.red(), c.green(), c.blue(), c.alpha()] }).collect();
    Ok(IconRaster { width, height, rgba })
}

// ---------------------------------------------------------------- model

struct Doc { view_box: (f32, f32, f32, f32), root: Props, paths: Vec<PathEl>, gradients: BTreeMap<String, Grad> }
struct PathEl { spec: PathSpec, props: Props }
struct Grad { units_user: bool, coords: [Coord; 4], stops: Vec<(f32, Rgba)> }
#[derive(Clone, Copy)]
enum Coord { Number(f32), Percent(f32) }

/// Presentation attributes (inherited from the root when a path does not set them).
#[derive(Clone, Default)]
struct Props {
    fill: Option<String>, stroke: Option<String>, stroke_width: Option<f32>, fill_opacity: Option<f32>, stroke_opacity: Option<f32>,
    fill_rule: Option<String>, dash: Option<String>, miter: Option<f32>, cap_set: Option<LineCap>, join_set: Option<LineJoin>,
    cap: LineCap, join: LineJoin,
}

impl Props {
    fn inherit(&self, root: &Props) -> Props {
        let pick = |a: &Option<String>, b: &Option<String>| a.clone().or_else(|| b.clone());
        let cap = self.cap_set.or(root.cap_set);
        let join = self.join_set.or(root.join_set);
        Props {
            fill: pick(&self.fill, &root.fill), stroke: pick(&self.stroke, &root.stroke), stroke_width: self.stroke_width.or(root.stroke_width),
            fill_opacity: self.fill_opacity.or(root.fill_opacity), stroke_opacity: self.stroke_opacity.or(root.stroke_opacity),
            fill_rule: pick(&self.fill_rule, &root.fill_rule), dash: pick(&self.dash, &root.dash), miter: self.miter.or(root.miter),
            cap_set: cap, join_set: join, cap: cap.unwrap_or(LineCap::Butt), join: join.unwrap_or(LineJoin::Miter),
        }
    }

    /// Paint for a fill or stroke value: none, currentColor, a colour, or `url(#id)` of a gradient.
    fn paint(&self, value: &str, opacity: Option<f32>, grads: &BTreeMap<String, Grad>, user: &tiny_skia::Path, ts: Transform, options: &IconOptions) -> DltResult<Option<Paint<'static>>> {
        let op = opacity.unwrap_or(1.0).clamp(0.0, 1.0);
        let v = value.trim();
        let mut paint = Paint { anti_alias: true, ..Paint::default() };
        if let Some(id) = v.strip_prefix("url(#").and_then(|r| r.strip_suffix(')')) {
            let g = grads.get(id).ok_or_else(|| DltError::unsupported(format!("paint url(#{id}) names no linearGradient")))?;
            let b = user.bounds();
            let at = |c: Coord, origin: f32, size: f32| match (c, g.units_user) { (Coord::Number(n), true) => n, (Coord::Percent(p), true) => p * size, (Coord::Number(n) | Coord::Percent(n), false) => origin + n * size };
            let (x1, y1, x2, y2) = (at(g.coords[0], b.left(), b.width()), at(g.coords[1], b.top(), b.height()), at(g.coords[2], b.left(), b.width()), at(g.coords[3], b.top(), b.height()));
            let stops = g.stops.iter().map(|(o, c)| tiny_skia::GradientStop::new(*o, tiny_skia::Color::from_rgba8(c.r, c.g, c.b, effects::faded(*c, op).a))).collect();
            paint.shader = tiny_skia::LinearGradient::new(tiny_skia::Point::from_xy(x1, y1), tiny_skia::Point::from_xy(x2, y2), stops, tiny_skia::SpreadMode::Pad, ts)
                .ok_or_else(|| DltError::unsupported(format!("linearGradient '{id}' has no drawable geometry")))?;
            return Ok(Some(paint));
        }
        let c = if v.eq_ignore_ascii_case("currentcolor") {
            let [r, g, b, a] = options.current_color;
            Rgba { r, g, b, a }
        } else {
            match parse_color(v) { Ok(Some(c)) => c, Ok(None) => return Ok(None), Err(()) => return Err(DltError::unsupported(format!("icon paint '{v}' is not supported"))) }
        };
        let c = effects::faded(c, op);
        paint.set_color_rgba8(c.r, c.g, c.b, c.a);
        Ok(Some(paint))
    }
}

/// PathSpec (user units × 1000) as a tiny-skia path in user units.
fn user_path(spec: &PathSpec) -> DltResult<tiny_skia::Path> {
    let u = |v: i64| v as f32 / 1000.0;
    let mut pb = PathBuilder::new();
    for c in &spec.commands {
        match *c {
            PathCommand::MoveTo { x, y } => pb.move_to(u(x), u(y)),
            PathCommand::LineTo { x, y } => pb.line_to(u(x), u(y)),
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } => pb.cubic_to(u(x1), u(y1), u(x2), u(y2), u(x), u(y)),
            PathCommand::Close => pb.close(),
        }
    }
    pb.finish().ok_or_else(|| DltError::invalid("icon path has no drawable segments"))
}

// ---------------------------------------------------------------- parsing

fn number(v: &str, what: &str) -> DltResult<f32> {
    v.trim().parse::<f32>().ok().filter(|n| n.is_finite()).ok_or_else(|| DltError::invalid(format!("{what} '{v}' is not a number")))
}
fn numbers(v: &str, what: &str) -> DltResult<Vec<f32>> {
    v.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|s| !s.is_empty()).map(|s| number(s, what)).collect()
}
fn coord(v: &str, what: &str) -> DltResult<Coord> {
    match v.trim().strip_suffix('%') { Some(p) => Ok(Coord::Percent(number(p, what)? / 100.0)), None => Ok(Coord::Number(number(v, what)?)) }
}
fn opacity(v: &str, what: &str) -> DltResult<f32> {
    match v.trim().strip_suffix('%') { Some(p) => Ok(number(p, what)? / 100.0), None => number(v, what) }
}

/// Reads one presentation attribute into `props`; false when `name` is not one.
fn presentation(props: &mut Props, name: &str, value: &str) -> DltResult<bool> {
    let bad = |what: &str| DltError::unsupported(format!("{what} '{value}' is not supported in icons"));
    match name {
        "fill" => props.fill = Some(value.to_string()),
        "stroke" => props.stroke = Some(value.to_string()),
        "stroke-width" => props.stroke_width = Some(number(value, name)?),
        "fill-opacity" => props.fill_opacity = Some(opacity(value, name)?),
        "stroke-opacity" => props.stroke_opacity = Some(opacity(value, name)?),
        "fill-rule" => props.fill_rule = Some(match value.trim() { "nonzero" | "evenodd" => value.trim().to_string(), _ => return Err(bad(name)) }),
        "stroke-dasharray" => props.dash = Some(value.to_string()),
        "stroke-miterlimit" => props.miter = Some(number(value, name)?),
        "stroke-linecap" => props.cap_set = Some(match value.trim() { "butt" => LineCap::Butt, "round" => LineCap::Round, "square" => LineCap::Square, _ => return Err(bad(name)) }),
        "stroke-linejoin" => props.join_set = Some(match value.trim() { "miter" => LineJoin::Miter, "round" => LineJoin::Round, "bevel" => LineJoin::Bevel, _ => return Err(bad(name)) }),
        _ => return Ok(false),
    }
    Ok(true)
}

struct Tag { name: String, attrs: Vec<(String, String)>, closing: bool, empty: bool }

fn unescape(v: &str) -> String { v.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&") }

/// Tags of `svg` in order; text between tags must be blank.
fn tags(svg: &str) -> DltResult<Vec<Tag>> {
    let s = svg.trim_start_matches('\u{feff}');
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let Some(lt) = s[i..].find('<') else {
            if !s[i..].trim().is_empty() { return Err(DltError::unsupported("text content is not supported in icons")); }
            break;
        };
        if !s[i..i + lt].trim().is_empty() { return Err(DltError::unsupported("text content is not supported in icons")); }
        let at = i + lt;
        let rest = &s[at..];
        if rest.starts_with("<!--") { i = at + rest.find("-->").ok_or_else(|| DltError::invalid("unterminated comment"))? + 3; continue; }
        if rest.starts_with("<?") { i = at + rest.find("?>").ok_or_else(|| DltError::invalid("unterminated declaration"))? + 2; continue; }
        if rest.starts_with("<!") { return Err(DltError::unsupported("DOCTYPE and CDATA are not supported in icons")); }
        // The tag ends at the first '>' outside quotes.
        let mut quote: Option<char> = None;
        let mut end = None;
        for (k, ch) in rest.char_indices().skip(1) {
            match (quote, ch) { (Some(q), c) if c == q => quote = None, (None, '"' | '\'') => quote = Some(ch), (None, '>') => { end = Some(k); break; } _ => {} }
        }
        let end = end.ok_or_else(|| DltError::invalid("unterminated tag"))?;
        let mut inner = &rest[1..end];
        let closing = inner.starts_with('/');
        if closing { inner = &inner[1..]; }
        let empty = inner.ends_with('/');
        if empty { inner = &inner[..inner.len() - 1]; }
        let inner = inner.trim();
        let name_end = inner.find(|c: char| c.is_ascii_whitespace()).unwrap_or(inner.len());
        let name = inner[..name_end].to_string();
        let mut attrs = Vec::new();
        let mut a = inner[name_end..].trim_start();
        while !a.is_empty() {
            let eq = a.find('=').ok_or_else(|| DltError::invalid(format!("attribute without a value in <{name}>")))?;
            let key = a[..eq].trim().to_string();
            let v = a[eq + 1..].trim_start();
            let q = v.chars().next().filter(|c| *c == '"' || *c == '\'').ok_or_else(|| DltError::invalid(format!("attribute {key} of <{name}> is not quoted")))?;
            let close = v[1..].find(q).ok_or_else(|| DltError::invalid(format!("attribute {key} of <{name}> is not terminated")))?;
            attrs.push((key, unescape(&v[1..1 + close])));
            a = v[close + 2..].trim_start();
        }
        out.push(Tag { name, attrs, closing, empty });
        i = at + end + 1;
    }
    Ok(out)
}

fn parse(svg: &str) -> DltResult<Doc> {
    let tags = tags(svg)?;
    let mut doc = Doc { view_box: (0.0, 0.0, 0.0, 0.0), root: Props::default(), paths: Vec::new(), gradients: BTreeMap::new() };
    let mut stack: Vec<String> = Vec::new();
    let mut current: Option<(String, Grad)> = None;
    let mut root_seen = false;
    let mut size = (None, None);
    for t in tags {
        if t.closing {
            if stack.pop().as_deref() != Some(t.name.as_str()) { return Err(DltError::invalid(format!("</{}> does not close the open element", t.name))); }
            if t.name == "linearGradient" { if let Some((id, g)) = current.take() { doc.gradients.insert(id, g); } }
            continue;
        }
        let parent = stack.last().map(String::as_str);
        let unsupported_attr = |name: &str, el: &str| DltError::unsupported(format!("attribute {name} of <{el}> is not supported in icons"));
        match (parent, t.name.as_str()) {
            (None, "svg") if !root_seen => {
                root_seen = true;
                for (k, v) in &t.attrs {
                    if presentation(&mut doc.root, k, v)? { continue; }
                    match k.as_str() {
                        "viewBox" => { let n = numbers(v, "viewBox")?; if n.len() != 4 || n[2] <= 0.0 || n[3] <= 0.0 { return Err(DltError::invalid(format!("viewBox '{v}' needs four numbers and a positive size"))); } doc.view_box = (n[0], n[1], n[2], n[3]); }
                        "width" => size.0 = Some(number(v.trim_end_matches("px"), "width")?),
                        "height" => size.1 = Some(number(v.trim_end_matches("px"), "height")?),
                        "xmlns" | "xmlns:xlink" | "version" | "id" | "class" | "aria-hidden" | "role" | "focusable" => {}
                        _ => return Err(unsupported_attr(k, "svg")),
                    }
                }
            }
            (Some("svg"), "path") => {
                let mut props = Props::default();
                let mut d = None;
                for (k, v) in &t.attrs {
                    if presentation(&mut props, k, v)? { continue; }
                    match k.as_str() { "d" => d = Some(v.clone()), "id" | "class" => {} _ => return Err(unsupported_attr(k, "path")) }
                }
                let d = d.ok_or_else(|| DltError::invalid("<path> has no d"))?;
                if doc.view_box.2 <= 0.0 {
                    let (w, h) = (size.0.ok_or_else(|| DltError::invalid("icon needs a viewBox or width and height"))?, size.1.ok_or_else(|| DltError::invalid("icon needs a viewBox or width and height"))?);
                    doc.view_box = (0.0, 0.0, w, h);
                }
                let vb = Rect { x: (doc.view_box.0 * 1000.0).round() as i64, y: (doc.view_box.1 * 1000.0).round() as i64, width: (doc.view_box.2 * 1000.0).round() as i64, height: (doc.view_box.3 * 1000.0).round() as i64 };
                let mut spec = PathSpec::from_svg(&d, vb)?;
                if props.fill_rule.as_deref() == Some("evenodd") { spec.fill_rule = FillRule_::Evenodd; }
                doc.paths.push(PathEl { spec, props });
            }
            (Some("svg"), "defs") => {}
            (Some("defs"), "linearGradient") => {
                let mut id = None;
                let mut g = Grad { units_user: false, coords: [Coord::Percent(0.0), Coord::Percent(0.0), Coord::Percent(1.0), Coord::Percent(0.0)], stops: Vec::new() };
                for (k, v) in &t.attrs {
                    match k.as_str() {
                        "id" => id = Some(v.clone()),
                        "x1" => g.coords[0] = coord(v, k)?, "y1" => g.coords[1] = coord(v, k)?, "x2" => g.coords[2] = coord(v, k)?, "y2" => g.coords[3] = coord(v, k)?,
                        "gradientUnits" => g.units_user = match v.trim() { "userSpaceOnUse" => true, "objectBoundingBox" => false, _ => return Err(unsupported_attr(k, "linearGradient")) },
                        _ => return Err(unsupported_attr(k, "linearGradient")),
                    }
                }
                let id = id.ok_or_else(|| DltError::invalid("<linearGradient> has no id"))?;
                if t.empty { doc.gradients.insert(id, g); } else { current = Some((id, g)); }
            }
            (Some("linearGradient"), "stop") => {
                let (mut offset, mut color, mut op) = (0.0f32, Rgba { r: 0, g: 0, b: 0, a: 255 }, 1.0f32);
                for (k, v) in &t.attrs {
                    match k.as_str() {
                        "offset" => offset = opacity(v, k)?.clamp(0.0, 1.0),
                        "stop-color" => color = match parse_color(v) { Ok(Some(c)) => c, Ok(None) => Rgba { r: 0, g: 0, b: 0, a: 0 }, Err(()) => return Err(unsupported_attr(k, "stop")) },
                        "stop-opacity" => op = opacity(v, k)?,
                        _ => return Err(unsupported_attr(k, "stop")),
                    }
                }
                let (_, g) = current.as_mut().ok_or_else(|| DltError::invalid("<stop> outside a linearGradient"))?;
                // SVG: an offset below the previous one is raised to it.
                let offset = g.stops.last().map_or(offset, |(prev, _)| offset.max(*prev));
                g.stops.push((offset, effects::faded(color, op.clamp(0.0, 1.0))));
            }
            (_, other) => return Err(DltError::unsupported(format!("<{other}> is not supported in icons (svg, path, defs, linearGradient, stop)"))),
        }
        if !t.empty { stack.push(t.name.clone()); }
    }
    if !root_seen { return Err(DltError::invalid("icon has no <svg> root")); }
    if !stack.is_empty() { return Err(DltError::invalid(format!("<{}> is not closed", stack.join("> <")))); }
    if doc.view_box.2 <= 0.0 {
        match size { (Some(w), Some(h)) if w > 0.0 && h > 0.0 => doc.view_box = (0.0, 0.0, w, h), _ => return Err(DltError::invalid("icon needs a viewBox or width and height")) }
    }
    Ok(doc)
}

use crate::effects::FillRule_;
