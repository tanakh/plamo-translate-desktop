//! Resolve table text defaults through the package's own slide/master links.
use super::{Error, MAX_XML, Paragraph};
use quick_xml::{events::Event, reader::Reader};
use std::{
    collections::HashMap,
    io::{Read, Seek},
};
use zip::ZipArchive;

fn xml<R: Read + Seek>(archive: &mut ZipArchive<R>, name: &str) -> Result<Option<String>, Error> {
    let Some(index) = archive.index_for_name(name) else {
        return Ok(None);
    };
    let file = archive.by_index(index)?;
    if file.size() > MAX_XML {
        return Err(Error::Package);
    }
    let mut text = String::new();
    file.take(MAX_XML + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_XML {
        return Err(Error::Package);
    }
    Ok(Some(text))
}

fn related<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    part: &str,
    kind: &str,
) -> Result<Option<String>, Error> {
    let Some((dir, file)) = part.rsplit_once('/') else {
        return Ok(None);
    };
    let Some(text) = xml(archive, &format!("{dir}/_rels/{file}.rels"))? else {
        return Ok(None);
    };
    let mut reader = Reader::from_str(&text);
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Empty(tag) | Event::Start(tag)
                if tag.local_name().as_ref() == b"Relationship" =>
            {
                let attrs = tag
                    .attributes()
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| Error::Xml)?;
                let value = |key: &[u8]| {
                    attrs
                        .iter()
                        .find(|a| a.key.as_ref() == key)
                        .map(|a| a.value.as_ref())
                };
                if value(b"TargetMode") == Some(b"External")
                    || !value(b"Type").is_some_and(|t| t.ends_with(format!("/{kind}").as_bytes()))
                {
                    continue;
                }
                let Some(target) = value(b"Target") else {
                    continue;
                };
                let target = std::str::from_utf8(target).map_err(|_| Error::Xml)?;
                let mut components: Vec<&str> = if target.starts_with('/') {
                    Vec::new()
                } else {
                    dir.split('/').collect()
                };
                for c in target.split('/') {
                    match c {
                        "" | "." => {}
                        ".." => {
                            if components.pop().is_none() {
                                return Err(Error::Package);
                            }
                        }
                        _ => components.push(c),
                    }
                }
                return Ok(Some(components.join("/")));
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

fn levels(text: &str, container: &[u8]) -> Result<[Option<f64>; 9], Error> {
    let mut result = [None; 9];
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| Error::Xml)?;
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = tag.local_name();
                if name.as_ref() == b"defRPr"
                    && stack.iter().any(|n| n == container)
                    && let Some(level) = stack.last().and_then(|n| {
                        (1..=9).find(|i| n.as_slice() == format!("lvl{i}pPr").as_bytes())
                    })
                {
                    let raw = format!("<{} />", std::str::from_utf8(tag).map_err(|_| Error::Xml)?);
                    result[level - 1] = super::layout::font_points(&raw);
                }
                if matches!(event, Event::Start(_)) {
                    stack.push(name.as_ref().to_vec());
                }
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result)
}

// Parse shared defaults once per document. Large decks reuse masters/layouts.
#[derive(Default)]
pub(super) struct Defaults {
    presentation: Option<[Option<f64>; 9]>,
    masters: HashMap<String, [Option<f64>; 9]>,
    layouts: HashMap<String, Option<String>>,
}

pub(super) fn resolve_tables<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    part: &str,
    source: &str,
    frames: &[super::layout::Frame],
    paragraphs: &mut [Paragraph],
    cache: &mut Defaults,
    main: &str,
) -> Result<(), Error> {
    let mut defaults = [None; 9];
    if let Some(layout) = related(archive, part, "slideLayout")? {
        if !cache.layouts.contains_key(&layout) {
            cache
                .layouts
                .insert(layout.clone(), related(archive, &layout, "slideMaster")?);
        }
        if let Some(master) = cache.layouts.get(&layout).cloned().flatten() {
            if !cache.masters.contains_key(&master) {
                let parsed = xml(archive, &master)?
                    .map(|text| levels(&text, b"otherStyle"))
                    .transpose()?
                    .unwrap_or([None; 9]);
                cache.masters.insert(master.clone(), parsed);
            }
            defaults = cache.masters[&master];
        }
    }
    if cache.presentation.is_none() {
        cache.presentation = Some(
            xml(archive, main)?
                .map(|text| levels(&text, b"defaultTextStyle"))
                .transpose()?
                .unwrap_or([None; 9]),
        );
    }
    for (size, fallback) in defaults.iter_mut().zip(cache.presentation.unwrap()) {
        *size = size.or(fallback);
    }
    for frame in frames.iter().filter(|frame| frame.is_cell()) {
        let local = levels(&source[frame.range.clone()], b"lstStyle")?;
        for paragraph in paragraphs
            .iter_mut()
            .filter(|p| frame.contains(p) && p.font_size.is_none())
        {
            let mut level = 0;
            let mut reader = Reader::from_str(&paragraph.before);
            while let Ok(event) = reader.read_event() {
                match event {
                    Event::Start(tag) | Event::Empty(tag)
                        if tag.local_name().as_ref() == b"pPr" =>
                    {
                        level = tag
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"lvl")
                            .and_then(|a| std::str::from_utf8(&a.value).ok()?.parse::<usize>().ok())
                            .unwrap_or(0);
                    }
                    Event::DocType(_) => return Err(Error::Xml),
                    Event::Eof => break,
                    _ => {}
                }
            }
            if let Some(size) = local
                .get(level)
                .copied()
                .flatten()
                .or_else(|| defaults.get(level).copied().flatten())
            {
                paragraph.inherited_font_size = Some(size);
                paragraph.font_size = Some(paragraph.font_size.unwrap_or(size).max(size));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn follows_the_slides_master_and_keeps_explicit_sizes() {
        let slide = r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><graphicFrame><a:tbl><a:tblGrid><a:gridCol w="1270000"/></a:tblGrid><a:tr h="254000"><a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr/><a:t>短文</a:t></a:r></a:p><a:p><a:r><a:rPr sz="900"/><a:t>別の文字</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc></a:tr></a:tbl></graphicFrame></root>"#;
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            ("[Content_Types].xml", "<Types/>"),
            ("ppt/presentation.xml", "<presentation/>"),
            ("ppt/slides/slide1.xml", slide),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                r#"<Relationships><Relationship Type="x/slideLayout" Target="../slideLayouts/slideLayout2.xml"/></Relationships>"#,
            ),
            (
                "ppt/slideLayouts/_rels/slideLayout2.xml.rels",
                r#"<Relationships><Relationship Type="x/slideMaster" Target="../slideMasters/slideMaster2.xml"/></Relationships>"#,
            ),
            (
                "ppt/slideMasters/slideMaster2.xml",
                r#"<master><otherStyle><lvl1pPr><defRPr sz="1300"/></lvl1pPr></otherStyle></master>"#,
            ),
        ] {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let deck = super::super::OfficeDocument::open_mode(bytes.clone(), true).unwrap();
        let paragraphs = &deck.parts()[0].paragraphs;
        assert_eq!(paragraphs[0].font_size, Some(13.0));
        assert_eq!(paragraphs[1].font_size, Some(9.0));
        assert_eq!(paragraphs[1].inherited_font_size, None);
        assert_eq!(deck.write("en", &[vec![None, None]]).unwrap(), bytes);
    }

    #[test]
    fn list_levels_do_not_leak_between_styles() {
        let text = r#"<root><otherStyle><lvl1pPr><defRPr sz="1300"/></lvl1pPr><lvl2pPr><defRPr sz="1100"/></lvl2pPr></otherStyle><titleStyle><lvl1pPr><defRPr sz="4000"/></lvl1pPr></titleStyle></root>"#;
        let sizes = levels(text, b"otherStyle").unwrap();
        assert_eq!(sizes[0], Some(13.0));
        assert_eq!(sizes[1], Some(11.0));
        assert_eq!(sizes[2], None);
    }
}
