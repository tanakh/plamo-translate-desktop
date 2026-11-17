//! Resolve the package's main and editable parts through internal relationships.
use super::*;

pub(super) struct Parts {
    pub main: String,
    pub editable: HashSet<String>,
}

fn relationships(
    archive: &mut ZipArchive<Cursor<&Vec<u8>>>,
    part: &str,
) -> Result<Vec<(String, String)>, Error> {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let rels = if part.is_empty() {
        "_rels/.rels".to_owned()
    } else if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    let Some(index) = archive.index_for_name(&rels) else {
        return Ok(Vec::new());
    };
    let entry = archive.by_index(index)?;
    if entry.size() > MAX_XML {
        return Err(Error::Package);
    }
    let mut text = String::new();
    entry.take(MAX_XML + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_XML {
        return Err(Error::Package);
    }
    let mut reader = quick_xml::Reader::from_str(&text);
    let mut result = Vec::new();
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(tag) | Event::Empty(tag)
                if tag.local_name().as_ref() == b"Relationship" =>
            {
                let mut kind = None;
                let mut target = None;
                let mut external = false;
                for attr in tag.attributes() {
                    let attr = attr.map_err(|_| Error::Xml)?;
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|_| Error::Xml)?
                        .into_owned();
                    match attr.key.as_ref() {
                        b"Type" => kind = Some(value),
                        b"Target" => target = Some(value),
                        b"TargetMode" => external = value == "External",
                        _ => {}
                    }
                }
                if external {
                    continue;
                }
                let (Some(kind), Some(target)) = (kind, target) else {
                    return Err(Error::Xml);
                };
                let kind = kind.rsplit('/').next().unwrap_or(&kind);
                if !matches!(
                    kind,
                    "officeDocument"
                        | "header"
                        | "footer"
                        | "footnotes"
                        | "endnotes"
                        | "slide"
                        | "notesSlide"
                ) {
                    continue;
                }
                // Relationship targets are package-relative URIs. Unsupported URI
                // spellings are rejected rather than translated as the wrong part.
                if target.contains(['\\', '%', '#', '?', ':']) {
                    return Err(Error::Package);
                }
                let mut path: Vec<_> = if target.starts_with('/') || dir.is_empty() {
                    Vec::new()
                } else {
                    dir.split('/').collect()
                };
                for segment in target.split('/') {
                    match segment {
                        "" | "." => {}
                        ".." => {
                            path.pop().ok_or(Error::Package)?;
                        }
                        _ => path.push(segment),
                    }
                }
                result.push((kind.to_owned(), path.join("/")));
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => return Ok(result),
            _ => {}
        }
    }
}

pub(super) fn parts(
    archive: &mut ZipArchive<Cursor<&Vec<u8>>>,
    format: Format,
) -> Result<Parts, Error> {
    let root = relationships(archive, "")?;
    let main = root
        .iter()
        .find(|(kind, _)| kind == "officeDocument")
        .map(|(_, path)| path.clone())
        .or_else(|| {
            (archive.index_for_name("_rels/.rels").is_none()).then(|| match format {
                Format::Word => "word/document.xml".to_owned(),
                Format::PowerPoint => "ppt/presentation.xml".to_owned(),
            })
        })
        .ok_or(Error::Package)?;
    if archive.index_for_name(&main).is_none() {
        return Err(Error::Package);
    }
    let mut editable = HashSet::new();
    if format == Format::Word {
        editable.insert(main.clone());
    }
    let related = relationships(archive, &main)?;
    for (kind, name) in related {
        let edit = match format {
            Format::Word => matches!(
                kind.as_str(),
                "header" | "footer" | "footnotes" | "endnotes"
            ),
            Format::PowerPoint => kind == "slide",
        };
        if !edit {
            continue;
        }
        if archive.index_for_name(&name).is_none() {
            return Err(Error::Package);
        }
        if format == Format::PowerPoint {
            for (kind, notes) in relationships(archive, &name)? {
                if kind == "notesSlide" {
                    if archive.index_for_name(&notes).is_none() {
                        return Err(Error::Package);
                    }
                    editable.insert(notes);
                }
            }
        }
        editable.insert(name);
    }
    // Also support minimal packages used by older exporters without relationship
    // parts. When relationships exist, they are authoritative, not filename guesses.
    let (dir, file) = main.rsplit_once('/').unwrap_or(("", &main));
    let rels = if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    if archive.index_for_name(&rels).is_none() {
        for name in archive.file_names() {
            let name = name?;
            if match format {
                Format::Word => word_body_part(&name),
                Format::PowerPoint => slide_or_notes(&name),
            } {
                editable.insert(name.into_owned());
            }
        }
    }
    Ok(Parts { main, editable })
}
