//! Protect run styles, fields and nested drawings while replacing paragraph text.
use super::*;

fn keep_numbers_together(mut children: Vec<Child>) -> Vec<Child> {
    let mut output = Vec::with_capacity(children.len());
    let mut runs = Vec::new();
    let flush = |runs: &mut Vec<(Run, String)>, output: &mut Vec<Child>| {
        if runs.len() < 2 {
            output.extend(runs.drain(..).map(|(run, text)| Child::Run(run, text)));
            return;
        }
        let mut joined = String::new();
        let mut ends = Vec::with_capacity(runs.len());
        for (_, text) in runs.iter() {
            joined.push_str(text);
            ends.push(joined.len());
        }
        let mut position = 0;
        let mut emit = |to: usize, one_style: bool| {
            while position < to {
                let index = ends.partition_point(|&end| end <= position);
                let end = if one_style { to } else { to.min(ends[index]) };
                let style = if one_style {
                    let digit = joined[position..end]
                        .find(|c: char| c.is_ascii_digit() || ('０'..='９').contains(&c));
                    digit.map_or(index, |offset| {
                        ends.partition_point(|&boundary| boundary <= position + offset)
                    })
                } else {
                    index
                };
                let run = &runs[style].0;
                let fragment = &joined[position..end];
                if let Some(Child::Run(previous, text)) = output.last_mut()
                    && !previous.hyperlink
                    && previous.key == run.key
                {
                    text.push_str(fragment);
                } else {
                    output.push(Child::Run(run.clone(), fragment.to_owned()));
                }
                position = end;
            }
        };
        for range in amounts::expressions(&joined) {
            let index = ends.partition_point(|&end| end <= range.start);
            if range.end > ends[index] {
                emit(range.start, false);
                emit(range.end, true);
            }
        }
        emit(joined.len(), false);
        runs.clear();
    };
    for child in children.drain(..) {
        match child {
            Child::Run(run, text) if !run.hyperlink => runs.push((run, text)),
            other => {
                flush(&mut runs, &mut output);
                output.push(other);
            }
        }
    }
    flush(&mut runs, &mut output);
    output
}

pub(super) fn balanced_word_content(xml: &str, contents: &[Node]) -> Result<Vec<Node>, ()> {
    let mut result = Vec::new();
    let mut i = 0;
    let attribute = |node: &Node, key: &[u8]| -> Option<Vec<u8>> {
        let mut reader = quick_xml::Reader::from_str(&xml[node.full.clone()]);
        let (Event::Start(tag) | Event::Empty(tag)) = reader.read_event().ok()? else {
            return None;
        };
        tag.attributes()
            .flatten()
            .find(|a| a.key.local_name().as_ref() == key)
            .map(|a| a.value.into_owned())
    };
    let field_type = |run: &Node| -> Option<Vec<u8>> {
        if run.kind != NodeKind::Run {
            return None;
        }
        let node = run
            .children
            .iter()
            .find(|n| n.kind == NodeKind::WordFieldChar)?;
        attribute(node, b"fldCharType")
    };
    while i < contents.len() {
        let node = &contents[i];
        if node.kind == NodeKind::WordRangeStart {
            let end = if contents
                .get(i + 1)
                .is_some_and(|n| n.kind == NodeKind::WordRangeEnd)
            {
                i + 1
            } else {
                i + 2
            };
            let close = contents
                .get(end)
                .filter(|n| n.kind == NodeKind::WordRangeEnd)
                .ok_or(())?;
            let bookmark = |marker: &Node| {
                xml[marker.full.clone()]
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| {
                        name.ends_with("bookmarkStart") || name.ends_with("bookmarkEnd")
                    })
            };
            if bookmark(node) != bookmark(close)
                || attribute(node, b"id")
                    .zip(attribute(close, b"id"))
                    .is_none_or(|(a, b)| a != b)
                || (end == i + 2 && contents[i + 1].kind != NodeKind::Run)
            {
                return Err(());
            }
            let mut wrapper = node.clone();
            wrapper.full.end = close.full.end;
            wrapper.kind = if end == i + 1 {
                NodeKind::Field
            } else {
                wrapper.children.push(contents[i + 1].clone());
                NodeKind::WordRange
            };
            result.push(wrapper);
            i = end + 1;
        } else if field_type(node).as_deref() == Some(b"begin") {
            let mut depth = 1usize;
            let mut end = i + 1;
            while depth != 0 {
                let run = contents
                    .get(end)
                    .filter(|n| n.kind == NodeKind::Run)
                    .ok_or(())?;
                if run.children.iter().any(|n| {
                    !matches!(
                        n.kind,
                        NodeKind::WordFieldChar
                            | NodeKind::WordFieldText
                            | NodeKind::RunProperties
                            | NodeKind::Text
                    )
                }) {
                    return Err(());
                }
                match field_type(run).as_deref() {
                    Some(b"begin") => depth += 1,
                    Some(b"end") => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            let mut wrapper = node.clone();
            wrapper.full.end = contents[end - 1].full.end;
            wrapper.kind = NodeKind::Field;
            result.push(wrapper);
            i = end;
        } else {
            result.push(node.clone());
            i += 1;
        }
    }
    Ok(result)
}

fn is_japanese(ch: char) -> bool {
    matches!(ch, 'ぁ'..='ゖ' | 'ァ'..='ヺ' | '㐀'..='䶿' | '一'..='鿿')
}

// Text-box paragraphs are extracted independently. Other drawing text (e.g.
// DrawingML chart labels) still prevents a silent claim of full translation.
fn supported_word_drawing(node: &Node) -> bool {
    if node.kind == NodeKind::WordTextBox {
        return true;
    }
    !matches!(node.kind, NodeKind::Text | NodeKind::Paragraph)
        && (node.text.trim().is_empty() || node.text.trim().parse::<f64>().is_ok())
        && node.children.iter().all(supported_word_drawing)
}

fn split_word_run(xml: &str, run: &Node, result: &mut Vec<Child>) -> Result<(), ()> {
    let props = run
        .children
        .first()
        .filter(|p| p.kind == NodeKind::RunProperties);
    let heading = &xml[run.full.start..run.body.start];
    let closing = &xml[run.body.end..run.full.end];
    let properties = props.map_or("", |p| &xml[p.full.clone()]);
    let key = props
        .map(|_| rewrite_properties(properties, None))
        .transpose()
        .map_err(|_| ())?
        .unwrap_or_default();
    for item in &run.children[usize::from(props.is_some())..] {
        match item.kind {
            NodeKind::Proofing => {}
            NodeKind::WordDrawing if supported_word_drawing(item) => {
                result.push(Child::Embedded(
                    format!("{heading}{properties}{}{closing}", &xml[item.full.clone()]),
                    item.full.clone(),
                    heading.len() + properties.len(),
                ));
            }
            NodeKind::Break => result.push(Child::Atom(format!(
                "{heading}{properties}{}{closing}",
                &xml[item.full.clone()]
            ))),
            NodeKind::Text
                if item.children.is_empty()
                    && item.body.start != item.full.end
                    && !["<a i=", "</a>"]
                        .iter()
                        .any(|prefix| item.text.contains(prefix)) =>
            {
                let opening = layout::rewrite_tag(
                    &xml[item.full.start..item.body.start],
                    &[(b"xml:space", "preserve")],
                )
                .map_err(|_| ())?;
                result.push(Child::Run(
                    Run {
                        write_before: format!("{heading}{properties}{opening}"),
                        write_after: format!("{}{closing}", &xml[item.body.end..item.full.end]),
                        key: key.clone(),
                        props: None,
                        hyperlink: false,
                        word: true,
                    },
                    item.text.clone(),
                ));
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

impl Paragraph {
    pub(super) fn from_node(
        xml: &str,
        node: &Node,
        protect_amounts: bool,
        format: Format,
    ) -> Result<Option<Self>, ()> {
        let mut before = String::new();
        let mut after = String::new();
        let mut children = Vec::new();
        let normalized;
        let content = if format == Format::Word {
            normalized = balanced_word_content(xml, &node.children)?;
            normalized.as_slice()
        } else {
            &node.children
        };
        let mut ordered_tags = format == Format::Word
            && content
                .iter()
                .any(|node| matches!(node.kind, NodeKind::Field | NodeKind::WordRange));
        for child in content {
            let full = &xml[child.full.clone()];
            match child.kind {
                NodeKind::Properties | NodeKind::WordPrefix => before.push_str(full),
                NodeKind::Proofing => {}
                NodeKind::EndProperties | NodeKind::WordSuffix => after.push_str(full),
                NodeKind::Break | NodeKind::Field => children.push(Child::Atom(full.to_owned())),
                NodeKind::WordDrawing if supported_word_drawing(child) => {
                    children.push(Child::Embedded(full.to_owned(), child.full.clone(), 0));
                }
                NodeKind::Run
                    if format == Format::Word
                        && child
                            .children
                            .iter()
                            .filter(|n| n.kind != NodeKind::RunProperties)
                            .count()
                            > 1 =>
                {
                    split_word_run(xml, child, &mut children)?;
                    ordered_tags |= child.children.iter().any(|n| n.kind == NodeKind::Break);
                }
                NodeKind::Run | NodeKind::WordLink | NodeKind::WordRange => {
                    let word = format == Format::Word;
                    let run_node = if matches!(child.kind, NodeKind::WordLink | NodeKind::WordRange)
                    {
                        match child.children.as_slice() {
                            [run] if run.kind == NodeKind::Run => run,
                            _ => return Err(()),
                        }
                    } else {
                        child
                    };
                    if word
                        && child.kind == NodeKind::Run
                        && run_node
                            .children
                            .iter()
                            .any(|n| matches!(n.kind, NodeKind::Break | NodeKind::WordDrawing))
                        && run_node.children.iter().all(|n| {
                            matches!(n.kind, NodeKind::Break | NodeKind::RunProperties)
                                || (n.kind == NodeKind::WordDrawing && supported_word_drawing(n))
                        })
                    {
                        // A Word run may contain a protected drawing, tab, or reference.
                        children.push(Child::Embedded(full.to_owned(), child.full.clone(), 0));
                        continue;
                    }
                    let Some(text) = run_node.children.iter().find(|c| c.kind == NodeKind::Text)
                    else {
                        return Err(());
                    };
                    if ["<a i=", "</a>"]
                        .iter()
                        .any(|prefix| text.text.contains(prefix))
                        || !text.children.is_empty()
                        || text.body.start == text.full.end
                        || run_node
                            .children
                            .iter()
                            .any(|c| !matches!(c.kind, NodeKind::Text | NodeKind::RunProperties))
                    {
                        return Err(());
                    }
                    let props = match run_node.children.as_slice() {
                        [only_text] if only_text.kind == NodeKind::Text => None,
                        [props, text]
                            if props.kind == NodeKind::RunProperties
                                && text.kind == NodeKind::Text =>
                        {
                            Some(props)
                        }
                        _ => return Err(()),
                    };
                    let mut write_before = xml[child.full.start..text.full.start].to_owned();
                    let text_open = &xml[text.full.start..text.body.start];
                    if word {
                        write_before.push_str(
                            &layout::rewrite_tag(text_open, &[(b"xml:space", "preserve")])
                                .map_err(|_| ())?,
                        );
                    } else {
                        write_before.push_str(text_open);
                    }
                    let run = Run {
                        write_before,
                        write_after: xml[text.body.end..child.full.end].to_owned(),
                        key: props
                            .map(|p| rewrite_properties(&xml[p.full.clone()], None))
                            .transpose()
                            .map_err(|_| ())?
                            .unwrap_or_default(),
                        props: props.map(|p| {
                            p.full.start - child.full.start..p.full.end - child.full.start
                        }),
                        hyperlink: matches!(child.kind, NodeKind::WordLink | NodeKind::WordRange)
                            || props.is_some_and(|p| {
                                p.children.iter().any(|n| n.kind == NodeKind::Link)
                            }),
                        word,
                    };
                    if let Some(Child::Run(prev, text_before)) = children.last_mut()
                        && !prev.hyperlink
                        && !run.hyperlink
                        && prev.key == run.key
                    {
                        text_before.push_str(&text.text);
                    } else {
                        children.push(Child::Run(run, text.text.clone()));
                    }
                }
                _ => return Err(()),
            }
        }
        if protect_amounts {
            children = keep_numbers_together(children);
        }
        let has_text = children
            .iter()
            .any(|c| matches!(c, Child::Run(_, text) if text.chars().any(char::is_alphabetic)));
        if !has_text {
            return Ok(None);
        }
        let mut weights: Vec<(&Run, usize)> = Vec::new();
        for child in &children {
            if let Child::Run(run, text) = child
                && !run.hyperlink
            {
                let n = text.chars().count();
                if let Some((_, count)) = weights.iter_mut().find(|(style, _)| style.key == run.key)
                {
                    *count += n;
                } else {
                    weights.push((run, n));
                }
            }
        }
        let base = weights
            .into_iter()
            .rev()
            .max_by_key(|(_, weight)| *weight)
            .map(|(run, _)| run.clone());
        let mut tags = Vec::new();
        let mut embedded_atoms = Vec::new();
        let mut items = Vec::new();
        for child in children {
            match child {
                Child::Run(_, text) if text.is_empty() => {}
                Child::Run(run, text)
                    if text.trim().is_empty()
                        && (run.hyperlink || base.as_ref().is_none_or(|b| b.key != run.key)) =>
                {
                    // Spaces in a distinct font or width are layout elements.
                    let original =
                        format!("{}{}{}", run.write_before, escape(&text), run.write_after);
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(original));
                }
                Child::Run(run, text)
                    if !run.hyperlink && base.as_ref().is_some_and(|b| b.key == run.key) =>
                {
                    items.push(Item::Text(text, None))
                }
                Child::Run(run, text) => {
                    items.push(Item::Text(text, Some(tags.len())));
                    tags.push(Tag::Style(run));
                }
                Child::Atom(xml) => {
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(xml));
                }
                Child::Embedded(xml, source, offset) => {
                    embedded_atoms.push(EmbeddedAtom {
                        tag: tags.len(),
                        source,
                        offset,
                    });
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(xml));
                }
            }
        }
        // For link-only paragraphs, only the tagged link can supply a run template.
        let base = base
            .or_else(|| {
                tags.iter().find_map(|t| match t {
                    Tag::Style(run) => Some(run.clone()),
                    _ => None,
                })
            })
            .ok_or(())?;
        let font_size = node
            .children
            .iter()
            .flat_map(|child| &child.children)
            .filter(|child| {
                matches!(
                    child.kind,
                    NodeKind::RunProperties | NodeKind::DefaultRunProperties
                )
            })
            .filter_map(|child| layout::font_points(&xml[child.full.clone()]))
            .max_by(f64::total_cmp);
        Ok(Some(Self {
            body: node.body.clone(),
            font_size,
            inherited_font_size: None,
            ordered_tags,
            word_location: None,
            before,
            after,
            base,
            tags,
            embedded_atoms,
            items,
        }))
    }

    /// Outer Word paragraph number and table membership; text boxes share their parent location.
    pub fn word_location(&self) -> Option<(usize, bool)> {
        self.word_location
    }

    pub fn word_translation_needs_review(&self, translated: &str, language: &str) -> bool {
        if self.word_location.is_none() {
            return false;
        }
        let translated = self.repair_word_atom_close(translated);
        let Ok(visible) = self.visible_target(&translated) else {
            return true;
        };
        let original = self.plain_text();
        if original.trim() == visible.trim() {
            return original.split_whitespace().count() > 1 || original.chars().any(is_japanese);
        }
        match language {
            "en" => visible.chars().filter(|&ch| is_japanese(ch)).count() >= 3,
            "ja" => {
                let original = original
                    .split(|ch: char| !ch.is_ascii_alphabetic())
                    .filter(|word| word.len() >= 3)
                    .collect::<HashSet<_>>();
                let mut consecutive = 0;
                visible
                    .split(|ch: char| !ch.is_ascii_alphabetic())
                    .filter(|word| !word.is_empty())
                    .any(|word| {
                        if original.contains(word) {
                            consecutive += 1;
                        } else {
                            consecutive = 0;
                        }
                        consecutive >= 2
                    })
            }
            _ => false,
        }
    }

    pub fn has_tags(&self) -> bool {
        !self.tags.is_empty()
    }

    pub fn plain_text(&self) -> String {
        self.items
            .iter()
            .map(|item| match item {
                Item::Text(s, _) => s.as_str(),
                Item::Atom(_) => " ",
            })
            .collect()
    }

    /// Model input: ordinary text plus anchors for distinct styles and immutable runs.
    pub fn source(&self) -> String {
        let mut text = String::new();
        for item in &self.items {
            match item {
                Item::Text(s, None) => text.push_str(s),
                Item::Text(s, Some(id)) => text.push_str(&anchors::wrap(id + 1, s, false)),
                Item::Atom(id) => text.push_str(&anchors::wrap(id + 1, "", true)),
            }
        }
        text
    }

    /// Validate anchor identity, order where required, and nonempty text before export.
    pub fn valid(&self, text: &str) -> bool {
        self.rebuild(text, "").is_ok()
    }

    /// Counts a change only after the same repair and validation used for export.
    pub fn changes_text(&self, text: &str, language: &str) -> bool {
        let text = self.repair_word_atom_close(text);
        self.with_word_spaces(&text, language)
            .and_then(|text| self.visible_target(&text))
            .is_ok_and(|visible| {
                !visible.trim().is_empty() && visible.trim() != self.plain_text().trim()
            })
    }

    /// Check scaled yen amounts for Japanese/English; this is not a general numeric proof.
    pub fn amounts_match(&self, text: &str, language: &str) -> bool {
        let text = self.repair_word_atom_close(text);
        self.with_word_spaces(&text, language)
            .and_then(|text| self.visible_target(&text))
            .is_ok_and(|visible| amounts::matches(&self.plain_text(), &visible, language))
    }

    // The model sometimes closes an ordinary style twice before resuming at a
    // protected Word atom. Remove only the surplus close. The regular parser
    // still requires every original tag and validates each tag's contents.
    fn repair_word_atom_close<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        use std::borrow::Cow;
        // An ordinary style may be left open immediately before the next style.
        // Repair only one missing close in a style-only Word paragraph. Links,
        // fields, drawings, unknown/duplicate IDs and any remaining malformed
        // structure are still rejected by the normal rebuild validation.
        if self.base.word
            && self
                .tags
                .iter()
                .all(|tag| matches!(tag, Tag::Style(run) if !run.hyperlink))
        {
            let opens: Vec<_> = text.match_indices("<a i=\"").map(|(i, _)| i).collect();
            if opens.len() == self.tags.len() && text.matches("</a>").count() + 1 == opens.len() {
                let missing: Vec<_> = opens
                    .windows(2)
                    .filter(|pair| !text[pair[0]..pair[1]].contains("</a>"))
                    .map(|pair| pair[1])
                    .collect();
                if let [at] = missing.as_slice() {
                    let mut corrected = text.to_owned();
                    corrected.insert_str(*at, "</a>");
                    return Cow::Owned(corrected);
                }
            }
        }
        if !self.base.word || !self.tags.iter().any(|tag| matches!(tag, Tag::Atom(_))) {
            return Cow::Borrowed(text);
        }

        let mut offset = 0;
        let mut previous_style = None;
        loop {
            let rest = &text[offset..];
            match anchors::next(rest) {
                Ok(Some(tag)) => {
                    previous_style =
                        matches!(self.tags.get(tag.id), Some(Tag::Style(run)) if !run.hyperlink)
                            .then_some(tag.id);
                    offset += tag.end;
                }
                Ok(None) => break,
                Err(_) => {
                    let Some(id) = previous_style else { break };
                    let Some(close) = rest.find("</a>") else {
                        break;
                    };
                    let Some(after) = rest.get(close + "</a>".len()..) else {
                        break;
                    };
                    if rest[..close].contains(['<', '>']) {
                        break;
                    }
                    let following = after.trim_start();
                    if !matches!(anchors::next(following), Ok(Some(tag))
                        if tag.start == 0
                            && tag.id == id + 1
                            && tag.body == Some("◇")
                            && matches!(self.tags.get(tag.id), Some(Tag::Atom(_))))
                    {
                        break;
                    }
                    let mut corrected = text.to_owned();
                    corrected.replace_range(offset + close..offset + close + "</a>".len(), "");
                    return Cow::Owned(corrected);
                }
            }
        }
        Cow::Borrowed(text)
    }

    pub(super) fn with_word_spaces<'a>(
        &self,
        text: &'a str,
        language: &str,
    ) -> Result<std::borrow::Cow<'a, str>, Error> {
        use std::borrow::Cow;
        if !self.base.word || language != "en" || self.tags.is_empty() {
            return Ok(Cow::Borrowed(text));
        }

        // A style boundary between contiguous Latin letters may be inside a name or identifier.
        // Source spaces and Japanese text provide evidence for a word boundary in English.
        let edge = |item: &Item, first: bool| match item {
            Item::Text(text, _) if first => text.chars().next(),
            Item::Text(text, _) => text.chars().next_back(),
            Item::Atom(_) => None,
        };
        let can_separate = |left: Option<char>, right: Option<char>| {
            left.zip(right).is_some_and(|(left, right)| {
                [left, right].iter().any(|&c| {
                    c.is_whitespace() || matches!(c, 'ぁ'..='ヿ' | '㐀'..='䶿' | '一'..='鿿')
                })
            })
        };
        let mut boundaries = vec![(false, false); self.tags.len()];
        for (index, item) in self.items.iter().enumerate() {
            if let Item::Text(_, Some(id)) = item {
                boundaries[*id] = (
                    can_separate(
                        index
                            .checked_sub(1)
                            .and_then(|i| edge(&self.items[i], false)),
                        edge(item, true),
                    ),
                    can_separate(
                        edge(item, false),
                        self.items.get(index + 1).and_then(|item| edge(item, true)),
                    ),
                );
            }
        }

        let mut insertions = Vec::new();
        let mut previous = None::<(u8, bool)>;
        let mut observe =
            |fragment: &str, offset, (before, after), previous: &mut Option<(u8, bool)>| {
                if let (Some(&first), Some(&last)) =
                    (fragment.as_bytes().first(), fragment.as_bytes().last())
                {
                    if first.is_ascii_alphabetic()
                        && previous.is_some_and(|(last, boundary)| {
                            last.is_ascii_alphabetic() && (boundary || before)
                        })
                    {
                        insertions.push(offset);
                    }
                    *previous = Some((last, after));
                }
            };
        let mut offset = 0;
        while let Some(tag) = anchors::next(&text[offset..])? {
            observe(
                &text[offset..offset + tag.start],
                offset,
                (false, false),
                &mut previous,
            );
            match (self.tags.get(tag.id), tag.body) {
                (Some(Tag::Style(_)), Some(body)) => {
                    observe(
                        body,
                        offset + tag.body_start,
                        boundaries[tag.id],
                        &mut previous,
                    );
                }
                _ => previous = None,
            }
            offset += tag.end;
        }
        observe(&text[offset..], offset, (false, false), &mut previous);
        if insertions.is_empty() {
            return Ok(Cow::Borrowed(text));
        }
        let mut spaced = String::with_capacity(text.len() + insertions.len());
        let mut start = 0;
        for insertion in insertions {
            spaced.push_str(&text[start..insertion]);
            spaced.push(' ');
            start = insertion;
        }
        spaced.push_str(&text[start..]);
        Ok(Cow::Owned(spaced))
    }

    pub(super) fn visible_target(&self, mut target: &str) -> Result<String, Error> {
        let mut out = String::new();
        while let Some(tag) = anchors::next(target)? {
            out.push_str(&target[..tag.start]);
            match self.tags.get(tag.id) {
                Some(Tag::Style(_)) => out.push_str(tag.body.unwrap_or_default()),
                Some(Tag::Atom(xml))
                    if xml.split_once('<').is_some_and(|(_, tag)| {
                        tag.split([':', ' ', '/', '>'])
                            .take(2)
                            .any(|part| part == "br")
                    }) =>
                {
                    out.push('\n')
                }
                _ => out.push(' '),
            }
            target = &target[tag.end..];
        }
        out.push_str(target);
        Ok(out)
    }

    pub(super) fn rebuild(&self, text: &str, target_language: &str) -> Result<String, Error> {
        self.rebuild_with_edits(text, target_language, &[])
    }

    pub(super) fn rebuild_with_edits(
        &self,
        text: &str,
        target_language: &str,
        edits: &[(Range<usize>, String)],
    ) -> Result<String, Error> {
        // Every nested edit must belong to exactly one preserved atom. Reject
        // unexpected overlap instead of discarding a child's translation.
        for (range, _) in edits {
            if self.body.start <= range.start
                && range.end <= self.body.end
                && self
                    .embedded_atoms
                    .iter()
                    .filter(|atom| atom.source.start <= range.start && range.end <= atom.source.end)
                    .count()
                    != 1
            {
                return Err(Error::Translation);
            }
        }
        if text.trim().is_empty() {
            return Err(Error::Translation);
        }
        let repaired = self.repair_word_atom_close(text);
        let text = self.with_word_spaces(&repaired, target_language)?;
        let mut result = self.before.clone();
        let mut seen = vec![false; self.tags.len()];
        let mut rest = text.as_ref();
        let mut previous = None;
        while let Some(ParsedTag {
            start,
            id,
            body,
            end,
            ..
        }) = anchors::next(rest)?
        {
            self.base.write(
                &mut result,
                &rest[..start],
                !self.base.hyperlink,
                target_language,
            )?;
            let tag = self.tags.get(id).ok_or(Error::Translation)?;
            if self.ordered_tags && previous.is_some_and(|old| id <= old) {
                return Err(Error::Translation);
            }
            previous = Some(id);
            if std::mem::replace(&mut seen[id], true) {
                return Err(Error::Translation);
            }
            match tag {
                Tag::Style(run) if body.is_some_and(|s| !s.trim().is_empty() && s != "◇") => {
                    run.write(&mut result, body.unwrap(), true, target_language)?;
                }
                Tag::Atom(xml) if body == Some("◇") => {
                    let mut xml = xml.clone();
                    if let Some(atom) = self.embedded_atoms.iter().find(|atom| atom.tag == id) {
                        // The writer visits paragraphs in reverse XML order, so
                        // these non-overlapping descendant edits are descending.
                        for (range, replacement) in edits {
                            if atom.source.start <= range.start && range.end <= atom.source.end {
                                let start = atom.offset + range.start - atom.source.start;
                                let end = atom.offset + range.end - atom.source.start;
                                xml.replace_range(start..end, replacement);
                            }
                        }
                    }
                    result.push_str(&xml);
                }
                _ => return Err(Error::Translation),
            }
            rest = &rest[end..];
        }
        self.base
            .write(&mut result, rest, !self.base.hyperlink, target_language)?;
        if seen.contains(&false) {
            return Err(Error::Translation);
        }
        if !self.after.is_empty() {
            result.push_str(&rewrite_properties(&self.after, Some(target_language))?);
        }
        Ok(result)
    }
}

impl Run {
    fn write(
        &self,
        out: &mut String,
        text: &str,
        allowed: bool,
        target_language: &str,
    ) -> Result<(), Error> {
        if text.is_empty() {
            return Ok(());
        }
        if !allowed
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
        {
            return Err(Error::Translation);
        }
        if !self.word
            && let Some(props) = &self.props
        {
            out.push_str(&self.write_before[..props.start]);
            out.push_str(&rewrite_properties(
                &self.write_before[props.clone()],
                Some(target_language),
            )?);
            out.push_str(&self.write_before[props.end..]);
        } else {
            out.push_str(&self.write_before);
        }
        out.push_str(&escape(text));
        out.push_str(&self.write_after);
        Ok(())
    }
}

/// Only the opening element is adjusted. Nested formatting and relationship IDs stay intact.
fn rewrite_properties(xml: &str, target: Option<&str>) -> Result<String, Error> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let event = reader.read_event().map_err(|_| Error::Xml)?;
    let (element, empty) = match event {
        Event::Start(e) => (e.clone(), false),
        Event::Empty(e) => (e.clone(), true),
        _ => return Err(Error::Xml),
    };
    let attributes = element
        .attributes()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::Xml)?;
    let mut rebuilt = element.clone().into_owned();
    rebuilt.clear_attributes();
    for attr in attributes {
        let key = attr.key.as_ref();
        match target {
            None if matches!(key, b"lang" | b"altLang" | b"dirty" | b"err" | b"smtClean") => {
                continue;
            }
            Some(_) if key == b"err" => continue,
            Some(lang) if key == b"lang" && !lang.is_empty() => {
                let lang = match lang {
                    "en" => "en-US",
                    "ja" => "ja-JP",
                    _ => lang,
                };
                rebuilt.push_attribute(("lang", lang));
            }
            _ => rebuilt.push_attribute(attr),
        }
    }
    let fragment = if empty {
        Event::Empty(rebuilt)
    } else {
        Event::Start(rebuilt)
    };
    let mut writer = quick_xml::Writer::new(Vec::new());
    writer.write_event(fragment)?;
    let mut out = String::from_utf8(writer.into_inner()).map_err(|_| Error::Xml)?;
    out.push_str(&xml[reader.buffer_position() as usize..]);
    Ok(out)
}
