//! Approximate PowerPoint text fit in points, preserving all frame geometry.
//! These estimates cannot reproduce native shaping, font substitution or pagination.
use std::ops::Range;

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::{Error, Paragraph};

const EMU_PER_POINT: f64 = 12700.0;
// Keep text readable. If it cannot fit above this size, report overflow.
const MIN_FONT: f64 = 8.0;
// Font substitution and bold runs can make PowerPoint lines wider than the estimate.
const WIDTH_RESERVE: f64 = 0.88;

#[derive(Clone, Default)]
struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

pub(super) struct Frame {
    pub(super) range: Range<usize>,
    size: Bounds,
    horizontal_inset: f64,
    vertical_inset: f64,
    wrap: bool,
    arrow: bool,
    rect: bool,
    top_aligned: bool,
    blank_height: f64,
    has_text: bool,
    body: Option<Range<usize>>,
    cell: Option<Range<usize>>,
    pub(super) index: usize,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            range: 0..0,
            size: Bounds::default(),
            horizontal_inset: 14.4,
            vertical_inset: 7.2,
            wrap: true,
            arrow: false,
            rect: false,
            top_aligned: true,
            blank_height: 0.0,
            has_text: false,
            body: None,
            cell: None,
            index: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Fit {
    scale: f64,
    pub(super) overflow: bool,
}

impl Frame {
    pub(super) fn is_cell(&self) -> bool {
        self.cell.is_some()
    }

    pub(super) fn contains(&self, paragraph: &Paragraph) -> bool {
        self.range.contains(&paragraph.body.start)
    }

    // Short kana-only labels in tiny frames are typically reading guides. They
    // remain in the source and are reported; this does not infer their parent text.
    pub(super) fn likely_furigana(&self, paragraph: &Paragraph) -> bool {
        let text = paragraph.plain_text();
        let text = text.trim();
        self.size.height <= 20.0
            && paragraph.font_size.is_some_and(|size| size <= 8.0)
            && (1..=14).contains(&text.chars().count())
            && text
                .chars()
                .all(|c| matches!(c, 'ぁ'..='ゖ' | 'ー' | ' ' | '　'))
    }

    pub(super) fn fit<'a>(
        &self,
        paragraphs: impl Iterator<Item = (&'a Paragraph, Option<&'a str>)>,
    ) -> Option<Fit> {
        let mut lines = Vec::new();
        let mut translated = false;
        let mut complete = true;
        for (paragraph, target) in paragraphs {
            complete &= paragraph.font_size.is_some();
            translated |= target.is_some();
            let Some(font) = paragraph.font_size else {
                continue;
            };
            let visible = target
                .and_then(|text| paragraph.visible_target(text).ok())
                .unwrap_or_else(|| paragraph.plain_text());
            lines.push(Text {
                visible,
                translated: target.is_some(),
                font,
                spacing: Spacing::read(&paragraph.before),
            });
        }
        let width = self.size.width - self.horizontal_inset;
        let height = self.size.height - self.vertical_inset - self.blank_height;
        if !translated || lines.is_empty() || width <= 0.0 || height <= 0.0 {
            return None;
        }

        let overflow = |scale, wrap, height, width_factor| {
            let mut total = 0.0;
            for line in &lines {
                // Retained source text is not rewritten or shrunk. Account for
                // its full height while fitting only the translated paragraphs.
                let scale = if line.translated { scale } else { 1.0 };
                let font = scaled_font(line.font, scale);
                // The text rectangle on a pentagonal arrow ends before its tip.
                let available = width
                    - line.spacing.indent
                    - if self.arrow {
                        self.size.height * 0.28
                    } else {
                        0.0
                    };
                let Some(count) = line_count(&line.visible, available * width_factor / font, wrap)
                else {
                    return true;
                };
                let line_height = line
                    .spacing
                    .line_points
                    .unwrap_or(font * line.spacing.line_ratio);
                total += f64::from(count) * line_height + line.spacing.extra_points;
            }
            total > height
        };

        if !overflow(1.0, self.wrap, height, WIDTH_RESERVE) {
            return None;
        }
        if !complete || (self.body.is_none() && self.cell.is_none()) {
            return Some(Fit {
                scale: 1.0,
                overflow: true,
            });
        }

        for width_factor in [WIDTH_RESERVE, 1.0] {
            for percentage in (80..=100).rev() {
                let scale = f64::from(percentage) / 100.0;
                if !overflow(scale, self.wrap, height, width_factor) {
                    return Some(Fit {
                        scale,
                        overflow: false,
                    });
                }
            }
        }
        // Keep the original sizes and full translation for manual review when
        // fitting would require more than 20% shrinking, including auto-fit shapes.
        Some(Fit {
            scale: 1.0,
            overflow: true,
        })
    }
}

struct Text {
    visible: String,
    translated: bool,
    font: f64,
    spacing: Spacing,
}

struct Spacing {
    indent: f64,
    extra_points: f64,
    line_points: Option<f64>,
    line_ratio: f64,
}

impl Spacing {
    fn read(xml: &str) -> Self {
        let mut spacing = Self {
            indent: 0.0,
            extra_points: 0.0,
            line_points: None,
            line_ratio: 1.2,
        };
        let mut reader = Reader::from_str(xml);
        let mut parent = None;
        while let Ok(event) = reader.read_event() {
            match event {
                Event::Start(tag) => {
                    let name = tag.local_name();
                    parent = match name.as_ref() {
                        b"lnSpc" => Some(true),
                        b"spcBef" | b"spcAft" => Some(false),
                        _ => None,
                    };
                    if name.as_ref() == b"pPr" {
                        spacing.indent = points(&tag, b"marL").unwrap_or(0.0)
                            + points(&tag, b"marR").unwrap_or(0.0);
                    }
                }
                Event::Empty(tag) => match (parent, tag.local_name().as_ref()) {
                    (Some(true), b"spcPts") => {
                        spacing.line_points = number(&tag, b"val").map(|n| n / 100.0)
                    }
                    (Some(true), b"spcPct") => {
                        spacing.line_ratio *= number(&tag, b"val").unwrap_or(100000.0) / 100000.0
                    }
                    (Some(false), b"spcPts") => {
                        spacing.extra_points += number(&tag, b"val").unwrap_or(0.0) / 100.0
                    }
                    _ => {}
                },
                Event::End(_) => parent = None,
                Event::Eof => break,
                _ => {}
            }
        }
        spacing
    }
}

fn line_count(text: &str, capacity: f64, wrap: bool) -> Option<u32> {
    if capacity <= 0.0 {
        return None;
    }
    let mut count = 0;
    for line in text.split('\n') {
        count += 1;
        if !wrap {
            if text_width(line) > capacity {
                return None;
            }
            continue;
        }
        // Latin words wrap as units. CJK text can normally wrap between characters.
        let mut used = 0.0;
        let mut word = 0.0;
        let mut gap = 0.0;
        let mut add_word = |word: f64, gap: f64| {
            if word > capacity {
                return None;
            }
            if used > 0.0 && used + gap + word > capacity {
                count += 1;
                used = word;
            } else {
                used += gap + word;
            }
            Some(())
        };
        for c in line.chars() {
            if c.is_whitespace() {
                if word > 0.0 {
                    add_word(word, gap)?;
                    word = 0.0;
                    gap = 0.0;
                }
                gap += 0.28;
            } else if c.is_ascii() || c.is_alphabetic() && c < '\u{2e80}' {
                word += char_width(c);
            } else {
                if word > 0.0 {
                    add_word(word, gap)?;
                    word = 0.0;
                    gap = 0.0;
                }
                add_word(char_width(c), gap)?;
                gap = 0.0;
            }
        }
        if word > 0.0 {
            add_word(word, gap)?;
        }
    }
    Some(count)
}

fn scaled_font(font: f64, scale: f64) -> f64 {
    // Office stores hundredths of a point. Round upward so the written size
    // never crosses the scale/absolute floor; use that same size for fitting.
    let points = (font * scale).max(MIN_FONT.min(font));
    (points * 100.0 - 1e-9).ceil() / 100.0
}

impl Fit {
    pub(super) fn applies(&self) -> bool {
        self.scale < 1.0
    }

    #[cfg(test)]
    pub(super) fn compact_paragraph(&self, xml: String) -> Result<String, Error> {
        self.compact_paragraph_with_default(xml, None)
    }

    pub(super) fn compact_paragraph_with_default(
        &self,
        xml: String,
        default: Option<f64>,
    ) -> Result<String, Error> {
        let mut reader = Reader::from_str(&xml);
        let mut edits = Vec::new();
        loop {
            let start = reader.buffer_position() as usize;
            match reader.read_event().map_err(|_| Error::Xml)? {
                Event::Start(tag) | Event::Empty(tag) => {
                    // A run can inherit its entire properties element. Materialize
                    // only the resolved size when the fitting operation needs it.
                    if tag.local_name().as_ref() == b"r"
                        && self.scale < 1.0
                        && let Some(font) = default
                    {
                        let end = reader.buffer_position() as usize;
                        let mut next = Reader::from_str(&xml[end..]);
                        let has_properties = matches!(next.read_event(), Ok(Event::Start(t) | Event::Empty(t)) if t.local_name().as_ref() == b"rPr");
                        if !has_properties {
                            let name = tag.name();
                            let name =
                                std::str::from_utf8(name.as_ref()).map_err(|_| Error::Xml)?;
                            let prefix = name
                                .rsplit_once(':')
                                .map_or(String::new(), |(p, _)| format!("{p}:"));
                            edits.push((
                                end..end,
                                format!(
                                    "<{prefix}rPr sz=\"{:.0}\"/>",
                                    (scaled_font(font, self.scale) * 100.0).round()
                                ),
                            ));
                        }
                    }
                    let (key, value) = match tag.local_name().as_ref() {
                        b"rPr" | b"defRPr" | b"endParaRPr" if self.scale < 1.0 => (
                            b"sz".as_slice(),
                            number(&tag, b"sz")
                                .or(default.map(|s| s * 100.0))
                                .map(|n| scaled_font(n / 100.0, self.scale) * 100.0),
                        ),
                        _ => continue,
                    };
                    if let Some(value) = value {
                        let range = start..reader.buffer_position() as usize;
                        let updated = rewrite_tag(
                            &xml[range.clone()],
                            &[(key, &format!("{:.0}", value.round()))],
                        )?;
                        edits.push((range, updated));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        let mut xml = xml;
        for (range, replacement) in edits.into_iter().rev() {
            xml.replace_range(range, &replacement);
        }
        Ok(xml)
    }
}

// Approximate advances in ems for proportional Latin and full-width CJK.
// The width reserve above absorbs moderate font differences; native review remains necessary.
fn char_width(c: char) -> f64 {
    match c {
        ' ' | 'i' | 'l' | 'I' | '.' | ',' | ':' | '!' | '\'' => 0.28,
        'm' | 'w' | 'M' | 'W' => 0.87,
        c if c.is_ascii_uppercase() => 0.69,
        c if c.is_ascii() || c < '\u{2e80}' => 0.56,
        _ => 1.0,
    }
}

fn text_width(s: &str) -> f64 {
    s.chars().map(char_width).sum()
}

fn attribute<'a>(event: &'a BytesStart<'_>, key: &[u8]) -> Option<std::borrow::Cow<'a, [u8]>> {
    event
        .attributes()
        .flatten()
        .find(|attr| attr.key.as_ref() == key)
        .map(|attr| attr.value)
}

fn number(event: &BytesStart<'_>, key: &[u8]) -> Option<f64> {
    let value = attribute(event, key)?;
    std::str::from_utf8(&value)
        .ok()?
        .parse::<i64>()
        .ok()
        .map(|n| n as f64)
}

fn points(event: &BytesStart<'_>, key: &[u8]) -> Option<f64> {
    number(event, key).map(|n| n / EMU_PER_POINT)
}

pub(super) fn rewrite_tag(xml: &str, changes: &[(&[u8], &str)]) -> Result<String, Error> {
    let mut reader = Reader::from_str(xml);
    let (tag, empty) = match reader.read_event().map_err(|_| Error::Xml)? {
        Event::Start(tag) => (tag, false),
        Event::Empty(tag) => (tag, true),
        _ => return Err(Error::Xml),
    };
    let attributes = tag
        .attributes()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::Xml)?;
    let mut rewritten = tag.to_owned();
    rewritten.clear_attributes();
    for attr in attributes {
        if changes.iter().all(|(key, _)| *key != attr.key.as_ref()) {
            rewritten.push_attribute(attr);
        }
    }
    for (key, value) in changes {
        rewritten.push_attribute((*key, value.as_bytes()));
    }
    let suffix = if empty { "/>" } else { ">" };
    let raw = std::str::from_utf8(&rewritten).map_err(|_| Error::Xml)?;
    let rest = &xml[reader.buffer_position() as usize..];
    // Keep the original namespace prefix and any bodyPr children.
    Ok(format!("<{raw}{suffix}{rest}"))
}

pub(super) fn font_points(tag: &str) -> Option<f64> {
    let start = match Reader::from_str(tag).read_event().ok()? {
        Event::Start(start) | Event::Empty(start) => start,
        _ => return None,
    };
    number(&start, b"sz")
        .map(|size| size / 100.0)
        .filter(|size| *size > 0.0)
}

#[derive(PartialEq)]
enum Parent {
    Shape,
    Group,
    Properties,
    Transform,
    TextBody,
    Body,
    Graphic,
    Table,
    Grid,
    Row,
    Cell,
    Other,
}

#[derive(Default)]
struct TableLayout {
    bounds: Bounds,
    columns: Vec<f64>,
    row_height: f64,
    row_height_sum: f64,
    column: usize,
    merged: bool,
    cells: Vec<Frame>,
    top_level: bool,
}

pub(super) fn frames(xml: &str) -> Result<Vec<Frame>, Error> {
    use Parent::*;
    let mut reader = Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut shape = None::<Frame>;
    let mut cell = None::<Frame>;
    let mut table = None::<TableLayout>;
    let mut obstacles = Vec::new();
    let mut frames = Vec::new();
    let mut index = 0;
    loop {
        let start = reader.buffer_position() as usize;
        let event = reader.read_event().map_err(|_| Error::Xml)?;
        let end = reader.buffer_position() as usize;
        if let Event::Start(node) | Event::Empty(node) = &event {
            let name = node.local_name().into_inner();
            let kind = match name {
                b"sp" => Shape,
                b"grpSp" => Group,
                b"spPr" => Properties,
                b"xfrm" => Transform,
                b"txBody" => TextBody,
                b"bodyPr" => Body,
                b"graphicFrame" => Graphic,
                b"tbl" => Table,
                b"tblGrid" => Grid,
                b"tr" => Row,
                b"tc" => Cell,
                _ => Other,
            };
            let opened = matches!(event, Event::Start(_));
            match (&kind, stack.as_slice()) {
                (Shape, _) if opened => {
                    index += 1;
                    shape = Some(Frame {
                        range: start..start,
                        index,
                        ..Frame::default()
                    });
                }
                (Graphic, _) if opened => {
                    table = Some(TableLayout {
                        top_level: !stack.contains(&Group),
                        ..Default::default()
                    });
                }
                (Cell, [.., Table, Row]) if opened => {
                    if let Some(table) = &mut table {
                        table.merged |= [b"gridSpan".as_slice(), b"rowSpan", b"hMerge", b"vMerge"]
                            .iter()
                            .any(|name| attribute(node, name).is_some());
                        let width = table.columns.get(table.column).copied().unwrap_or(0.0);
                        table.column += 1;
                        index += 1;
                        cell = Some(Frame {
                            range: start..start,
                            size: Bounds {
                                width,
                                height: table.row_height,
                                ..Default::default()
                            },
                            index,
                            ..Frame::default()
                        });
                    }
                }
                _ => {}
            }
            if let Some(frame) = &mut shape {
                match (name, stack.as_slice()) {
                    (b"off", [.., Shape, Properties, Transform]) if !stack.contains(&Group) => {
                        frame.size.x = points(node, b"x").unwrap_or_default();
                        frame.size.y = points(node, b"y").unwrap_or_default();
                    }
                    (b"ext", [.., Shape, Properties, Transform]) => {
                        frame.size.width = points(node, b"cx").unwrap_or_default();
                        frame.size.height = points(node, b"cy").unwrap_or_default();
                    }
                    (b"prstGeom", [.., Shape, Properties]) => {
                        if let Some(preset) = attribute(node, b"prst") {
                            frame.arrow = matches!(
                                preset.as_ref(),
                                b"homePlate" | b"chevron" | b"rightArrow"
                            );
                            frame.rect = preset.as_ref() == b"rect" && !stack.contains(&Group);
                        }
                    }
                    (b"bodyPr", [.., Shape, TextBody]) => {
                        frame.horizontal_inset = points(node, b"lIns").unwrap_or(7.2)
                            + points(node, b"rIns").unwrap_or(7.2);
                        frame.vertical_inset = points(node, b"tIns").unwrap_or(3.6)
                            + points(node, b"bIns").unwrap_or(3.6);
                        frame.wrap =
                            attribute(node, b"wrap").is_none_or(|value| value.as_ref() != b"none");
                        frame.top_aligned =
                            attribute(node, b"anchor").is_none_or(|value| value.as_ref() == b"t");
                        let horizontal =
                            attribute(node, b"vert").is_none_or(|value| value.as_ref() == b"horz");
                        let single_column =
                            number(node, b"numCol").is_none_or(|value| value == 1.0);
                        if horizontal && single_column {
                            frame.body = Some(start..end);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(table) = &mut table {
                match (name, stack.as_slice()) {
                    (b"off", [.., Graphic, Transform]) => {
                        table.bounds.x = points(node, b"x").unwrap_or_default();
                        table.bounds.y = points(node, b"y").unwrap_or_default();
                    }
                    (b"ext", [.., Graphic, Transform]) => {
                        table.bounds.width = points(node, b"cx").unwrap_or_default();
                        table.bounds.height = points(node, b"cy").unwrap_or_default();
                    }
                    (b"gridCol", [.., Table, Grid]) => {
                        table.columns.push(points(node, b"w").unwrap_or_default())
                    }
                    (b"tr", [.., Table]) => {
                        table.row_height = points(node, b"h").unwrap_or_default();
                        table.row_height_sum += table.row_height;
                        table.column = 0;
                    }
                    (b"tcPr", [.., Table, Row, Cell]) if let Some(cell) = &mut cell => {
                        cell.horizontal_inset = points(node, b"marL").unwrap_or(7.2)
                            + points(node, b"marR").unwrap_or(7.2);
                        cell.vertical_inset = points(node, b"marT").unwrap_or(3.6)
                            + points(node, b"marB").unwrap_or(3.6);
                        cell.cell = Some(start..end);
                    }
                    _ => {}
                }
            }
            if opened {
                stack.push(kind);
            }
        }
        match event {
            Event::End(_) => match stack.pop() {
                Some(Body) if stack.ends_with(&[Shape, TextBody]) => {
                    if let Some(range) = shape.as_mut().and_then(|frame| frame.body.as_mut()) {
                        range.end = end;
                    }
                }
                Some(Shape) if let Some(mut frame) = shape.take() => {
                    frame.range.end = start;
                    let (has_text, blank_height) = blank_paragraphs(&xml[frame.range.clone()])?;
                    frame.has_text = has_text;
                    frame.blank_height = blank_height;
                    if frame.size.width > 0.0 && frame.size.height > 0.0 {
                        frames.push(frame);
                    }
                }
                Some(Cell)
                    if let Some(mut frame) = cell.take()
                        && let Some(table) = &mut table =>
                {
                    frame.range.end = start;
                    table.cells.push(frame);
                }
                Some(Graphic) if let Some(table) = table.take() => {
                    if table.top_level && table.row_height_sum > 0.0 {
                        obstacles.push(table.bounds);
                    }
                    if !table.merged {
                        frames.extend(table.cells);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    // Top-level non-text rectangles are diagram panels, not prose capacity.
    // Ignore groups: their transforms need a different coordinate system.
    obstacles.extend(
        frames
            .iter()
            .filter(|f| f.rect && !f.has_text)
            .map(|f| f.size.clone()),
    );
    for frame in &mut frames {
        if frame.rect && frame.top_aligned {
            let bounds = &mut frame.size;
            // A table inside a prose box reserves the lower part of the original layout.
            for table in &obstacles {
                let before = table.y - bounds.y;
                if before >= 36.0
                    && before < bounds.height
                    && table.x >= bounds.x
                    && table.x + table.width <= bounds.x + bounds.width
                {
                    bounds.height = before - 2.0;
                }
            }
        }
    }
    frames.sort_unstable_by_key(|frame| frame.range.start);
    Ok(frames)
}

// Empty paragraphs are often deliberate spacers over separately drawn diagrams.
// They are not translation units, but still occupy height in PowerPoint.
fn blank_paragraphs(xml: &str) -> Result<(bool, f64), Error> {
    let mut reader = Reader::from_str(xml);
    let mut start = None;
    let mut visible = false;
    let mut any_text = false;
    let mut font = 0.0_f64;
    let mut height = 0.0;
    loop {
        let at = reader.buffer_position() as usize;
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(tag) | Event::Empty(tag) => match tag.local_name().as_ref() {
                b"p" => {
                    start = Some(at);
                    visible = false;
                    font = 0.0;
                }
                b"t" | b"fld" if start.is_some() => {
                    visible = true;
                    any_text = true;
                }
                b"endParaRPr" if start.is_some() => {
                    font = number(&tag, b"sz").unwrap_or(0.0) / 100.0;
                }
                _ => {}
            },
            Event::End(tag) if tag.local_name().as_ref() == b"p" => {
                if let Some(start) = start.take()
                    && !visible
                    && font > 0.0
                {
                    let spacing = Spacing::read(&xml[start..at]);
                    height += spacing.line_points.unwrap_or(font * spacing.line_ratio)
                        + spacing.extra_points;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((any_text, height))
}

#[cfg(test)]
mod inheritance_tests {
    use super::*;

    #[test]
    fn empty_paragraph_spacing_is_reserved_and_internal_panels_limit_prose() {
        let xml = r#"<root><sp><spPr><xfrm><off x="0" y="0"/><ext cx="5080000" cy="5080000"/></xfrm><prstGeom prst="rect"/></spPr><txBody><bodyPr/><p><r><t>Prose</t></r></p><p><pPr><spcAft><spcPts val="600"/></spcAft></pPr><endParaRPr sz="1000"/></p></txBody></sp><sp><spPr><xfrm><off x="127000" y="1905000"/><ext cx="2540000" cy="1270000"/></xfrm><prstGeom prst="rect"/></spPr></sp></root>"#;
        let frames = frames(xml).unwrap();
        assert_eq!(frames[0].blank_height, 18.0);
        // The panel begins above the midpoint: the old half-height rule missed it.
        assert_eq!(frames[0].size.height, 148.0);
        assert_eq!(frames[1].size.height, 100.0);
    }

    #[test]
    fn writes_inherited_size_even_when_run_has_no_properties() {
        let fit = Fit {
            scale: 0.8,
            overflow: false,
        };
        let text = r#"<a:r><a:t>English words</a:t></a:r><a:r><a:rPr lang="en-US"/><a:t> more</a:t></a:r>"#;
        let out = fit
            .compact_paragraph_with_default(text.into(), Some(13.0))
            .unwrap();
        assert!(out.contains(r#"<a:rPr sz="1040"/>"#));
        assert!(out.contains(r#"<a:rPr lang="en-US" sz="1040"/>"#));
        assert!(!out.contains("800"));
    }
}
