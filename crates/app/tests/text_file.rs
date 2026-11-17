//! The file adapter uses the same chunk recovery as pasted text.
use files::Translation;

#[test]
fn document_cut_keeps_original_and_reports_its_line() {
    use engine::document::{DocumentOptions, Output, Progress, translate_document};
    use engine::pfml::{LangCode, Prompt, TranslationOptions};
    use engine::{Generation, StopReason, TranslateStats, TranslationBackend};
    use std::cell::RefCell;

    struct CutSecond;
    impl TranslationBackend for CutSecond {
        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            _: &Generation,
            sink: &mut dyn FnMut(&str),
            _: &dyn Fn() -> bool,
        ) -> engine::Result<TranslateStats> {
            let (text, stop_reason) = if prompt.text == "切ります。" {
                ("partial", StopReason::LengthLimit)
            } else {
                ("Translated.", StopReason::Finished)
            };
            sink(text);
            Ok(TranslateStats {
                stop_reason,
                prompt_tokens: 0,
                generated_tokens: 4,
                prompt_duration: Default::default(),
                generation_duration: Default::default(),
                retries: 0,
                cut: 0,
            })
        }
    }

    let source = "最初です。\n\n切ります。";
    let result = RefCell::new(Translation::default());
    let stats = translate_document(
        &mut CutSecond,
        &str::len,
        4096,
        &DocumentOptions {
            max_chunk_tokens: 18,
            ..Default::default()
        },
        &TranslationOptions::new(LangCode::new("en").unwrap()),
        source,
        &mut |event| match event {
            Output::Text(piece) => result.borrow_mut().append(piece),
            Output::Truncate(len) => result.borrow_mut().rewind(len),
        },
        &mut |event| {
            if let Progress::Cut {
                source: (a, b),
                output: (c, d),
                ..
            } = event
            {
                result.borrow_mut().cut(a..b, c..d);
            }
        },
        &|| false,
    )
    .unwrap();
    assert_eq!(stats.cut, 1);
    assert_eq!(
        result.into_inner().finish(source).unwrap(),
        ("Translated.\n\n切ります。".into(), vec![3])
    );
}
