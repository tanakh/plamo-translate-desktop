//! Local OOXML paragraph translation for PowerPoint and Word.
pub(crate) mod amounts;
mod inheritance;
mod layout;
mod package;
mod paragraph;
use paragraph::balanced_word_content;

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::ops::Range;

use quick_xml::escape::escape;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use thiserror::Error;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

// Resource bounds apply before decompression; per-part reads are bounded too.
// OOXML is a ZIP container, so total expanded size and entry count both matter.
const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const MAX_TOTAL_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
const MAX_XML: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const DRAWING: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const WORD: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const STRICT_WORD: &[u8] = b"http://purl.oclc.org/ooxml/wordprocessingml/main";
const STRICT_DRAWING: &[u8] = b"http://purl.oclc.org/ooxml/drawingml/main";

#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported or oversized Office package")]
    Package,
    #[error("unsupported or malformed Office XML")]
    Xml,
    #[error("Office translations do not match the extracted paragraphs or tags")]
    Translation,
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Tagged runs cannot be split safely. Oversized units are retained and reported.
pub const MAX_UNIT_TOKENS: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    PowerPoint,
    Word,
}

/// A bounded OOXML package. Export patches paragraph bodies and raw-copies other ZIP entries.
pub struct OfficeDocument {
    format: Format,
    for_translation: bool,
    bytes: Vec<u8>,
    parts: Vec<Part>,
    pub unsupported: Vec<RetainedContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Why source content was kept instead of being offered to the translation model.
pub enum RetentionReason {
    UnsupportedPart,
    ReadingGuide,
    Range,
    ProtectedStructure,
    InlineStructure,
    TableOfContents,
}

#[derive(Clone, Debug)]
/// A one-based source location that callers can present for manual review.
pub struct RetainedContent {
    pub part: String,
    pub paragraph: Option<usize>,
    pub in_table: bool,
    pub count: usize,
    pub source: String,
    pub reason: RetentionReason,
}

impl std::fmt::Display for RetainedContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.part)?;
        if let Some(paragraph) = self.paragraph {
            write!(
                f,
                ": paragraph {paragraph}{}",
                if self.in_table { " (table)" } else { "" }
            )?;
        }
        write!(
            f,
            ": retained {:?} ({}): {}",
            self.reason, self.count, self.source
        )
    }
}

/// An editable XML part and its paragraphs in source order.
pub struct Part {
    format: Format,
    pub name: String,
    archive_index: usize,
    xml: String,
    pub paragraphs: Vec<Paragraph>,
    frames: Vec<layout::Frame>,
}

/// One original paragraph, with run styles and non-text elements protected by anchors.
/// Paragraph boundaries are never inferred from punctuation or character counts.
pub struct Paragraph {
    body: Range<usize>,
    font_size: Option<f64>,
    inherited_font_size: Option<f64>,
    ordered_tags: bool,
    word_location: Option<(usize, bool)>,
    before: String,
    after: String,
    base: Run,
    tags: Vec<Tag>,
    embedded_atoms: Vec<EmbeddedAtom>,
    items: Vec<Item>,
}

struct EmbeddedAtom {
    tag: usize,
    source: Range<usize>,
    offset: usize,
}

#[derive(Clone)]
struct Run {
    write_before: String,
    write_after: String,
    key: String,
    props: Option<Range<usize>>,
    hyperlink: bool,
    word: bool,
}

enum Child {
    Run(Run, String),
    Atom(String),
    Embedded(String, Range<usize>, usize),
}

enum Tag {
    Style(Run),
    Atom(String),
}

enum Item {
    Text(String, Option<usize>),
    Atom(usize),
}

impl OfficeDocument {
    /// Read a package without translation-only filtering or layout fitting.
    pub fn read(path: &std::path::Path) -> Result<Self, Error> {
        Self::read_mode(path, false, Self::format_for_path(path))
    }

    /// Extract supported prose and retain unsupported structures with review locations.
    pub fn read_for_translation(path: &std::path::Path) -> Result<Self, Error> {
        Self::read_mode(path, true, Self::format_for_path(path))
    }

    fn format_for_path(path: &std::path::Path) -> Format {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("docx"))
        {
            Format::Word
        } else {
            Format::PowerPoint
        }
    }

    pub fn extension(&self) -> &'static str {
        match self.format {
            Format::PowerPoint => "pptx",
            Format::Word => "docx",
        }
    }

    fn read_mode(
        path: &std::path::Path,
        for_translation: bool,
        format: Format,
    ) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_ARCHIVE + 1)
            .read_to_end(&mut bytes)?;
        Self::open_format(bytes, for_translation, format)
    }

    /// Open a PowerPoint package from bytes without translation-only filtering.
    pub fn open(bytes: Vec<u8>) -> Result<Self, Error> {
        Self::open_mode(bytes, false)
    }

    fn open_mode(bytes: Vec<u8>, for_translation: bool) -> Result<Self, Error> {
        Self::open_format(bytes, for_translation, Format::PowerPoint)
    }

    fn open_format(bytes: Vec<u8>, for_translation: bool, format: Format) -> Result<Self, Error> {
        if bytes.len() as u64 > MAX_ARCHIVE {
            return Err(Error::Package);
        }
        let mut archive = ZipArchive::new(Cursor::new(&bytes))?;
        if archive.len() > MAX_ENTRIES || archive.index_for_name("[Content_Types].xml").is_none() {
            return Err(Error::Package);
        }
        // Validate all metadata before opening relationships or editable XML.
        let mut total = 0u64;
        let mut names = HashSet::new();
        for index in 0..archive.len() {
            let raw = archive.by_index_raw(index)?;
            total = total.saturating_add(raw.size());
            if raw.encrypted()
                || raw.is_symlink()
                || raw.enclosed_name().is_none()
                || !names.insert(raw.name()?.into_owned())
                || total > MAX_TOTAL_UNCOMPRESSED
            {
                return Err(Error::Package);
            }
        }
        let package = package::parts(&mut archive, format)?;
        let mut defaults = inheritance::Defaults::default();
        let mut parts = Vec::new();
        let mut unsupported = Vec::new();
        for index in 0..archive.len() {
            let raw = archive.by_index_raw(index)?;
            let name = raw.name()?.into_owned();
            let editable = package.editable.contains(&name);
            let report_untouched = match format {
                Format::PowerPoint => for_translation && unsupported_text_part(&name),
                Format::Word => name == "word/comments.xml",
            };
            if !editable && !report_untouched {
                continue;
            }
            if raw.size() > MAX_XML {
                return Err(Error::Package);
            }
            drop(raw);
            let mut xml = String::new();
            archive
                .by_index(index)?
                .take(MAX_XML + 1)
                .read_to_string(&mut xml)?;
            if xml.len() as u64 > MAX_XML {
                return Err(Error::Package);
            }
            if !editable {
                if has_untranslated_text(&xml)? {
                    unsupported.push(RetainedContent {
                        part: name.clone(),
                        paragraph: None,
                        in_table: false,
                        count: 1,
                        source: String::new(),
                        reason: RetentionReason::UnsupportedPart,
                    });
                }
                continue;
            }
            let (mut paragraphs, skipped) = parse_paragraph_details(&xml, for_translation, format)?;
            let frames = if for_translation && format == Format::PowerPoint {
                layout::frames(&xml)?
            } else {
                Vec::new()
            };
            if for_translation && format == Format::PowerPoint {
                inheritance::resolve_tables(
                    &mut archive,
                    &name,
                    &xml,
                    &frames,
                    &mut paragraphs,
                    &mut defaults,
                    &package.main,
                )?;
            }
            let mut furigana = 0;
            paragraphs.retain(|p| {
                let index = frames.partition_point(|frame| frame.range.end <= p.body.start);
                let retain = frames
                    .get(index)
                    .is_some_and(|frame| frame.contains(p) && frame.likely_furigana(p));
                furigana += usize::from(retain);
                !retain
            });
            if furigana > 0 {
                unsupported.push(RetainedContent {
                    part: name.clone(),
                    paragraph: None,
                    in_table: false,
                    count: furigana,
                    source: String::new(),
                    reason: RetentionReason::ReadingGuide,
                });
            }
            unsupported.extend(
                skipped
                    .iter()
                    .filter(|item| {
                        item.text.chars().any(char::is_alphabetic)
                            || item.reason == RetentionReason::TableOfContents
                    })
                    .map(|item| RetainedContent {
                        part: name.clone(),
                        paragraph: Some(item.index),
                        in_table: item.in_table,
                        count: 1,
                        source: item.text.clone(),
                        reason: item.reason,
                    }),
            );
            parts.push(Part {
                format,
                name,
                archive_index: index,
                xml,
                paragraphs,
                frames,
            });
        }
        if parts.is_empty() {
            return Err(Error::Package);
        }
        Ok(Self {
            format,
            for_translation,
            bytes,
            parts,
            unsupported,
        })
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// The original package is returned byte-for-byte when no paragraph changes.
    pub fn write(
        &self,
        target_language: &str,
        translations: &[Vec<Option<String>>],
    ) -> Result<Vec<u8>, Error> {
        if translations.len() != self.parts.len() {
            return Err(Error::Translation);
        }
        let mut changed = std::collections::HashMap::new();
        for (part, targets) in self.parts.iter().zip(translations) {
            if targets.len() != part.paragraphs.len() {
                return Err(Error::Translation);
            }
            let mut edits = Vec::new();
            let mut fitted = vec![None; part.paragraphs.len()];
            for (_, indices, fit) in part.fits(targets) {
                if fit.applies() {
                    fitted[indices].fill(Some(fit));
                }
            }
            // Descendants must be rendered first, then inserted into the parent's
            // opaque drawing atom. Applying overlapping XML edits loses either
            // the inner translation or the surrounding paragraph.
            for ((p, target), fit) in part.paragraphs.iter().zip(targets).zip(fitted).rev() {
                if let Some(target) = target {
                    if self.for_translation && !p.amounts_match(target, target_language) {
                        return Err(Error::Translation);
                    }
                    let mut rendered = p.rebuild_with_edits(target, target_language, &edits)?;
                    if let Some(fit) = fit {
                        rendered =
                            fit.compact_paragraph_with_default(rendered, p.inherited_font_size)?;
                    }
                    edits.retain(|(range, _)| {
                        !(p.body.start <= range.start && range.end <= p.body.end)
                    });
                    edits.push((p.body.clone(), rendered));
                }
            }
            if !edits.is_empty() {
                let mut xml = part.xml.clone();
                edits.sort_unstable_by_key(|(range, _)| range.start);
                for (range, replacement) in edits.into_iter().rev() {
                    xml.replace_range(range, &replacement);
                }
                // Reject malformed XML before copying it into the package.
                if self.format == Format::PowerPoint {
                    paragraphs(&xml)?;
                } else {
                    parse_office_paragraphs(&xml, false, self.format)?;
                }
                changed.insert(part.archive_index, xml);
            }
        }
        if changed.is_empty() {
            return Ok(self.bytes.clone());
        }
        let mut archive = ZipArchive::new(Cursor::new(&self.bytes))?;
        let mut output = ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index)?;
            if let Some(xml) = changed.remove(&index) {
                output.start_file(
                    entry.name()?,
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
                )?;
                output.write_all(xml.as_bytes())?;
            } else {
                output.raw_copy_file(entry)?;
            }
        }
        Ok(output.finish()?.into_inner())
    }
}

impl Part {
    pub fn source_context(&self) -> String {
        if !self.name.starts_with("ppt/") {
            return String::new();
        }
        let mut unique = HashSet::new();
        self.paragraphs
            .iter()
            .map(Paragraph::plain_text)
            .filter(|text| unique.insert(text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fits<'a>(
        &'a self,
        results: &'a [Option<String>],
    ) -> impl Iterator<Item = (&'a layout::Frame, Range<usize>, layout::Fit)> {
        self.frames.iter().filter_map(move |frame| {
            let start = self
                .paragraphs
                .partition_point(|p| p.body.start < frame.range.start);
            let end = self
                .paragraphs
                .partition_point(|p| p.body.start < frame.range.end);
            let paragraphs = self.paragraphs[start..end]
                .iter()
                .zip(results.iter().skip(start).map(Option::as_deref));
            frame.fit(paragraphs).map(|fit| (frame, start..end, fit))
        })
    }

    /// Locations requiring a native Word layout check. No geometry is rewritten.
    /// Count outer paragraphs, matching word_location(), not textbox fallback copies.
    pub fn word_layout_risks(&self) -> Vec<usize> {
        if self.format != Format::Word {
            return Vec::new();
        }
        let mut reader = NsReader::from_str(&self.xml);
        let mut paragraphs = 0;
        let mut depth: usize = 0;
        let mut risks = Vec::new();
        while let Ok((ns, event)) = reader.read_resolved_event() {
            let word = matches!(&ns, ResolveResult::Bound(n) if n.as_ref() == WORD || n.as_ref() == STRICT_WORD);
            match event {
                Event::Start(ref tag) | Event::Empty(ref tag) => {
                    let name = tag.local_name();
                    if word && name.as_ref() == b"p" {
                        if depth == 0 {
                            paragraphs += 1;
                        }
                        if matches!(event, Event::Start(_)) {
                            depth += 1;
                        }
                    }
                    let floating = name.as_ref() == b"anchor"
                        && matches!(&ns, ResolveResult::Bound(n) if n.as_ref().ends_with(b"/wordprocessingDrawing"));
                    let vml = name.as_ref() == b"shape"
                        && matches!(&ns, ResolveResult::Bound(n) if n.as_ref() == b"urn:schemas-microsoft-com:vml")
                        && tag.attributes().flatten().any(|a| {
                            a.key.as_ref() == b"style"
                                && a.value.windows(17).any(|w| w == b"position:absolute")
                        });
                    if depth > 0 && (floating || vml) {
                        risks.push(paragraphs);
                    }
                    if word && name.as_ref() == b"tblpPr" {
                        risks.push(paragraphs + 1);
                    }
                    if word
                        && name.as_ref() == b"trHeight"
                        && tag.attributes().flatten().any(|a| {
                            a.key.local_name().as_ref() == b"hRule" && a.value.as_ref() == b"exact"
                        })
                    {
                        risks.push(paragraphs + 1);
                    }
                }
                Event::End(tag) if word && tag.local_name().as_ref() == b"p" => {
                    depth = depth.saturating_sub(1);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        risks.sort_unstable();
        risks.dedup();
        risks
    }

    pub fn overflow_frames(&self, results: &[Option<String>]) -> Vec<usize> {
        self.fits(results)
            .filter_map(|(frame, _, fit)| fit.overflow.then_some(frame.index))
            .collect()
    }
}

fn unsupported_text_part(name: &str) -> bool {
    name.ends_with(".xml")
        && [
            "ppt/charts/",
            "ppt/slideLayouts/",
            "ppt/slideMasters/",
            "ppt/notesMasters/",
            "ppt/diagrams/",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn has_untranslated_text(xml: &str) -> Result<bool, Error> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut in_text = false;
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(node) => in_text = matches!(node.local_name().as_ref(), b"t" | b"v"),
            Event::End(_) => in_text = false,
            Event::Text(text) if in_text => {
                if text
                    .decode()
                    .map_err(|_| Error::Xml)?
                    .chars()
                    .any(char::is_alphabetic)
                {
                    return Ok(true);
                }
            }
            Event::CData(text) if in_text => {
                if text
                    .decode()
                    .map_err(|_| Error::Xml)?
                    .chars()
                    .any(char::is_alphabetic)
                {
                    return Ok(true);
                }
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => return Ok(false),
            _ => {}
        }
    }
}

fn slide_or_notes(name: &str) -> bool {
    ["ppt/slides/slide", "ppt/notesSlides/notesSlide"]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|tail| tail.strip_suffix(".xml"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

fn word_body_part(name: &str) -> bool {
    matches!(
        name,
        "word/document.xml" | "word/footnotes.xml" | "word/endnotes.xml"
    ) || ["word/header", "word/footer"].into_iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|stem| stem.strip_suffix(".xml"))
            .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Paragraph,
    Properties,
    EndProperties,
    Run,
    RunProperties,
    DefaultRunProperties,
    Text,
    Break,
    Field,
    Link,
    WordLink,
    WordRange,
    WordRangeStart,
    WordRangeEnd,
    WordPrefix,
    WordSuffix,
    WordFieldChar,
    WordFieldText,
    WordDrawing,
    WordTextBox,
    WordFallback,
    WordControl,
    WordControlContent,
    WordProtected,
    Proofing,
    Bullet,
    SpacingAfter,
    LineSpacing,
    Other,
}

#[derive(Clone)]
struct Node {
    kind: NodeKind,
    full: Range<usize>,
    body: Range<usize>,
    children: Vec<Node>,
    text: String,
    in_word_table: bool,
    word_paragraph: bool,
}

fn kind(namespace: ResolveResult<'_>, name: &[u8], format: Format) -> NodeKind {
    if format == Format::Word
        && name == b"Fallback"
        && matches!(&namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
    {
        return NodeKind::WordFallback;
    }
    if format == Format::Word
        && name == b"AlternateContent"
        && matches!(&namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
    {
        return NodeKind::WordDrawing;
    }
    if format == Format::Word
        && name == b"t"
        && matches!(&namespace, ResolveResult::Bound(ns) if [DRAWING, STRICT_DRAWING].contains(&ns.as_ref()))
    {
        return NodeKind::Text;
    }
    let namespaces = match format {
        Format::PowerPoint => [DRAWING, STRICT_DRAWING],
        Format::Word => [WORD, STRICT_WORD],
    };
    if !matches!(namespace, ResolveResult::Bound(ns) if namespaces.contains(&ns.as_ref())) {
        return NodeKind::Other;
    }
    match name {
        b"drawing" | b"pict" if format == Format::Word => NodeKind::WordDrawing,
        b"txbxContent" if format == Format::Word => NodeKind::WordTextBox,
        b"sdt" if format == Format::Word => NodeKind::WordControl,
        b"sdtContent" if format == Format::Word => NodeKind::WordControlContent,
        b"ins" | b"del" | b"moveFrom" | b"moveTo" if format == Format::Word => {
            NodeKind::WordProtected
        }
        b"p" => NodeKind::Paragraph,
        b"pPr" => NodeKind::Properties,
        b"endParaRPr" => NodeKind::EndProperties,
        b"r" => NodeKind::Run,
        b"rPr" => NodeKind::RunProperties,
        b"spcAft" => NodeKind::SpacingAfter,
        b"lnSpc" => NodeKind::LineSpacing,
        b"defRPr" => NodeKind::DefaultRunProperties,
        b"t" => NodeKind::Text,
        b"br" => NodeKind::Break,
        b"tab" | b"cr" | b"footnoteReference" | b"endnoteReference" | b"footnoteRef"
        | b"endnoteRef" | b"commentReference"
            if format == Format::Word =>
        {
            NodeKind::Break
        }
        b"hyperlink" if format == Format::Word => NodeKind::WordLink,
        b"fldSimple" if format == Format::Word => NodeKind::Field,
        b"fldChar" if format == Format::Word => NodeKind::WordFieldChar,
        b"instrText" if format == Format::Word => NodeKind::WordFieldText,
        b"bookmarkStart" | b"commentRangeStart" if format == Format::Word => {
            NodeKind::WordRangeStart
        }
        b"bookmarkEnd" | b"commentRangeEnd" if format == Format::Word => NodeKind::WordRangeEnd,
        b"proofErr" | b"lastRenderedPageBreak" if format == Format::Word => NodeKind::Proofing,
        b"fld" => NodeKind::Field,
        b"buChar" | b"buAutoNum" | b"buBlip" => NodeKind::Bullet,
        b"hlinkClick" | b"hlinkMouseOver" => NodeKind::Link,
        _ => NodeKind::Other,
    }
}

fn paragraphs(xml: &str) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_paragraphs(xml, false)
}

fn parse_paragraphs(xml: &str, for_translation: bool) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_office_paragraphs(xml, for_translation, Format::PowerPoint)
}

fn parse_office_paragraphs(
    xml: &str,
    for_translation: bool,
    format: Format,
) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_paragraph_details(xml, for_translation, format)
        .map(|(paragraphs, skipped)| (paragraphs, skipped.len()))
}

struct RetainedParagraph {
    index: usize,
    in_table: bool,
    text: String,
    reason: RetentionReason,
}

fn visible_word_node(node: &Node, output: &mut String) {
    if node.kind == NodeKind::Text {
        output.push_str(&node.text);
    }
    for child in &node.children {
        // Nested text boxes have their own translation/retention records.
        if !child.word_paragraph {
            visible_word_node(child, output);
        }
    }
}

fn retained_paragraph(node: &Node, index: usize, reason: RetentionReason) -> RetainedParagraph {
    fn toc(node: &Node) -> bool {
        (node.kind == NodeKind::WordFieldText && node.text.split_whitespace().next() == Some("TOC"))
            || node.children.iter().any(toc)
    }
    let mut text = String::new();
    visible_word_node(node, &mut text);
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((end, _)) = text.char_indices().nth(72) {
        text.truncate(end);
        text.push('…');
    }
    RetainedParagraph {
        index,
        in_table: node.in_word_table,
        text,
        reason: if toc(node) {
            RetentionReason::TableOfContents
        } else {
            reason
        },
    }
}

// Ordinary unbound text controls are transparent at block and inline level.
// Keep placeholder/typed/bound/locked controls
// intact: Word may regenerate their display from data or a glossary entry.
fn ordinary_word_control(xml: &str) -> bool {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut depth: usize = 0;
    let mut properties = None;
    loop {
        let Ok(event) = reader.read_event() else {
            return false;
        };
        let opened = matches!(&event, Event::Start(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = tag.local_name();
                if depth == 1 && name.as_ref() == b"sdtPr" {
                    properties = Some(depth + 1);
                } else if properties == Some(depth)
                    && !matches!(
                        name.as_ref(),
                        b"rPr" | b"id" | b"alias" | b"tag" | b"placeholder" | b"text" | b"richText"
                    )
                {
                    return false;
                } else if depth == 1 && name.as_ref() == b"sdtContent" {
                    return true;
                }
                if opened {
                    depth += 1;
                }
            }
            Event::End(_) => {
                if properties == Some(depth) {
                    properties = None;
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return false;
                }
            }
            Event::Eof => return false,
            _ => {}
        }
    }
}

type WordIntervals = Vec<(Range<usize>, bool)>;

/// Scan bounded XML into paragraph trees; ranges are resolved separately so a
/// bookmark crossing paragraphs cannot accidentally make its endpoints editable.
fn read_nodes(xml: &str, format: Format) -> Result<(Vec<Node>, WordIntervals), Error> {
    let mut reader = NsReader::from_str(xml);
    let mut stack: Vec<Node> = Vec::new();
    let mut nodes = Vec::new();
    let mut xml_depth = 0usize;
    let mut word_table_depth = 0usize;
    let mut word_locked_at = None;
    let mut word_field_depth = 0usize;
    let mut word_marks: HashMap<_, usize> = HashMap::new();
    let mut word_intervals = Vec::new();
    let mut fallback_depth = 0usize;
    loop {
        let start = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event().map_err(|_| Error::Xml)?;
        let word_element = format == Format::Word
            && matches!(&ns, ResolveResult::Bound(namespace) if [WORD, STRICT_WORD].contains(&namespace.as_ref()));
        let start_kind = match &event {
            Event::Start(e) | Event::Empty(e) => kind(ns, e.local_name().as_ref(), format),
            _ => NodeKind::Other,
        };
        // Keep the original fallback bytes in the drawing atom, but neither
        // translate nor report the duplicate compatibility representation.
        if fallback_depth > 0 || start_kind == NodeKind::WordFallback {
            match &event {
                Event::Start(_) => fallback_depth += 1,
                Event::End(_) => fallback_depth -= 1,
                Event::Eof => return Err(Error::Xml),
                Event::DocType(_) => return Err(Error::Xml),
                _ => {}
            }
            continue;
        }
        if matches!(&event, Event::Start(_)) {
            xml_depth += 1;
            if word_element
                && let Event::Start(e) = &event
                && e.local_name().as_ref() == b"tbl"
            {
                word_table_depth += 1;
            }
        }
        if word_element && let Event::Start(tag) | Event::Empty(tag) = &event {
            let name = tag.local_name();
            if matches!(
                start_kind,
                NodeKind::WordRangeStart | NodeKind::WordRangeEnd
            ) && let Some(id) = tag
                .attributes()
                .flatten()
                .find(|attr| attr.key.local_name().as_ref() == b"id")
            {
                let key = (
                    name.as_ref().starts_with(b"bookmark"),
                    id.value.into_owned(),
                );
                if start_kind == NodeKind::WordRangeStart {
                    if let Some(previous) = word_marks.insert(key, start) {
                        word_intervals.push((previous..start, false));
                    }
                } else if let Some(previous) = word_marks.remove(&key) {
                    word_intervals.push((previous..start, true));
                }
            }
            match name.as_ref() {
                b"fldChar" => {
                    match tag
                        .attributes()
                        .flatten()
                        .find(|attr| attr.key.local_name().as_ref() == b"fldCharType")
                        .map(|attr| attr.value)
                        .as_deref()
                    {
                        Some(b"begin") => word_field_depth += 1,
                        Some(b"end") => word_field_depth = word_field_depth.saturating_sub(1),
                        _ => {}
                    }
                }
                b"ins" | b"del" | b"moveFrom" | b"moveTo" | b"sdt"
                    if matches!(&event, Event::Start(_))
                        && word_locked_at.is_none()
                        && (name.as_ref() != b"sdt"
                            || word_field_depth != 0
                            || !ordinary_word_control(&xml[start..])) =>
                {
                    word_locked_at = Some(xml_depth);
                }
                _ => {}
            }
        }
        if let Event::End(e) = &event {
            if word_element && e.local_name().as_ref() == b"tbl" {
                word_table_depth = word_table_depth.saturating_sub(1);
            }
            if word_locked_at == Some(xml_depth) {
                word_locked_at = None;
            }
            xml_depth = xml_depth.saturating_sub(1);
        }
        let end = reader.buffer_position() as usize;
        match event {
            Event::Start(_) if !stack.is_empty() || start_kind == NodeKind::Paragraph => {
                if stack.len() >= 64 {
                    return Err(Error::Xml);
                }
                stack.push(Node {
                    kind: if start_kind == NodeKind::Paragraph
                        && (word_locked_at.is_some() || word_field_depth > 0)
                    {
                        NodeKind::Other
                    } else {
                        start_kind
                    },
                    full: start..end,
                    body: end..end,
                    children: Vec::new(),
                    text: String::new(),
                    in_word_table: word_table_depth > 0,
                    word_paragraph: format == Format::Word && start_kind == NodeKind::Paragraph,
                });
            }
            Event::Empty(_) if !stack.is_empty() => {
                stack.last_mut().unwrap().children.push(Node {
                    kind: start_kind,
                    full: start..end,
                    body: end..end,
                    children: Vec::new(),
                    text: String::new(),
                    in_word_table: word_table_depth > 0,
                    word_paragraph: false,
                });
            }
            Event::End(_) if !stack.is_empty() => {
                let mut node = stack.pop().unwrap();
                node.full.end = end;
                node.body.end = start;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    nodes.push(node);
                }
            }
            Event::Text(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&event.decode().map_err(|_| Error::Xml)?);
                }
            }
            Event::CData(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&event.decode().map_err(|_| Error::Xml)?);
                }
            }
            Event::GeneralRef(event) if !stack.is_empty() => {
                let entity = event.decode().map_err(|_| Error::Xml)?;
                let escaped = format!("&{entity};");
                let value = quick_xml::escape::unescape(&escaped).map_err(|_| Error::Xml)?;
                stack.last_mut().unwrap().text.push_str(&value);
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof if stack.is_empty() => break,
            Event::Eof => return Err(Error::Xml),
            _ => {}
        }
    }
    word_intervals.extend(
        word_marks
            .into_values()
            .map(|start| (start..xml.len(), false)),
    );
    Ok((nodes, word_intervals))
}

fn parse_paragraph_details(
    xml: &str,
    for_translation: bool,
    format: Format,
) -> Result<(Vec<Paragraph>, Vec<RetainedParagraph>), Error> {
    let (nodes, word_intervals) = read_nodes(xml, format)?;
    // Keep the established outer-paragraph location numbering for result UI and
    // layout warnings. Text-box paragraphs share that location, but are separate
    // units in XML order, including when their enclosing paragraph is retained.
    fn nested_paragraphs(node: &Node, index: usize, out: &mut Vec<(usize, Node)>) {
        for child in &node.children {
            if child.word_paragraph {
                out.push((index, child.clone()));
            }
            nested_paragraphs(child, index, out);
        }
    }
    let mut located = Vec::new();
    for (index, node) in nodes.into_iter().enumerate() {
        located.push((index, node.clone()));
        if format == Format::Word {
            nested_paragraphs(&node, index, &mut located);
        }
    }
    let (locations, mut nodes): (Vec<_>, Vec<_>) = located.into_iter().unzip();

    // Bookmarks outside all paragraph text keep the same membership when that
    // text is replaced. Do not extend this to inline ranges or comment anchors.
    let valid_edges: HashSet<_> = word_intervals
        .iter()
        .filter(|(_, balanced)| *balanced)
        .flat_map(|(span, _)| [span.start, span.end])
        .collect();
    let mut edges = HashMap::new();
    for node in &nodes {
        let bookmark = |n: &Node| {
            valid_edges.contains(&n.full.start)
                && matches!(n.kind, NodeKind::WordRangeStart | NodeKind::WordRangeEnd)
                && xml[n.full.clone()]
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| {
                        name.ends_with("bookmarkStart") || name.ends_with("bookmarkEnd")
                    })
        };
        let whitespace_run = |n: &Node| {
            n.kind == NodeKind::Run
                && n.children.iter().all(|c| match c.kind {
                    NodeKind::RunProperties | NodeKind::Proofing | NodeKind::Break => true,
                    NodeKind::Text => c.text.trim().is_empty(),
                    _ => false,
                })
        };
        let prefix: Vec<_> = node
            .children
            .iter()
            .take_while(|n| {
                bookmark(n)
                    || whitespace_run(n)
                    || matches!(n.kind, NodeKind::Properties | NodeKind::Proofing)
            })
            .collect();
        let suffix: Vec<_> = node
            .children
            .iter()
            .rev()
            .take_while(|n| {
                bookmark(n)
                    || whitespace_run(n)
                    || matches!(n.kind, NodeKind::EndProperties | NodeKind::Proofing)
            })
            .collect();
        for (group, kind) in [
            (prefix, NodeKind::WordPrefix),
            (suffix, NodeKind::WordSuffix),
        ] {
            if group.iter().any(|n| bookmark(n)) {
                for n in group {
                    if bookmark(n) || whitespace_run(n) {
                        edges.entry(n.full.start).or_insert(kind);
                    }
                }
            }
        }
    }
    // A local inline range must be handled as a pair by balanced_word_content.
    // Do not detach just its opening marker from the existing tagged path.
    for (span, balanced) in &word_intervals {
        if *balanced
            && nodes
                .iter()
                .any(|p| p.full.start <= span.start && p.full.end > span.end)
            && (!edges.contains_key(&span.start) || !edges.contains_key(&span.end))
        {
            edges.remove(&span.start);
            edges.remove(&span.end);
        }
    }

    // A complete paragraph strictly inside a balanced range cannot move outside
    // its endpoints when its body is replaced. Keep endpoint paragraphs intact
    // unless their range can be handled locally by balanced_word_content.
    // Malformed/unclosed ranges remain locked in their entirety.
    let mut locks = vec![false; nodes.len()];
    for (span, balanced) in word_intervals {
        for (node, locked) in nodes.iter().zip(&mut locks) {
            if node.full.end <= span.start || node.full.start > span.end {
                continue;
            }
            if !balanced {
                *locked = true;
            } else if node.full.start > span.start || node.full.end <= span.end {
                // Only endpoint paragraphs are ambiguous. Fully enclosed text
                // keeps its range membership, including inside a text box.
                if (node.full.start <= span.start && !edges.contains_key(&span.start))
                    || (node.full.end > span.end && !edges.contains_key(&span.end))
                {
                    *locked = true;
                }
            }
        }
    }
    for node in &mut nodes {
        for child in &mut node.children {
            if let Some(kind) = edges.get(&child.full.start) {
                child.kind = *kind;
            }
        }
    }

    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let nodes = locations.into_iter().zip(nodes.into_iter().zip(locks));
    for (index, (mut node, locked)) in nodes {
        if locked || node.kind != NodeKind::Paragraph {
            skipped.push(retained_paragraph(
                &node,
                index + 1,
                if locked {
                    RetentionReason::Range
                } else {
                    RetentionReason::ProtectedStructure
                },
            ));
            continue;
        }
        if format == Format::Word {
            match balanced_word_content(xml, &node.children) {
                Ok(content) => node.children = content,
                Err(()) => {
                    skipped.push(retained_paragraph(
                        &node,
                        index + 1,
                        RetentionReason::InlineStructure,
                    ));
                    continue;
                }
            }
        }
        let paragraph = Paragraph::from_node(xml, &node, for_translation, format);
        match paragraph {
            Ok(Some(mut p)) => {
                if format == Format::Word {
                    p.word_location = Some((index + 1, node.in_word_table));
                }
                out.push(p);
            }
            Ok(None) => {}
            Err(()) => skipped.push(retained_paragraph(
                &node,
                index + 1,
                RetentionReason::InlineStructure,
            )),
        }
    }
    Ok((out, skipped))
}

struct ParsedTag<'a> {
    start: usize,
    id: usize,
    body: Option<&'a str>,
    body_start: usize,
    end: usize,
}

mod anchors {
    use super::{Error, ParsedTag};
    pub(super) fn wrap(id: usize, text: &str, atom: bool) -> String {
        format!("<a i=\"{id}\">{}</a>", if atom { "◇" } else { text })
    }

    pub(super) fn next(text: &str) -> Result<Option<ParsedTag<'_>>, Error> {
        let prefixes = ["<a i=", "</a>"];
        let Some((start, prefix)) = prefixes
            .iter()
            .filter_map(|&p| text.find(p).map(|i| (i, p)))
            .min()
        else {
            return Ok(None);
        };
        let rest = &text[start + prefix.len()..];
        let (id, body, body_start, end) = match prefix {
            "<a i=" => {
                let (raw_id, content) = rest.split_once('>').ok_or(Error::Translation)?;
                let id = raw_id
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .ok_or(Error::Translation)?;
                let close = "</a>";
                let (body, _) = content.split_once(close).ok_or(Error::Translation)?;
                if prefixes.iter().any(|p| body.contains(p)) {
                    return Err(Error::Translation);
                }
                (
                    id,
                    Some(body),
                    text.len() - content.len(),
                    text.len() - content.len() + body.len() + close.len(),
                )
            }
            _ => return Err(Error::Translation),
        };
        let id = id
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or(Error::Translation)?;
        Ok(Some(ParsedTag {
            start,
            id,
            body,
            body_start,
            end,
        }))
    }
}

#[cfg(test)]
mod tests;
