//! File units share the document translator while letting callers protect tagged content.

use std::sync::Arc;

pub use crate::document::translate_units;

/// Format-specific validation, called before text can be published or remembered.
pub type Validator = Arc<dyn Fn(usize, &str) -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Only `Complete` may be applied; all other outcomes retain source content.
pub enum UnitResult {
    Complete { text: String, retried: bool },
    Cut,
    TooLong,
    Invalid,
}

pub struct Unit {
    pub text: String,
    /// Guard tags or inline elements against being split during recovery.
    pub indivisible: bool,
}

#[derive(Default)]
pub struct UnitTuning {
    /// Contexts shared by groups of units, with an optional context index per unit.
    pub contexts: Vec<String>,
    pub context_by_unit: Vec<usize>,
    /// A different group starts a fresh translation history.
    pub history_by_unit: Vec<usize>,
    pub reuse_translations: bool,
}

/// Ordered file units and their independent validation/context budgets.
pub struct UnitRequest {
    pub units: Vec<Unit>,
    pub tuning: UnitTuning,
    pub max_unit_tokens: usize,
    pub max_history_tokens: usize,
    pub validate: Validator,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Generation, Result, StopReason, TranslateStats, TranslationBackend};
    use pfml::LangCode;
    use pfml::{Prompt, TranslationOptions};
    use std::time::Duration;

    #[derive(Default)]
    struct Fake {
        history: Vec<Vec<String>>,
        calls: Vec<String>,
        contexts: Vec<String>,
    }

    impl TranslationBackend for Fake {
        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            _: &Generation,
            sink: &mut dyn FnMut(&str),
            _: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            self.calls.push(prompt.text.into());
            self.contexts.push(prompt.options.context.clone());
            self.history.push(
                prompt
                    .history
                    .iter()
                    .map(|p| p.translation.clone())
                    .collect(),
            );
            sink(prompt.text);
            Ok(TranslateStats {
                prompt_tokens: 10,
                generated_tokens: prompt.text.len(),
                prompt_duration: Duration::ZERO,
                generation_duration: Duration::ZERO,
                stop_reason: match prompt.text {
                    "cut" => StopReason::LengthLimit,
                    "cancel" => StopReason::Cancelled,
                    _ => StopReason::Finished,
                },
                retries: 0,
                cut: 0,
            })
        }
    }

    fn tagged(texts: &[&str]) -> Vec<Unit> {
        texts
            .iter()
            .map(|text| Unit {
                text: (*text).into(),
                indivisible: true,
            })
            .collect()
    }

    #[test]
    fn file_context_and_history_stay_in_scope_and_repeated_units_reuse_valid_output() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            units: tagged(&["heading", "line", "line2", "heading", "line3"]),
            tuning: UnitTuning {
                contexts: vec!["first slide".into(), "second slide".into()],
                context_by_unit: vec![0, 0, 1, 1, 1],
                history_by_unit: vec![1, 1, 2, 2, 3],
                reuse_translations: true,
            },
            max_unit_tokens: 100,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let mut output = Vec::new();
        translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, result| output.push(result),
            &|| false,
        )
        .unwrap();
        assert_eq!(backend.calls, ["heading", "line", "line2", "line3"]);
        assert_eq!(
            backend.contexts,
            ["first slide", "first slide", "second slide", "second slide"]
        );
        assert_eq!(
            backend.history,
            [vec![], vec!["heading".to_owned()], vec![], vec![]]
        );
        assert_eq!(output[0], output[3]);
        assert_eq!(output.len(), 5);
    }

    #[test]
    fn failed_units_are_not_committed_or_used_as_history() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["good", "bad", "cut", "last"]),
            max_unit_tokens: 100,
            max_history_tokens: 1000,

            validate: Arc::new(|_, text| text != "bad"),
        };
        let mut results = Vec::new();
        let stats = translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |i, r| results.push((i, r)),
            &|| false,
        )
        .unwrap();
        assert_eq!(
            results,
            vec![
                (
                    0,
                    UnitResult::Complete {
                        text: "good".into(),
                        retried: false
                    }
                ),
                (1, UnitResult::Invalid),
                (2, UnitResult::Cut),
                (
                    3,
                    UnitResult::Complete {
                        text: "last".into(),
                        retried: false
                    }
                )
            ]
        );
        assert_eq!(backend.history.last().unwrap(), &["good"]);
        assert_eq!(stats.cut, 1);
        assert_eq!(stats.retries, 0);
    }

    #[test]
    fn oversized_unit_is_not_split_and_later_unit_runs() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["<g1>do not split me</g1>", "ok"]),
            max_unit_tokens: 5,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let mut results = Vec::new();
        translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, r| results.push(r),
            &|| false,
        )
        .unwrap();
        assert_eq!(results[0], UnitResult::TooLong);
        assert_eq!(backend.calls, ["ok"]);
    }

    #[test]
    fn rejects_plain_unit_without_leaving_its_chunks_in_history() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: ["a\n\nb", "c"]
                .into_iter()
                .map(|text| Unit {
                    text: text.into(),
                    indivisible: false,
                })
                .collect(),
            max_unit_tokens: 1,
            max_history_tokens: 100,

            validate: Arc::new(|i, _| i != 0),
        };
        let mut results = Vec::new();
        translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, result| results.push(result),
            &|| false,
        )
        .unwrap();
        assert_eq!(backend.calls, ["a", "b", "c"]);
        assert!(backend.history.last().unwrap().is_empty());
        assert_eq!(results[0], UnitResult::Invalid);
    }

    #[test]
    fn cancellation_does_not_publish_a_unit() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["hello"]),
            max_unit_tokens: 100,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let stats = translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, _| panic!("cancelled unit"),
            &|| true,
        )
        .unwrap();
        assert_eq!(stats.stop_reason, StopReason::Cancelled);
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn cancellation_during_generation_discards_partial_text_and_stops_batch() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["good", "cancel", "later"]),
            max_unit_tokens: 100,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let mut published = Vec::new();
        let stats = translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |i, result| published.push((i, result)),
            &|| false,
        )
        .unwrap();
        assert_eq!(stats.stop_reason, StopReason::Cancelled);
        assert_eq!(backend.calls, ["good", "cancel"]);
        assert_eq!(
            published,
            [(
                0,
                UnitResult::Complete {
                    text: "good".into(),
                    retried: false
                }
            )]
        );
    }
}
