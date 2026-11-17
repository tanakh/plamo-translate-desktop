use super::*;

#[test]
fn word_layout_risks_use_outer_paragraph_positions() {
    let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><w:body><w:p><w:r><w:drawing><wp:anchor><w:txbxContent><w:p><w:r><w:t>Box</w:t></w:r></w:p></w:txbxContent></wp:anchor></w:drawing></w:r></w:p><w:p><w:r><w:t>Label</w:t></w:r></w:p><w:tbl><w:tblPr><w:tblpPr/></w:tblPr><w:tr><w:trPr><w:trHeight w:hRule="exact"/></w:trPr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
    let part = Part {
        format: Format::Word,
        name: "word/document.xml".into(),
        archive_index: 0,
        xml: xml.into(),
        paragraphs: Vec::new(),
        frames: Vec::new(),
    };
    assert_eq!(part.word_layout_risks(), vec![1, 3]);
    assert_eq!(part.xml, xml);
}

#[test]
fn preserves_paragraph_boundaries_even_when_text_looks_wrapped() {
    let root = r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#;
    let paragraph = |text, after| {
        format!(
            r#"<a:p><a:pPr><a:spcAft><a:spcPts val="{after}"/></a:spcAft><a:buNone/></a:pPr><a:r><a:rPr sz="1200"/><a:t>{text}</a:t></a:r></a:p>"#
        )
    };
    let xml = format!(
        "{root}<txBody>{}{}{}{}</txBody><txBody>{}{}</txBody></root>",
        paragraph("お店で商品を買いました。", 0),
        paragraph("代金 1,000円と、", 0),
        paragraph("税金100円を", 400),
        paragraph("支払いました。", 0),
        paragraph("街を清潔に", 0),
        paragraph("保つために 4.5%", 0),
    );
    let (groups, skipped) = parse_paragraphs(&xml, true).unwrap();
    assert_eq!(skipped, 0);
    let texts: Vec<_> = groups.iter().map(Paragraph::plain_text).collect();
    assert_eq!(
        texts,
        [
            "お店で商品を買いました。",
            "代金 1,000円と、",
            "税金100円を",
            "支払いました。",
            "街を清潔に",
            "保つために 4.5%",
        ]
    );

    let mut edited = xml.clone();
    let paid = "I paid 1,000 yen for the item and 100 yen in tax.";
    edited.replace_range(
        groups[1].body.clone(),
        &groups[1].rebuild(paid, "en").unwrap(),
    );
    let (written, skipped) = paragraphs(&edited).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(written[1].plain_text(), paid);
    assert_eq!(written.len(), 6);
    assert_eq!(paragraphs(&xml).unwrap().0.len(), 6);
}

#[test]
fn inherited_bullets_and_literal_old_tags_keep_separate_paragraphs() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("bullets.pptx");
    let first = "従業員の働きやすい環境づくりに向けた活動";
    let second = "地域とのつながりを深めるための取り組み";
    let slide = format!(
        r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr lvl="0"/><a:r><a:t>{first}</a:t></a:r></a:p><a:p><a:pPr lvl="0"/><a:r><a:t>{second}</a:t></a:r></a:p><a:p><a:r><a:t>Literal &lt;g and &lt;x strings</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:sld>"#
    );
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let master = r#"<p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:txStyles><p:bodyStyle><a:lvl1pPr><a:buChar char="•"/></a:lvl1pPr></p:bodyStyle></p:txStyles></p:sldMaster>"#;
    for (name, contents) in [
        ("[Content_Types].xml", "<Types/>"),
        ("ppt/presentation.xml", "<presentation/>"),
        ("ppt/slides/slide1.xml", slide.as_str()),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            r#"<Relationships><Relationship Type="x/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#,
        ),
        ("ppt/slideLayouts/slideLayout1.xml", "<layout/>"),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            r#"<Relationships><Relationship Type="x/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#,
        ),
        ("ppt/slideMasters/slideMaster1.xml", master),
    ] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(contents.as_bytes()).unwrap();
    }
    std::fs::write(&input, writer.finish().unwrap().into_inner()).unwrap();
    let doc = OfficeDocument::read_for_translation(&input).unwrap();
    let paragraphs = &doc.parts()[0].paragraphs;
    assert_eq!(
        paragraphs
            .iter()
            .map(Paragraph::plain_text)
            .collect::<Vec<_>>(),
        [first, second, "Literal <g and <x strings"]
    );
    let out = doc
        .write(
            "en",
            &[vec![
                Some("Workplace activities".into()),
                Some("Community activities".into()),
                Some("Literal <g and <x preserved".into()),
            ]],
        )
        .unwrap();
    let reopened = OfficeDocument::open(out.clone()).unwrap();
    assert_eq!(reopened.parts()[0].paragraphs.len(), 3);
    let mut archive = ZipArchive::new(Cursor::new(out)).unwrap();
    let mut untouched = String::new();
    archive
        .by_name("ppt/slideMasters/slideMaster1.xml")
        .unwrap()
        .read_to_string(&mut untouched)
        .unwrap();
    assert_eq!(untouched, master);
}

#[test]
fn warns_about_overflow_and_preserves_small_reading_guides() {
    let xml = concat!(
        r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
        r#"<sp><spPr><a:xfrm><a:ext cx="762000" cy="190500"/></a:xfrm></spPr>"#,
        r#"<txBody><a:bodyPr wrap="none" tIns="0" bIns="0"/>"#,
        r#"<a:p><a:r><a:rPr sz="700"/><a:t>ぜいむしょ</a:t></a:r></a:p></txBody></sp>"#,
        r#"<sp><spPr><a:xfrm><a:ext cx="762000" cy="254000"/></a:xfrm></spPr>"#,
        r#"<txBody><a:bodyPr wrap="none" tIns="0" bIns="0"/>"#,
        r#"<a:p><a:r><a:rPr sz="1200"/><a:t>消費税</a:t></a:r></a:p></txBody></sp></root>"#,
    );
    let (paragraphs, _) = parse_paragraphs(xml, true).unwrap();
    let frames = layout::frames(xml).unwrap();
    assert!(frames[0].contains(&paragraphs[0]));
    assert!(!frames[0].contains(&paragraphs[1]));
    assert!(frames[0].likely_furigana(&paragraphs[0]));
    assert!(!frames[1].likely_furigana(&paragraphs[1]));
    assert!(
        frames[1]
            .fit(std::iter::once((
                &paragraphs[1],
                Some("Consumption tax collected by the store")
            )),)
            .is_some()
    );
    assert!(
        frames[1]
            .fit(std::iter::once((&paragraphs[1], Some("Tax"))),)
            .is_none()
    );
    assert!(has_untranslated_text(r#"<chart><v>消費税</v></chart>"#).unwrap());
    assert!(!has_untranslated_text(r#"<chart><v>4.5%</v></chart>"#).unwrap());
}

#[test]
fn yen_amounts_are_not_split_across_style_tags_and_keep_their_value() {
    let xml = concat!(
        r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p>"#,
        r#"<a:r><a:rPr sz="900"/><a:t>約</a:t></a:r>"#,
        r#"<a:r><a:rPr sz="2000"/><a:t>1兆6,721</a:t></a:r>"#,
        r#"<a:r><a:rPr sz="900"/><a:t>億円を計上</a:t></a:r></a:p></root>"#,
    );
    let (parts, skipped) = parse_paragraphs(xml, true).unwrap();
    assert_eq!(skipped, 0);
    let p = &parts[0];
    assert!(p.source().contains("1兆6,721億円"));
    assert!(p.amounts_match("About 1.6721 trillion yen", "en"));
    assert!(p.amounts_match("About ¥1,672,100,000,000", "en"));
    assert!(!p.amounts_match("About 1.6721 billion yen", "en"));
    assert!(amounts::matches(
        "115兆1,978億円",
        "115 trillion 197.8 billion yen",
        "en"
    ));
    assert!(amounts::matches(
        "約2兆6,002億円",
        "2 trillion and 600.2 billion yen",
        "en"
    ));
    assert!(!p.amounts_match("About 1.6721 trillion", "en"));
    assert!(amounts::matches(
        "about 123 million yen",
        "約１億２，３００万円",
        "ja"
    ));
    assert!(!amounts::matches("123 million yen", "１２３億円", "ja"));
    assert!(amounts::matches(
        "Sales may rise 123 points.",
        "売上は123ポイント伸びるかもしれない。",
        "ja"
    ));
}

#[test]
fn word_preserves_textless_drawings_beside_translated_instructions() {
    let prefix = r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">"#;
    let drawing = r#"<mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><shape/><offset>1050925</offset></w:drawing></mc:Choice><mc:Fallback><w:pict><shape/></w:pict></mc:Fallback></mc:AlternateContent>"#;
    for mixed in [false, true] {
        let middle = if mixed { "" } else { "</w:r><w:r>" };
        let xml = format!(
            "{prefix}<w:p><w:r>{drawing}{middle}<w:t>太枠内に○を記入してください。</w:t></w:r></w:p></root>"
        );
        let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(paragraphs.len(), 1);
        let source = paragraphs[0].source();
        let target = source.replace(
            "太枠内に○を記入してください。",
            "Place a circle inside the bold border.",
        );
        let rebuilt = paragraphs[0].rebuild(&target, "en").unwrap();
        assert!(rebuilt.contains(drawing));
        assert!(rebuilt.contains("Place a circle"));
        assert!(!rebuilt.contains("太枠"));
    }
    let xml = format!(
        "{prefix}<w:p><w:r><w:drawing><w:txbxContent><w:p><w:r><w:t>図形の文字</w:t></w:r></w:p></w:txbxContent></w:drawing></w:r><w:r><w:t>本文</w:t></w:r></w:p></root>"
    );
    let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
    assert_eq!(paragraphs.len(), 2);
    assert_eq!(skipped, 0);
    assert_eq!(paragraphs[0].plain_text().trim(), "本文");
    assert_eq!(paragraphs[1].plain_text(), "図形の文字");
    assert_eq!(paragraphs[0].word_location(), Some((1, false)));
    assert_eq!(paragraphs[1].word_location(), Some((1, false)));
}

fn word_package(xml: &str) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in [
        ("[Content_Types].xml", "<Types/>"),
        ("word/document.xml", xml),
        ("word/_rels/document.xml.rels", "<Relationships/>"),
        ("word/media/untouched.bin", "original asset"),
    ] {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(contents.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn word_writes_inner_and_outer_translations_without_overlapping_edits() {
    for mixed_run in [false, true] {
        for alternate in [false, true] {
            let textbox = r#"<w:drawing><w:txbxContent><w:p><w:r><w:t>Box</w:t></w:r></w:p><w:p><w:r><w:t>Box</w:t></w:r></w:p></w:txbxContent></w:drawing>"#;
            let fallback = r#"<mc:Fallback><w:pict><w:txbxContent><w:p><w:r><w:t>Box fallback</w:t></w:r></w:p></w:txbxContent></w:pict></mc:Fallback>"#;
            let drawing = if alternate {
                format!(
                    r#"<mc:AlternateContent><mc:Choice Requires="wps">{textbox}</mc:Choice>{fallback}</mc:AlternateContent>"#
                )
            } else {
                textbox.to_owned()
            };
            let middle = if mixed_run { "" } else { "</w:r><w:r>" };
            let xml = format!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><w:body><w:p><w:r>{drawing}{middle}<w:t>Body</w:t></w:r></w:p><w:p><w:r><w:t>Following</w:t></w:r></w:p></w:body></w:document>"#
            );
            let original = word_package(&xml);
            let document =
                OfficeDocument::open_format(original.clone(), true, Format::Word).unwrap();
            assert!(document.unsupported.is_empty());
            let paragraphs = &document.parts[0].paragraphs;
            assert_eq!(paragraphs.len(), 4);
            assert_eq!(paragraphs[3].word_location(), Some((2, false)));
            let translated = ["本文", "枠の一段落目", "枠の二段落目は別の訳", "次の段落"];
            // Cover independent failures/retentions of the parent and each child.
            for mask in 0..16 {
                let targets = paragraphs
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        (mask & (1 << i) != 0)
                            .then(|| p.source().replace(p.plain_text().trim(), translated[i]))
                    })
                    .collect::<Vec<_>>();
                let output = document.write("ja", &[targets]).unwrap();
                if mask == 0 {
                    assert_eq!(output, original);
                }
                let reopened =
                    OfficeDocument::open_format(output.clone(), true, Format::Word).unwrap();
                assert!(reopened.unsupported.is_empty());
                assert_eq!(reopened.parts[0].paragraphs.len(), 4);
                for (i, p) in reopened.parts[0].paragraphs.iter().enumerate() {
                    let original_text = paragraphs[i].plain_text();
                    assert_eq!(
                        p.plain_text().trim(),
                        if mask & (1 << i) != 0 {
                            translated[i]
                        } else {
                            original_text.trim()
                        }
                    );
                }
                if alternate {
                    assert!(reopened.parts[0].xml.contains(fallback));
                }
                let mut before = ZipArchive::new(Cursor::new(&original)).unwrap();
                let mut after = ZipArchive::new(Cursor::new(&output)).unwrap();
                for name in [
                    "[Content_Types].xml",
                    "word/_rels/document.xml.rels",
                    "word/media/untouched.bin",
                ] {
                    let mut a = Vec::new();
                    let mut b = Vec::new();
                    before.by_name(name).unwrap().read_to_end(&mut a).unwrap();
                    after.by_name(name).unwrap().read_to_end(&mut b).unwrap();
                    assert_eq!(a, b);
                }
            }
        }
    }
}

#[test]
fn word_reports_protected_textbox_contents_without_locking_surrounding_body() {
    for protected in [
        "<w:ins><w:p><w:r><w:t>Protected</w:t></w:r></w:p></w:ins>",
        "<w:p><w:ins><w:r><w:t>Protected</w:t></w:r></w:ins></w:p>",
        "<w:sdt><w:sdtPr><w:dataBinding/></w:sdtPr><w:sdtContent><w:p><w:r><w:t>Protected</w:t></w:r></w:p></w:sdtContent></w:sdt>",
    ] {
        let xml = format!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:drawing><w:txbxContent>{protected}<w:p><w:r><w:t>Editable</w:t></w:r></w:p></w:txbxContent></w:drawing></w:r><w:r><w:t>Body</w:t></w:r></w:p></w:body></w:document>"#
        );
        let document = OfficeDocument::open_format(word_package(&xml), true, Format::Word).unwrap();
        assert_eq!(document.unsupported.len(), 1);
        assert_eq!(document.unsupported[0].source, "Protected");
        assert_eq!(document.unsupported[0].paragraph, Some(1));
        let paragraphs = &document.parts[0].paragraphs;
        assert_eq!(paragraphs.len(), 2);
        let targets = paragraphs
            .iter()
            .map(|p| {
                Some(
                    p.source()
                        .replace("Body", "本文")
                        .replace("Editable", "編集可能"),
                )
            })
            .collect();
        let output = document.write("ja", &[targets]).unwrap();
        let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
        assert!(reopened.parts[0].xml.contains(protected));
        assert_eq!(reopened.unsupported.len(), 1);
        assert_eq!(reopened.parts[0].paragraphs[0].plain_text().trim(), "本文");
        assert_eq!(reopened.parts[0].paragraphs[1].plain_text(), "編集可能");
    }
}

#[test]
fn word_nested_textboxes_keep_bookmarks_and_each_translation() {
    let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><w:body><w:p><w:r><w:t>Outer</w:t></w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><w:txbxContent><w:p><w:bookmarkStart w:id="12"/><w:r><w:t>Middle</w:t></w:r><w:r><w:pict><w:txbxContent><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Inner</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:txbxContent></w:pict></w:r><w:bookmarkEnd w:id="12"/></w:p></w:txbxContent></w:drawing></mc:Choice><mc:Fallback/></mc:AlternateContent></w:p></w:body></w:document>"#;
    let document = OfficeDocument::open_format(word_package(xml), true, Format::Word).unwrap();
    assert!(document.unsupported.is_empty());
    let paragraphs = &document.parts[0].paragraphs;
    assert_eq!(paragraphs.len(), 3);
    assert_eq!(paragraphs[2].word_location(), Some((1, true)));
    let replacements = [("Outer", "外側"), ("Middle", "中間"), ("Inner", "内側")];
    for mask in 0..8 {
        let targets = paragraphs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (mask & (1 << i) != 0)
                    .then(|| p.source().replace(replacements[i].0, replacements[i].1))
            })
            .collect();
        let output = document.write("ja", &[targets]).unwrap();
        let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
        assert!(reopened.unsupported.is_empty());
        assert_eq!(reopened.parts[0].paragraphs.len(), 3);
        for (i, p) in reopened.parts[0].paragraphs.iter().enumerate() {
            assert_eq!(
                p.plain_text().trim(),
                if mask & (1 << i) != 0 {
                    replacements[i].1
                } else {
                    replacements[i].0
                }
            );
        }
        for marker in [
            r#"<w:bookmarkStart w:id="12"/>"#,
            r#"<w:bookmarkEnd w:id="12"/>"#,
        ] {
            assert_eq!(reopened.parts[0].xml.matches(marker).count(), 1);
        }
    }
}

#[test]
fn word_repairs_one_missing_close_between_ordinary_styles() {
    let xml = r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>それぞれに</w:t></w:r><w:r><w:t>当てはまる項目の太枠内に○を記入</w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t>してください。</w:t></w:r></w:p></root>"#;
    let (paragraphs, _) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
    let p = &paragraphs[0];
    let target = r#"<a i="1">Please mark a circle inside the bold box for each applicable item<a i="2">.</a>"#;
    let rebuilt = p.rebuild(target, "en").unwrap();
    assert!(rebuilt.contains("Please mark a circle"));
    assert!(!p.valid(&target.replace("i=\"2\"", "i=\"1\"")));
    assert!(!p.valid(&target.replace("i=\"2\"", "i=\"9\"")));
    assert!(!p.valid(r#"<a i="1">Missing second style</a>"#));
    let link = xml
        .replace("<w:r><w:rPr><w:b/>", "<w:hyperlink><w:r><w:rPr><w:b/>")
        .replace(
            "それぞれに</w:t></w:r>",
            "それぞれに</w:t></w:r></w:hyperlink>",
        );
    let (paragraphs, _) = parse_office_paragraphs(&link, true, Format::Word).unwrap();
    assert!(!paragraphs[0].valid(target));
}

#[test]
fn word_restores_missing_english_spaces_at_style_boundaries() {
    let xml = concat!(
        r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>粗利益率は</w:t></w:r>"#,
        r#"<w:r><w:rPr><w:i/></w:rPr><w:t xml:space="default">低下</w:t></w:r>"#,
        r#"<w:r><w:t>し、38.5%となりました。</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>Open </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>Power</w:t></w:r><w:r><w:t>Point now.</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>価格は</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>１億円</w:t></w:r>"#,
        r#"<w:r><w:t>です。</w:t></w:r></w:p></root>"#,
    );
    let (paragraphs, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
    assert_eq!(skipped, 0);
    let p = &paragraphs[0];
    let spacing = r#"<a i="1">The gross profit margin</a><a i="2">fell</a>to 38.5%."#;
    let rebuilt = p.rebuild(spacing, "en").unwrap();
    assert!(rebuilt.contains(r#"<w:rPr><w:i/></w:rPr><w:t xml:space="preserve"> fell</w:t>"#));
    assert!(rebuilt.contains(r#"<w:t xml:space="preserve"> to 38.5%.</w:t>"#));
    assert!(!rebuilt.contains(r#"xml:space="default""#));
    let natural = r#"<a i="1">The gross profit margin</a> <a i="2">fell</a>, to 38.5%."#;
    assert_eq!(p.with_word_spaces(natural, "en").unwrap(), natural);
    assert_eq!(p.with_word_spaces(spacing, "fr").unwrap(), spacing);

    let p = &paragraphs[1];
    let brand = r#"Open<a i="1">Power</a>Point now."#;
    assert_eq!(
        p.with_word_spaces(brand, "en").unwrap(),
        r#"Open<a i="1"> Power</a>Point now."#
    );
    assert!(paragraphs[2].amounts_match(r#"The price is <a i="1">100 million</a>yen."#, "en"));
}

#[test]
fn word_unbound_block_rich_text_keeps_control_properties_and_protects_typed_controls() {
    let wrap = |pr: &str| {
        format!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:sdt><w:sdtPr>{pr}</w:sdtPr><w:sdtContent><w:p><w:r><w:t>Policy objectives</w:t></w:r></w:p></w:sdtContent></w:sdt></w:body></w:document>"#
        )
    };
    let source =
        wrap(r#"<w:id w:val="42"/><w:placeholder><w:docPart w:val="original"/></w:placeholder>"#);
    let (paragraphs, skipped) = parse_office_paragraphs(&source, true, Format::Word).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(paragraphs.len(), 1);
    let p = &paragraphs[0];
    let mut output = source.clone();
    output.replace_range(p.body.clone(), &p.rebuild("政策目標", "ja").unwrap());
    assert!(output.contains(
        r#"<w:id w:val="42"/><w:placeholder><w:docPart w:val="original"/></w:placeholder>"#
    ));
    for restricted in [
        "<w:dataBinding/>",
        "<w:dropDownList/>",
        "<w:showingPlcHdr/>",
        "<w:lock/>",
        "<w:date/>",
    ] {
        let (p, skipped) = parse_office_paragraphs(&wrap(restricted), true, Format::Word).unwrap();
        assert!(p.is_empty(), "{restricted}");
        assert_eq!(skipped, 1);
    }
}

#[test]
fn word_preserves_links_and_simple_bookmarks_and_leaves_revisions_intact() {
    let xml = concat!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>参照先：</w:t></w:r>"#,
        r#"<w:hyperlink r:id="rId5"><w:r><w:rPr><w:b/></w:rPr><w:t>対応表</w:t></w:r></w:hyperlink></w:p>"#,
        r#"<w:p><w:bookmarkStart w:id="1"/><w:r><w:t>保存</w:t></w:r><w:bookmarkEnd w:id="1"/></w:p>"#,
        r#"<w:p><w:ins w:id="2"><w:r><w:t>修正中</w:t></w:r></w:ins></w:p></w:body></w:document>"#,
    );
    let (paragraphs, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
    assert_eq!(skipped, 1);
    assert_eq!(paragraphs.len(), 2);
    let p = &paragraphs[0];
    assert_eq!(p.source(), r#"参照先：<a i="1">対応表</a>"#);
    let rendered = p
        .rebuild(r#"See <a i="1">mapping table</a>"#, "en")
        .unwrap();
    assert!(rendered.contains(r#"<w:hyperlink r:id="rId5">"#));
    assert!(rendered.contains(r#"<w:t xml:space="preserve">See </w:t>"#));
    assert!(rendered.contains("<w:rPr><w:b/></w:rPr>"));
    assert!(rendered.contains(r#"<w:pStyle w:val="Heading1"/>"#));
    let bookmark = paragraphs[1].rebuild("Save", "en").unwrap();
    assert!(bookmark.contains(r#"<w:bookmarkStart w:id="1"/><w:r><w:t xml:space="preserve">Save</w:t></w:r><w:bookmarkEnd w:id="1"/>"#));
    assert!(paragraphs[1].valid("Save"));
}

#[test]
fn word_repairs_a_surplus_style_close_before_a_protected_atom() {
    let xml = format!(
        r#"<w:document xmlns:w="{}"><w:body><w:p>
            <w:r><w:t>通常の文</w:t></w:r><w:r><w:rPr><w:u w:val="single"/></w:rPr><w:t>内容</w:t></w:r>
            <w:r><w:br/></w:r><w:r><w:t>役割分担の確認</w:t></w:r>
            </w:p></w:body></w:document>"#,
        std::str::from_utf8(WORD).unwrap()
    );
    let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
    assert_eq!(skipped, 0);
    let paragraph = &paragraphs[0];
    let output =
        r#"Did you confirm the <a i="1">nature</a>, and </a><a i="2">◇</a> the division of roles?"#;
    let rebuilt = paragraph.rebuild(output, "en").unwrap();
    assert!(rebuilt.contains(">nature</w:t>"));
    assert!(rebuilt.contains("<w:br/>"));
    assert!(rebuilt.contains("division of roles?"));
    assert!(!rebuilt.contains("&lt;/a&gt;"));
    assert!(paragraph.amounts_match(output, "en"));

    let missing_break = output.replace(r#"<a i="2">◇</a>"#, "");
    let wrong_break = output.replace("◇", "line");
    let duplicate_break = format!(r#"{output}<a i="2">◇</a>"#);
    assert!(!paragraph.valid(&missing_break));
    assert!(!paragraph.valid(&wrong_break));
    assert!(!paragraph.valid(&duplicate_break));
}

#[test]
fn word_translates_text_sharing_a_run_with_tab_or_saved_page_break() {
    let xml = format!(
        r#"<w:document xmlns:w="{}"><w:body><w:tbl><w:tr><w:tc>
            <w:p><w:r><w:rPr><w:b/></w:rPr><w:lastRenderedPageBreak/><w:tab/><w:t>Business Needs</w:t></w:r></w:p>
            <w:p><w:sdt><w:sdtContent><w:r><w:t>Protected form text</w:t></w:r></w:sdtContent></w:sdt></w:p>
            </w:tc></w:tr></w:tbl></w:body></w:document>"#,
        std::str::from_utf8(WORD).unwrap()
    );
    let (paragraphs, retained) = parse_paragraph_details(&xml, true, Format::Word).unwrap();
    assert_eq!(paragraphs.len(), 1);
    let heading = &paragraphs[0];
    assert_eq!(heading.word_location(), Some((1, true)));
    assert_eq!(heading.source(), r#"<a i="1">◇</a>Business Needs"#);
    let translated = heading
        .rebuild(r#"<a i="1">◇</a>事業上の必要性"#, "ja")
        .unwrap();
    assert!(translated.contains("<w:tab/>"));
    assert!(translated.contains("事業上の必要性"));
    assert!(!translated.contains("lastRenderedPageBreak"));
    assert_eq!(retained[0].text, "Protected form text");
    assert!(retained[0].in_table);
}

#[test]
fn word_translates_inside_balanced_ranges_and_preserves_endpoint_paragraphs() {
    let xml = concat!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:commentRangeStart w:id="4"/><w:r><w:t>始点</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>範囲内</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>終点</w:t></w:r><w:commentRangeEnd w:id="4"/><w:r><w:commentReference w:id="4"/></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>ページ </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r>"#,
        r#"<w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r>"#,
        r#"<w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r>"#,
        r#"<w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:t>です。</w:t></w:r></w:p>"#,
        r#"</w:body></w:document>"#,
    );
    let (p, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
    assert_eq!(skipped, 2);
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].plain_text(), "範囲内");
    assert_eq!(p[1].source(), r#"ページ <a i="1">◇</a>です。"#);
    let rebuilt = p[1].rebuild(r#"Page <a i="1">◇</a>."#, "en").unwrap();
    assert!(rebuilt.contains(r#"<w:instrText xml:space="preserve"> PAGE </w:instrText>"#));
    assert!(rebuilt.contains(r#"<w:fldChar w:fldCharType="end"/>"#));
}

#[test]
fn word_keeps_unclosed_ranges_locked_and_translates_nested_range_interiors() {
    let prefix = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#;
    let inner = concat!(
        r#"<w:p><w:bookmarkStart w:id="11" w:name="outer"/><w:bookmarkStart w:id="12" w:name="inner"/><w:r><w:t>Heading</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>Body inside both bookmarks.</w:t></w:r></w:p>"#,
        r#"<w:p><w:bookmarkEnd w:id="12"/><w:bookmarkEnd w:id="11"/><w:r><w:t>End</w:t></w:r></w:p>"#,
    );
    let xml = format!("{prefix}{inner}</w:body></w:document>");
    let (p, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(p.len(), 3);
    let mut output = xml.clone();
    output.replace_range(
        p[1].body.clone(),
        &p[1].rebuild("両方の範囲内の本文。", "ja").unwrap(),
    );
    assert_eq!(
        output,
        xml.replace(
            "<w:t>Body inside both bookmarks.",
            r#"<w:t xml:space="preserve">両方の範囲内の本文。"#
        )
    );
    let unclosed = xml.replace(r#"<w:bookmarkEnd w:id="11"/>"#, "");
    let (p, _) = parse_office_paragraphs(&unclosed, true, Format::Word).unwrap();
    assert!(p.is_empty());
}

#[test]
fn word_heading_bookmarks_keep_their_original_edges_and_ids() {
    let xml = concat!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="1" w:name="_Toc1"/><w:bookmarkStart w:id="2" w:name="_Toc2"/><w:r><w:lastRenderedPageBreak/><w:t>C</w:t></w:r><w:r><w:t>olour</w:t></w:r><w:bookmarkEnd w:id="2"/></w:p>"#,
        r#"<w:p><w:bookmarkEnd w:id="1"/><w:r><w:t>Next</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>Before</w:t></w:r><w:bookmarkStart w:id="3"/><w:r><w:t>Inside</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>Inside</w:t></w:r><w:bookmarkEnd w:id="3"/><w:r><w:t>After</w:t></w:r></w:p>"#,
        r#"</w:body></w:document>"#,
    );
    let (p, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
    assert_eq!(skipped, 2); // Inline cross-paragraph endpoints stay protected.
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].source(), "Colour");
    let rebuilt = p[0].rebuild("色", "ja").unwrap();
    assert_eq!(
        rebuilt,
        concat!(
            r#"<w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="1" w:name="_Toc1"/><w:bookmarkStart w:id="2" w:name="_Toc2"/>"#,
            r#"<w:r><w:t xml:space="preserve">色</w:t></w:r><w:bookmarkEnd w:id="2"/>"#,
        )
    );
    assert!(
        p[1].rebuild("次", "ja")
            .unwrap()
            .starts_with(r#"<w:bookmarkEnd w:id="1"/>"#)
    );
    let tail = r#"<w:r><w:t xml:space="preserve"> </w:t></w:r><w:r><w:br/></w:r>"#;
    let with_tail = xml.replace(
        r#"<w:bookmarkEnd w:id="2"/></w:p>"#,
        &format!(r#"<w:bookmarkEnd w:id="2"/>{tail}</w:p>"#),
    );
    let (p, _) = parse_office_paragraphs(&with_tail, true, Format::Word).unwrap();
    assert_eq!(p[0].source(), "Colour");
    assert!(
        p[0].rebuild("色", "ja")
            .unwrap()
            .ends_with(&format!(r#"<w:bookmarkEnd w:id="2"/>{tail}"#))
    );
}

#[test]
fn powerpoint_export_limits_shrinking_and_reports_unfitted_frames() {
    // Heights exercise an exact 80% fit, a fit requiring more shrinking,
    // auto-fit shapes, hundredth-point rounding, and the absolute font floor.
    for (height, font, autofit, expected_font, overflow) in [
        (243840, 2000, "", 1600, false),
        (230000, 2000, "", 2000, true),
        (127000, 2000, "<a:spAutoFit/>", 2000, true),
        (123000, 1001, "", 801, false),
        (121920, 900, "", 800, false),
        (63500, 700, "", 700, true),
    ] {
        let geometry = format!(r#"<a:ext cx="3810000" cy="{height}"/>"#);
        let xml = format!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><sp><spPr><a:xfrm>{geometry}</a:xfrm></spPr><txBody><a:bodyPr lIns="0" rIns="0" tIns="0" bIns="0">{autofit}</a:bodyPr><a:p><a:r><a:rPr sz="{font}"/><a:t>Source label</a:t></a:r></a:p></txBody></sp></root>"#
        );
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in [
            ("[Content_Types].xml", "<Types/>"),
            ("ppt/presentation.xml", "<presentation/>"),
            ("ppt/slides/slide1.xml", xml.as_str()),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        let deck = OfficeDocument::open_mode(zip.finish().unwrap().into_inner(), true).unwrap();
        let translation = "Translation & text";
        let targets = vec![Some(translation.to_owned())];
        let review = deck.parts()[0].overflow_frames(&targets);
        assert_eq!(
            review.len(),
            usize::from(overflow),
            "font={font}, height={height}"
        );
        let output = deck.write("en", &[targets]).unwrap();
        let reopened = OfficeDocument::open(output).unwrap();
        let slide = &reopened.parts()[0];
        assert_eq!(slide.paragraphs[0].plain_text(), translation);
        assert_eq!(
            slide.paragraphs[0].font_size,
            Some(f64::from(expected_font) / 100.0)
        );
        assert!(slide.xml.contains(&geometry));
        assert!(slide.xml.contains(autofit));
    }
}

#[test]
fn reports_overflow_without_changing_shape_or_paragraph_geometry() {
    let xml = concat!(
        r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
        r#"<sp><spPr><a:xfrm><a:ext cx="1524000" cy="889000"/></a:xfrm>"#,
        r#"<a:prstGeom prst="homePlate"/></spPr><txBody><a:bodyPr wrap="none"/>"#,
        r#"<a:p><a:pPr latinLnBrk="1"/><a:r><a:rPr sz="1300"/><a:t>一般公募</a:t></a:r></a:p></txBody></sp>"#,
        r#"<graphicFrame><xfrm><a:off x="0" y="1524000"/><a:ext cx="3048000" cy="252000"/></xfrm>"#,
        r#"<a:tbl><a:tblGrid><a:gridCol w="3048000"/></a:tblGrid><a:tr h="252000"><a:tc><a:txBody>"#,
        r#"<a:bodyPr/><a:p><a:r><a:rPr sz="1100"/><a:t>インフラ、防災・減災、安全・安心分野</a:t></a:r></a:p>"#,
        r#"</a:txBody><a:tcPr marT="54000" marB="36000"/></a:tc></a:tr></a:tbl></graphicFrame></root>"#,
    );
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in [
        ("[Content_Types].xml", "<Types/>"),
        ("ppt/presentation.xml", "<presentation/>"),
        ("ppt/slides/slide1.xml", xml),
    ] {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(contents.as_bytes()).unwrap();
    }
    let original = zip.finish().unwrap().into_inner();
    let deck = OfficeDocument::open_mode(original.clone(), true).unwrap();
    assert_eq!(deck.write("en", &[vec![None, None]]).unwrap(), original);
    let arrow = "Public Call for Data Disclosure Requests";
    let cell = "Infrastructure, Disaster Prevention and Mitigation, and Safety and Security";
    let targets = vec![Some(arrow.into()), Some(cell.into())];
    assert!(!deck.parts()[0].overflow_frames(&targets).is_empty());
    let out = deck.write("en", &[targets]).unwrap();
    let parsed = OfficeDocument::open(out).unwrap();
    let slide = &parsed.parts()[0];
    assert_eq!(slide.paragraphs[0].plain_text(), arrow);
    assert_eq!(slide.paragraphs[1].plain_text(), cell);
    assert!(slide.xml.contains(r#"wrap="none""#));
    assert!(slide.xml.contains(r#"latinLnBrk="1""#));
    assert!(slide.xml.contains(r#"marT="54000" marB="36000""#));
    assert!(
        slide.paragraphs[1]
            .font_size
            .is_some_and(|size| (8.0..=11.0).contains(&size))
    );
    assert!(slide.xml.contains(r#"<a:ext cx="1524000" cy="889000"/>"#));
    assert!(slide.xml.contains(r#"<a:tr h="252000">"#));
}

#[test]
fn fits_translated_text_beside_a_retained_paragraph() {
    let xml = concat!(
        r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
        r#"<sp><spPr><a:xfrm><a:ext cx="2540000" cy="635000"/></a:xfrm></spPr><txBody><a:bodyPr/>"#,
        r#"<a:p><a:r><a:rPr sz="1400"/><a:t>Keep this label</a:t></a:r></a:p>"#,
        r#"<a:p><a:r><a:rPr sz="1400"/><a:t>短い文章</a:t></a:r></a:p>"#,
        r#"</txBody></sp></root>"#,
    );
    let (p, _) = paragraphs(xml).unwrap();
    let frame = layout::frames(xml).unwrap().remove(0);
    let translation = "This translated sentence needs several lines inside the original text box.";
    let fit = frame.fit(p.iter().zip([None, Some(translation)])).unwrap();
    // This smaller box used to fit by shrinking more than 20%.
    assert!(fit.overflow);
    assert!(!fit.applies());

    let taller = xml.replace(r#"cy="635000""#, r#"cy="850900""#);
    let frame = layout::frames(&taller).unwrap().remove(0);
    let fit = frame.fit(p.iter().zip([None, Some(translation)])).unwrap();
    assert!(fit.applies());
    assert!(!fit.overflow);
    // The source paragraph remains absent from the replacement set; its
    // original 14pt height must still leave room for the translated text.
    let changed = fit
        .compact_paragraph(p[1].rebuild("Translated text", "en").unwrap())
        .unwrap();
    assert!(!changed.contains(r#"sz="1400""#));
}

#[test]
fn preserves_namespaces_relationships_and_fields_when_rebuilding() {
    let xml = concat!(
        r#"<root xmlns:d="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
        r#"<d:p><d:pPr lvl="1"/><d:r><d:t>Read the following </d:t></d:r>"#,
        r#"<d:r><d:rPr><d:hlinkClick r:id="rId5"/></d:rPr><d:t>report</d:t></d:r>"#,
        r#"<d:fld id="field-1" type="slidenum"><d:t>1</d:t></d:fld></d:p></root>"#,
    );
    let (found, skipped) = paragraphs(xml).unwrap();
    assert_eq!(skipped, 0);
    let p = &found[0];
    assert_eq!(
        p.source(),
        r#"Read the following <a i="1">report</a><a i="2">◇</a>"#
    );
    {
        let translated = format!(
            "次の{}を確認{}",
            anchors::wrap(1, "報告書", false),
            anchors::wrap(2, "", true)
        );
        assert!(!p.valid(&format!("次の{}を確認", anchors::wrap(1, "報告書", false))));
        assert!(!p.valid(&format!("{translated}{}", anchors::wrap(2, "", true))));
        let replacement = p.rebuild(&translated, "ja").unwrap();
        assert!(replacement.contains(r#"<d:hlinkClick r:id="rId5"/>"#));
        assert!(
            replacement.contains(r#"<d:fld id="field-1" type="slidenum"><d:t>1</d:t></d:fld>"#)
        );
        let mut updated = xml.to_owned();
        updated.replace_range(p.body.clone(), &replacement);
        assert_eq!(paragraphs(&updated).unwrap().0[0].source(), translated);
    }
}
#[test]
fn styled_whitespace_round_trips_as_an_opaque_run() {
    let xml = concat!(
        r#"<a:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
        r#"<a:r><a:t>Read the following</a:t></a:r>"#,
        r#"<a:r><a:rPr sz="1400"/><a:t>&#160;　</a:t></a:r>"#,
        r#"<a:r><a:t>report</a:t></a:r></a:p>"#,
    );
    let (found, skipped) = paragraphs(xml).unwrap();
    assert_eq!(skipped, 0);
    let paragraph = &found[0];
    assert_eq!(
        paragraph.source(),
        r#"Read the following<a i="1">◇</a>report"#
    );
    let out = paragraph
        .rebuild(r#"Read the following<a i="1">◇</a>report"#, "")
        .unwrap();
    assert!(out.contains("<a:rPr sz=\"1400\"/><a:t>\u{a0}　</a:t>"));
}

#[test]
fn follows_main_and_header_relationships_instead_of_fixed_filenames() {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let body = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Body text</w:t></w:r></w:p></w:body></w:document>"#;
    let header = r#"<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>Header text</w:t></w:r></w:p></w:hdr>"#;
    for (name, content) in [
        ("[Content_Types].xml", "<Types/>"),
        (
            "_rels/.rels",
            r#"<Relationships><Relationship Type="x/officeDocument" Target="content/main.xml"/></Relationships>"#,
        ),
        ("content/main.xml", body),
        (
            "content/_rels/main.xml.rels",
            r#"<Relationships><Relationship Type="x/header" Target="../head/title.xml"/></Relationships>"#,
        ),
        ("head/title.xml", header),
        ("word/document.xml", "unrelated bytes"),
    ] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(content.as_bytes()).unwrap();
    }
    let original = writer.finish().unwrap().into_inner();
    let doc = OfficeDocument::open_format(original, true, Format::Word).unwrap();
    assert_eq!(doc.parts.len(), 2);
    let targets = doc
        .parts
        .iter()
        .map(|p| {
            p.paragraphs
                .iter()
                .map(|p| Some(p.source().replace("text", "translated")))
                .collect()
        })
        .collect::<Vec<_>>();
    let output = doc.write("en", &targets).unwrap();
    let again = OfficeDocument::open_format(output.clone(), true, Format::Word).unwrap();
    assert_eq!(again.parts[0].paragraphs[0].plain_text(), "Body translated");
    assert_eq!(
        again.parts[1].paragraphs[0].plain_text(),
        "Header translated"
    );
    let mut zip = ZipArchive::new(Cursor::new(output)).unwrap();
    let mut retained = String::new();
    zip.by_name("word/document.xml")
        .unwrap()
        .read_to_string(&mut retained)
        .unwrap();
    assert_eq!(retained, "unrelated bytes");
}

#[test]
fn rejects_doctype_and_escaping_targets_in_package_relationships() {
    for relationships in [
        r#"<!DOCTYPE Relationships [<!ENTITY x "payload">]><Relationships/>"#,
        r#"<Relationships><Relationship Type="x/officeDocument" Target="../outside.xml"/></Relationships>"#,
        r#"<Relationships><Relationship Type="x/officeDocument" Target="https://example.org/main.xml" TargetMode="External"/></Relationships>"#,
    ] {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in [
            ("[Content_Types].xml", "<Types/>"),
            ("_rels/.rels", relationships),
            ("word/document.xml", "<document/>"),
        ] {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        assert!(
            OfficeDocument::open_format(writer.finish().unwrap().into_inner(), true, Format::Word)
                .is_err()
        );
    }
}
