//! File UI is a child of `view`, sharing its worker without a second model instance.
use std::sync::Arc;

use ::files::markdown::Markdown;
use ::files::office::{MAX_UNIT_TOKENS, OfficeDocument};
use engine::units::{Unit, UnitRequest, UnitResult, UnitTuning};

use gpui_kit::component::{Icon, progress::Progress};

use super::*;
use crate::file_history::FileReview;

pub(super) const FILE_PANEL_WIDTH: f32 = 780.;

#[derive(Clone)]
enum FileDocument {
    Text(Arc<::files::Document>),
    Markdown(Arc<Markdown>),
    Office {
        deck: Arc<OfficeDocument>,
        japanese: bool,
        units: usize,
    },
}

impl FileDocument {
    fn read(path: &Path) -> Result<Self, ::files::FileError> {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            return Ok(Self::Markdown(Arc::new(Markdown::read(path)?)));
        }
        if path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("pptx") || extension.eq_ignore_ascii_case("docx")
        }) {
            let deck = Arc::new(OfficeDocument::read_for_translation(path)?);
            let paragraphs = || deck.parts().iter().flat_map(|part| &part.paragraphs);
            let sample = paragraphs()
                .map(|p| p.plain_text())
                .scan(0, |size, text| {
                    if *size >= 16_384 {
                        return None;
                    }
                    *size += text.len();
                    Some(text)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let japanese = lang::looks_japanese(&sample);
            let units = paragraphs().count();
            Ok(Self::Office {
                deck,
                japanese,
                units,
            })
        } else {
            Ok(Self::Text(Arc::new(::files::read_document(path)?)))
        }
    }

    fn japanese(&self) -> bool {
        match self {
            Self::Text(doc) => lang::looks_japanese(doc.source()),
            Self::Markdown(doc) => {
                let sample = doc
                    .segments()
                    .iter()
                    .take(100)
                    .map(|s| &s.text[..s.text.floor_char_boundary(512)])
                    .collect::<Vec<_>>()
                    .join("\n");
                lang::looks_japanese(&sample)
            }
            Self::Office { japanese, .. } => *japanese,
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            Self::Text(_) => "txt",
            Self::Markdown(_) => "md",
            Self::Office { deck, .. } => deck.extension(),
        }
    }

    fn empty(&self) -> bool {
        match self {
            Self::Text(doc) => doc.source().trim().is_empty(),
            Self::Markdown(doc) => doc.segments().is_empty(),
            Self::Office { units, .. } => *units == 0,
        }
    }
}

#[derive(Clone)]
enum FileOutput {
    Text(Arc<str>),
    Markdown(Arc<Vec<u8>>),
    Office(Arc<Vec<Vec<Option<String>>>>),
}

impl FileOutput {
    fn bytes<'a>(
        &'a self,
        document: &FileDocument,
        target: &str,
    ) -> Result<std::borrow::Cow<'a, [u8]>, ::files::FileError> {
        Ok(match (document, self) {
            (FileDocument::Text(doc), Self::Text(text)) => doc.write(text).into(),
            (FileDocument::Markdown(_), Self::Markdown(bytes)) => bytes.as_slice().into(),
            (FileDocument::Office { deck, .. }, Self::Office(parts)) => {
                deck.write(target, parts)?.into()
            }
            _ => return Err(::files::FileError::MismatchedResults),
        })
    }
}

struct CompletedFile {
    output: FileOutput,
    review: FileReview,
    has_changes: bool,
}

struct UnitTranslation {
    results: Vec<Option<UnitResult>>,
    completed: usize,
}

impl UnitTranslation {
    fn new(count: usize) -> Self {
        Self {
            results: vec![None; count],
            completed: 0,
        }
    }
    fn text_targets(self) -> Option<Vec<Option<String>>> {
        (self.completed == self.results.len()).then(|| {
            self.results
                .into_iter()
                .map(|result| match result {
                    Some(UnitResult::Complete { text, .. }) => Some(text),
                    _ => None,
                })
                .collect()
        })
    }
}

fn office_location(part: &str, paragraph: Option<usize>) -> String {
    let stem = Path::new(part)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let number = stem.trim_start_matches(|c: char| !c.is_ascii_digit());
    let key = if part.starts_with("ppt/slides/") {
        "files.slide"
    } else if part.starts_with("ppt/notesSlides/") {
        "files.notes"
    } else if part.starts_with("word/header") {
        "files.header"
    } else if part.starts_with("word/footer") {
        "files.footer"
    } else if part == "word/document.xml" {
        "files.body"
    } else if part == "word/footnotes.xml" {
        "files.footnotes"
    } else if part == "word/endnotes.xml" {
        "files.endnotes"
    } else {
        "files.other_part"
    };
    let label = t!(key, number = number, name = stem.as_ref()).into_owned();
    match paragraph {
        Some(n) => t!("files.paragraph_location", part = label, number = n).into_owned(),
        None => label,
    }
}

fn finish_office(
    deck: &OfficeDocument,
    job: UnitTranslation,
    target: &str,
) -> Option<CompletedFile> {
    use ::files::office::RetentionReason;

    if job.completed != job.results.len() {
        return None;
    }
    let mut review = FileReview::default();
    for item in &deck.unsupported {
        review.locations.push(
            t!(
                "files.retained_at",
                location = office_location(&item.part, item.paragraph)
            )
            .into_owned(),
        );
        match item.reason {
            RetentionReason::TableOfContents => review.update_toc = true,
            RetentionReason::Range
            | RetentionReason::ProtectedStructure
            | RetentionReason::InlineStructure => {
                review.partial_translation |= !item.source.trim().is_empty();
            }
            RetentionReason::UnsupportedPart => {
                // Unused master/layout placeholders are still listed in the
                // details, but do not make every deck look partially translated.
                review.partial_translation |=
                    item.part.starts_with("ppt/charts/") || item.part.starts_with("ppt/diagrams/");
            }
            RetentionReason::ReadingGuide => {}
        }
    }

    let mut has_changes = false;
    let mut results = job.results.into_iter();
    let mut parts = Vec::new();
    for part in deck.parts() {
        let targets = part
            .paragraphs
            .iter()
            .enumerate()
            .map(|(index, paragraph)| {
                let location = office_location(
                    &part.name,
                    Some(paragraph.word_location().map_or(index + 1, |(n, _)| n)),
                );
                let outcome = results.next().flatten()?;
                if let UnitResult::Complete { text, .. } = outcome
                    && paragraph.amounts_match(&text, target)
                {
                    let changed = paragraph.changes_text(&text, target);
                    has_changes |= changed;
                    let needs_review = paragraph.word_translation_needs_review(&text, target);
                    review.partial_translation |= !changed || needs_review;
                    review.needs_review |= needs_review;
                    review.locations.push(
                        t!(
                            if changed {
                                "files.applied_at"
                            } else {
                                "files.retained_at"
                            },
                            location = location
                        )
                        .into_owned(),
                    );
                    if needs_review {
                        review.locations.push(
                            t!(
                                "files.check_at",
                                location = office_location(
                                    &part.name,
                                    paragraph.word_location().map(|(n, _)| n)
                                )
                            )
                            .into_owned(),
                        );
                    }
                    return Some(text);
                }
                review.needs_review = true;
                review.partial_translation = true;
                review
                    .locations
                    .push(t!("files.retained_at", location = location).into_owned());
                None
            })
            .collect::<Vec<_>>();
        let overflow = part.overflow_frames(&targets);
        review.check_slides |= !overflow.is_empty();
        for frame in overflow {
            review.locations.push(
                t!(
                    "files.frame_review",
                    location = office_location(&part.name, None),
                    number = frame
                )
                .into_owned(),
            );
        }
        let risks = part.word_layout_risks();
        review.check_word |= !risks.is_empty();
        for paragraph in risks {
            review.locations.push(
                t!(
                    "files.check_at",
                    location = office_location(&part.name, Some(paragraph))
                )
                .into_owned(),
            );
        }
        parts.push(targets);
    }
    Some(CompletedFile {
        output: FileOutput::Office(Arc::new(parts)),
        review,
        has_changes,
    })
}

#[derive(Default)]
pub(super) struct FilePanel {
    loaded: Option<(PathBuf, FileDocument)>,
    output: ::files::Translation,
    unit_translation: Option<UnitTranslation>,
    completed: Option<CompletedFile>,
    progress: Option<(usize, usize)>,
    io_busy: bool,
    cancelling: bool,
    target: String,
    pub(super) message: Option<String>,
    saved: Option<PathBuf>,
    show_review: bool,
    expanded_history: Option<PathBuf>,
    history_status: Option<String>,
}

fn completion_label(review: &FileReview) -> String {
    t!(if review.needs_review {
        "files.needs_review"
    } else {
        "files.done"
    })
    .into_owned()
}

impl FilePanel {
    /// A finished translation that exists only in memory.
    fn unsaved(&self) -> bool {
        self.saved.is_none() && self.completed.as_ref().is_some_and(|c| c.has_changes)
    }
}

/// What the user was doing when asked whether to drop an unsaved translation.
#[derive(Clone)]
enum AfterDiscard {
    Choose,
    Read(PathBuf),
    Close { quit: bool },
}

impl TranslatorView {
    pub(super) fn file_operation_busy(&self) -> bool {
        self.jobs.busy() || self.files.io_busy
    }
    pub(super) fn file_source_is_japanese(&self) -> Option<bool> {
        self.files.loaded.as_ref().map(|(_, doc)| doc.japanese())
    }

    /// Opens the file picker, asking first if that would replace an unsaved translation.
    pub(crate) fn open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.files.unsaved() && !self.file_operation_busy() {
            self.confirm_discard(AfterDiscard::Choose, window, cx);
        } else {
            self.choose_file(cx);
        }
    }

    /// Loads a dropped file, asking first if that would replace an unsaved translation.
    pub(super) fn drop_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.files.unsaved() && !self.file_operation_busy() {
            self.file_tab = true;
            self.confirm_discard(AfterDiscard::Read(path), window, cx);
            cx.notify();
        } else {
            self.read_file(path, cx);
        }
    }

    /// The translation stays until another file has actually been read: cancelling the picker
    /// after agreeing here loses nothing.
    fn confirm_discard(&mut self, then: AfterDiscard, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let then = then.clone();
            dialog
                .title(t!("files.discard_title").into_owned())
                .child(t!("files.discard_body").into_owned())
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("keep-file-result")
                                .label(t!("files.discard_keep").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("discard-file-result")
                                .danger()
                                .label(t!("files.discard_confirm").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    match then.clone() {
                                        AfterDiscard::Close { quit: true } => cx.quit(),
                                        AfterDiscard::Close { quit: false } => {
                                            window.remove_window()
                                        }
                                        AfterDiscard::Choose => {
                                            let _ =
                                                view.update(cx, |this, cx| this.choose_file(cx));
                                        }
                                        AfterDiscard::Read(path) => {
                                            let _ = view
                                                .update(cx, |this, cx| this.read_file(path, cx));
                                        }
                                    }
                                }),
                        ),
                )
        });
    }

    fn choose_file(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        self.file_tab = true;
        self.files.io_busy = true;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("files.choose").into_owned().into()),
        });
        cx.spawn(async move |this, cx| {
            let result = paths.await;
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.read_file(path, cx);
                        }
                    }
                    Ok(Err(_)) => {
                        this.files.message = Some(t!("files.picker_failed").into_owned());
                        cx.notify();
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn read_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        self.file_tab = true;
        self.files.io_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let result = FileDocument::read(&path);
                    (path, result)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    (path, Ok(document)) => {
                        let message = if document.empty() {
                            Some(t!("files.empty").into_owned())
                        } else {
                            None
                        };
                        this.files = FilePanel {
                            loaded: Some((path, document)),
                            message,
                            ..Default::default()
                        };
                    }
                    (_, Err(error)) => {
                        this.files.message =
                            Some(t!("files.error", error = error.to_string()).into_owned())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn start_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.jobs.busy()
            || self.files.io_busy
            || self
                .files
                .completed
                .as_ref()
                .is_some_and(|result| result.has_changes)
        {
            return;
        }
        if !self.model_ready {
            self.open_models(window, cx);
            return;
        }
        let Some((_, document)) = &self.files.loaded else {
            return;
        };
        if document.empty() {
            return;
        }
        let text = match document {
            FileDocument::Text(doc) => doc.source().to_owned(),
            FileDocument::Office { .. } | FileDocument::Markdown(_) => String::new(),
        };
        let request = self.build_request(text, document.japanese());
        let options = request
            .document
            .options(appdata::cut_marker(&rust_i18n::locale()).to_owned());
        self.debounce = Task::ready(());
        self.shown_partial |= self.current_job.is_some();
        self.current_job = None;
        self.status = Status::Ready;
        self.files.output = ::files::Translation::default();
        self.files.completed = None;
        self.files.progress = None;
        self.files.show_review = false;
        self.files.cancelling = false;
        self.files.message = None;
        self.files.saved = None;
        self.files.expanded_history = None;
        self.files.target = request.options.target.as_str().to_owned();
        let job = match document {
            FileDocument::Text(_) => self
                .worker
                .translate(request.options, options, request.text),
            FileDocument::Markdown(doc) => {
                let units = doc
                    .segments()
                    .iter()
                    .map(|s| Unit {
                        text: s.text.clone(),
                        indivisible: s.has_anchors(),
                    })
                    .collect::<Vec<_>>();
                self.files.unit_translation = Some(UnitTranslation::new(units.len()));
                let validator = Arc::clone(doc);
                self.worker.translate_units(
                    request.options,
                    UnitRequest {
                        units,
                        tuning: UnitTuning::default(),
                        max_unit_tokens: MAX_UNIT_TOKENS,
                        max_history_tokens: ::files::markdown::HISTORY_TOKENS,

                        validate: Arc::new(move |i, text| validator.segments()[i].valid(text)),
                    },
                )
            }
            FileDocument::Office { deck, units, .. } => {
                let mut locations = Vec::with_capacity(*units);
                let mut request_units = Vec::with_capacity(*units);
                for (part_index, part) in deck.parts().iter().enumerate() {
                    for (index, paragraph) in part.paragraphs.iter().enumerate() {
                        locations.push((part_index, index));
                        request_units.push(Unit {
                            text: paragraph.source(),
                            indivisible: paragraph.has_tags(),
                        });
                    }
                }
                let tuning = UnitTuning {
                    contexts: deck
                        .parts()
                        .iter()
                        .map(|part| part.source_context())
                        .collect(),
                    context_by_unit: locations.iter().map(|&(part, _)| part).collect(),
                    reuse_translations: true,
                    ..Default::default()
                };
                let validator = Arc::clone(deck);
                self.files.unit_translation = Some(UnitTranslation::new(*units));
                self.worker.translate_units(
                    request.options,
                    UnitRequest {
                        units: request_units,
                        tuning,
                        max_unit_tokens: MAX_UNIT_TOKENS,
                        max_history_tokens: 0,

                        validate: Arc::new(move |index, translated| {
                            let (part, paragraph) = locations[index];
                            validator.parts()[part].paragraphs[paragraph].valid(translated)
                        }),
                    },
                )
            }
        };
        self.jobs.start_file(job);
        cx.notify();
    }

    pub(super) fn cancel_file(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() && !self.files.cancelling {
            self.files.cancelling = true;
            self.worker.cancel();
            cx.notify();
        }
    }

    pub(super) fn file_token(&mut self, job: u64, text: &str) {
        if self.jobs.file() == Some(job) && !self.files.cancelling {
            self.files.output.append(text);
        }
    }

    /// Return true only for events owned by the file job. The existing text handler still
    /// receives all model/runtime events.
    pub(super) fn file_event(
        &mut self,
        event: &Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.jobs.file() else {
            return false;
        };
        let resume = match event {
            Event::Chunk {
                job, index, count, ..
            } if *job == active => {
                // Chunk events arrive when a chunk starts. Earlier chunks are complete.
                self.files.progress = Some((*index, *count));
                cx.notify();
                return true;
            }
            Event::Rewind { job, len } if *job == active => {
                self.files.output.rewind(*len);
                return true;
            }
            Event::Cut {
                job,
                source,
                output,
                ..
            } if *job == active => {
                self.files
                    .output
                    .cut(source.0..source.1, output.0..output.1);
                return true;
            }
            Event::Unit { job, index, result } if *job == active => {
                if let Some(batch) = &mut self.files.unit_translation
                    && let Some(slot) = batch.results.get_mut(*index)
                    && slot.is_none()
                {
                    *slot = Some(result.clone());
                    batch.completed += 1;
                    self.files.progress = Some((batch.completed, batch.results.len()));
                }
                cx.notify();
                return true;
            }
            Event::Finished { job, stats } if *job == active => {
                let output = std::mem::take(&mut self.files.output);
                let batch = self.files.unit_translation.take();
                if self.files.cancelling || stats.stop_reason == StopReason::Cancelled {
                    self.files.message = Some(t!("files.cancelled").into_owned());
                } else if stats.stop_reason == StopReason::Finished {
                    self.files.completed = match (&self.files.loaded, batch) {
                        (Some((_, FileDocument::Text(doc))), None) => output
                            .finish(doc.source())
                            .ok()
                            .filter(|(_, warnings)| warnings.len() == stats.cut)
                            .map(|(text, warnings)| CompletedFile {
                                has_changes: !text.trim().is_empty()
                                    && text.trim() != doc.source().trim(),
                                output: FileOutput::Text(text.into()),
                                review: FileReview {
                                    needs_review: !warnings.is_empty(),
                                    partial_translation: !warnings.is_empty(),
                                    locations: warnings
                                        .iter()
                                        .map(|n| t!("files.retained_line", number = n).into_owned())
                                        .collect(),
                                    ..Default::default()
                                },
                            }),
                        (Some((_, FileDocument::Markdown(doc))), Some(batch)) => batch
                            .text_targets()
                            .and_then(|targets| match doc.write(&targets) {
                                Ok(export) => Some(export),
                                Err(::files::FileError::NoTranslation) => {
                                    self.files.message = Some(t!("files.no_changes").into_owned());
                                    None
                                }
                                Err(_) => None,
                            })
                            .map(|export| CompletedFile {
                                has_changes: export.translated > 0,
                                output: FileOutput::Markdown(Arc::new(export.bytes)),
                                review: FileReview {
                                    needs_review: export.translated < doc.segments().len(),
                                    partial_translation: !export.retained_lines.is_empty(),
                                    locations: doc
                                        .segments()
                                        .iter()
                                        .map(|segment| {
                                            t!(
                                                if export.retained_lines.contains(&segment.line) {
                                                    "files.retained_line"
                                                } else {
                                                    "files.applied_line"
                                                },
                                                number = segment.line
                                            )
                                            .into_owned()
                                        })
                                        .chain(
                                            export
                                                .retained_lines
                                                .iter()
                                                .filter(|line| {
                                                    !doc.segments()
                                                        .iter()
                                                        .any(|segment| segment.line == **line)
                                                })
                                                .map(|line| {
                                                    t!("files.retained_line", number = line)
                                                        .into_owned()
                                                }),
                                        )
                                        .collect(),
                                    ..Default::default()
                                },
                            }),
                        (Some((_, FileDocument::Office { deck, .. })), Some(pptx)) => {
                            finish_office(deck, pptx, &self.files.target)
                        }
                        _ => None,
                    };
                }
                if self.files.completed.is_none() && self.files.message.is_none() {
                    self.files.message = Some(t!("files.translation_failed").into_owned());
                }
                self.archive_file(cx);
                self.jobs.finish_file(*job).unwrap_or(false)
            }
            Event::Failed { job, .. } if *job == active => {
                self.files.unit_translation = None;
                self.files.message = Some(t!("files.translation_failed").into_owned());
                self.jobs.finish_file(*job).unwrap_or(false)
            }
            Event::RuntimeFailed(_) => {
                self.files.unit_translation = None;
                self.files.cancelling = false;
                self.files.message = Some(t!("files.translation_failed").into_owned());
                self.jobs.finish_file(active);
                cx.notify();
                return false;
            }
            _ => return false,
        };
        self.files.cancelling = false;
        if resume {
            self.translate_now(window, cx);
        }
        cx.notify();
        true
    }

    pub(super) fn load_file_history(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async {
                    crate::file_history::load(&appdata::settings::data_file("file-history"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        // A translation may have finished while the directory was being scanned.
                        let known = this
                            .file_history
                            .iter()
                            .map(|entry| &entry.output)
                            .collect::<std::collections::HashSet<_>>();
                        let older = entries
                            .into_iter()
                            .filter(|entry| !known.contains(&entry.output))
                            .collect::<Vec<_>>();
                        this.file_history.extend(older);
                    }
                    Err(error) => {
                        this.files.message =
                            Some(t!("files.error", error = error.to_string()).into_owned())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn archive_file(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self
            .files
            .completed
            .as_ref()
            .filter(|result| result.has_changes)
        else {
            return;
        };
        let Some((source, document)) = self.files.loaded.clone() else {
            return;
        };
        let output = result.output.clone();
        let review = result.review.clone();
        let target = self.files.target.clone();
        self.files.io_busy = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let bytes = output.bytes(&document, &target)?;
                    crate::file_history::record(
                        &appdata::settings::data_file("file-history"),
                        &source,
                        target,
                        document.extension(),
                        &bytes,
                        review,
                    )
                    .map_err(::files::FileError::from)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    Ok(entry) => {
                        this.files = FilePanel::default();
                        this.file_history.retain(|old| old.output != entry.output);
                        this.file_history.insert(0, entry);
                    }
                    Err(error) => {
                        this.files.message =
                            Some(t!("files.history_failed", error = error.to_string()).into_owned())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn save_file(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        let Some((source, document)) = self.files.loaded.clone() else {
            return;
        };
        let Some(completed) = &self.files.completed else {
            return;
        };
        if !completed.has_changes {
            return;
        }
        let translation = completed.output.clone();
        let target = self.files.target.clone();
        let extension = document.extension();
        let suggested = format!(
            "{}.{target}.{extension}",
            source.file_stem().unwrap_or_default().to_string_lossy()
        );
        let picker =
            cx.prompt_for_new_path(source.parent().unwrap_or(Path::new(".")), Some(&suggested));
        self.files.io_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let selected = picker.await;
            let result = match selected {
                Ok(Ok(Some(destination))) => Some(
                    cx.background_spawn(async move {
                        let bytes = translation.bytes(&document, &target)?;
                        ::files::save_new(&source, &destination, &bytes)?;
                        Ok::<_, ::files::FileError>(destination)
                    })
                    .await,
                ),
                Ok(Err(error)) => Some(Err(::files::FileError::Io(std::io::Error::other(
                    error.to_string(),
                )))),
                _ => None,
            };
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    Some(Ok(path)) => {
                        this.files.history_status = Some(
                            t!("files.history_exported", path = path.display().to_string())
                                .into_owned(),
                        );
                        this.files.saved = Some(path);
                        this.files.message = None;
                    }
                    Some(Err(error)) => {
                        this.files.message =
                            Some(t!("files.error", error = error.to_string()).into_owned())
                    }
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        div()
            .id("file-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_5()
            .py_6()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(FILE_PANEL_WIDTH))
                    .mx_auto()
                    .gap_5()
                    .child(self.render_file_drop_zone(cx))
                    .when(
                        self.files.loaded.is_some() || !self.file_history.is_empty(),
                        |panel| panel.child(self.render_file_history(cx)),
                    )
                    .when_some(self.files.history_status.clone(), |panel, message| {
                        panel.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(message),
                        )
                    })
                    .when(self.files.io_busy, |panel| {
                        panel.child(
                            div()
                                .px_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("files.working").into_owned()),
                        )
                    })
                    .when_some(self.files.message.clone(), |panel, message| {
                        panel.child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .rounded_lg()
                                .border_1()
                                .border_color(theme.warning.opacity(0.42))
                                .bg(theme.warning.opacity(0.08))
                                .px_3()
                                .py_2()
                                .text_sm()
                                .child(Icon::new(Lucide::CircleAlert))
                                .child(div().min_w_0().child(message)),
                        )
                    }),
            )
            .into_any_element()
    }

    fn export_history_file(&mut self, path: PathBuf, suggested: &str, cx: &mut Context<Self>) {
        if self.file_operation_busy() {
            return;
        }
        let picker = cx.prompt_for_new_path(&appdata::settings::file_export_dir(), Some(suggested));
        self.files.io_busy = true;
        self.files.history_status = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = match picker.await {
                Ok(Ok(Some(destination))) => {
                    cx.background_spawn(async move {
                        ::files::save_new(&path, &destination, &std::fs::read(&path)?)?;
                        Ok::<_, ::files::FileError>(Some(destination))
                    })
                    .await
                }
                Ok(Err(error)) => Err(::files::FileError::Io(std::io::Error::other(
                    error.to_string(),
                ))),
                _ => Ok(None),
            };
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    Ok(Some(destination)) => {
                        this.files.message = None;
                        this.files.history_status = Some(
                            t!(
                                "files.history_exported",
                                path = destination.display().to_string()
                            )
                            .into_owned(),
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.files.message =
                            Some(t!("files.error", error = error.to_string()).into_owned())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_remove_history(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_operation_busy() || paths.is_empty() || window.has_active_dialog(cx) {
            return;
        }
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let paths = paths.clone();
            dialog
                .title(t!("files.history_delete_title").into_owned())
                .child(t!("files.history_delete_body", count = paths.len()).into_owned())
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("cancel-history-delete")
                                .label(t!("files.discard_keep").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-history-delete")
                                .danger()
                                .label(t!("files.history_delete").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = view.update(cx, |this, cx| {
                                        this.remove_history(paths.clone(), cx)
                                    });
                                }),
                        ),
                )
        });
    }

    fn remove_history(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.file_operation_busy() {
            return;
        }
        self.files.io_busy = true;
        self.files.history_status = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| {
                            let result = crate::file_history::remove(&path);
                            (path, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                for (path, result) in results {
                    match result {
                        Ok(()) => this.file_history.retain(|entry| entry.output != path),
                        Err(error) => {
                            this.files.message =
                                Some(t!("files.error", error = error.to_string()).into_owned())
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_file_history(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        // Use the same columns while reading the current file, translating it, and after
        // publishing the cached result. The archive replaces the in-memory row atomically.
        let rows = self.file_history.iter().enumerate().map(|(index, entry)| {
            let path = &entry.output;
            let open = path.clone();
            let export = path.clone();
            let remove = path.clone();
            let suggested = entry.export_name();
            let expanded = self.files.expanded_history.as_ref() == Some(path);
            let name = v_flex()
                .items_start()
                .min_w_0()
                .child(
                    Button::new(("history-result", index))
                        .small()
                        .ghost()
                        .min_w_0()
                        .max_w_full()
                        .overflow_hidden()
                        .label(entry.filename().to_owned())
                        .tooltip(t!("files.open_saved").into_owned())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if open
                                .symlink_metadata()
                                .is_ok_and(|metadata| metadata.is_file())
                            {
                                cx.open_with_system(&open);
                            } else {
                                this.file_history.retain(|entry| entry.output != open);
                                this.files.message = Some(t!("files.history_missing").into_owned());
                                cx.notify();
                            }
                        })),
                )
                .child(
                    div()
                        .pl_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(entry.completed_at()),
                );
            let status = if let Some(review) = entry.review() {
                let label = completion_label(review);
                let path = path.clone();
                Button::new(("history-review", index))
                    .small()
                    .ghost()
                    .icon(if expanded {
                        Lucide::ChevronDown
                    } else {
                        Lucide::ChevronRight
                    })
                    .label(label)
                    .tooltip(t!("files.show_review").into_owned())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.files.show_review = false;
                        this.files.expanded_history = (!expanded).then(|| path.clone());
                        cx.notify();
                    }))
                    .into_any_element()
            } else {
                div()
                    .pl_2()
                    .text_sm()
                    .child(t!("files.done").into_owned())
                    .into_any_element()
            };
            let actions = h_flex()
                .gap_1()
                .child(
                    Button::new(("history-export", index))
                        .small()
                        .outline()
                        .icon(Lucide::Download)
                        .label(t!("files.save").into_owned())
                        .disabled(self.file_operation_busy())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.export_history_file(export.clone(), &suggested, cx)
                        })),
                )
                .child(
                    Button::new(("history-remove", index))
                        .small()
                        .ghost()
                        .label(t!("files.history_delete").into_owned())
                        .disabled(self.file_operation_busy())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_remove_history(vec![remove.clone()], window, cx)
                        })),
                );
            v_flex()
                .child(self.file_row(
                    name,
                    status,
                    lang::language_name(entry.target()),
                    actions,
                    cx,
                ))
                .when_some(entry.review().filter(|_| expanded), |row, review| {
                    row.child(self.render_file_review(review, cx))
                })
        });
        let count = self.file_history.len() + usize::from(self.files.loaded.is_some());
        let heading = h_flex()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_semibold()
                    .child(t!("files.history_title").into_owned()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(count.to_string()),
            )
            .child(div().flex_1())
            .when(!self.file_history.is_empty(), |heading| {
                heading.child(
                    Button::new("history-clear")
                        .small()
                        .ghost()
                        .label(t!("files.history_clear").into_owned())
                        .disabled(self.file_operation_busy())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_remove_history(
                                this.file_history
                                    .iter()
                                    .map(|entry| entry.output.clone())
                                    .collect(),
                                window,
                                cx,
                            );
                        })),
                )
            });
        let columns = self
            .file_row(
                div().pl_2().child(t!("files.filename").into_owned()),
                div().pl_2().child(t!("files.status").into_owned()),
                t!("files.history_target").into_owned(),
                div(),
                cx,
            )
            .text_color(theme.muted_foreground)
            .bg(theme.muted.opacity(0.5));
        v_flex()
            .gap_3()
            .child(heading)
            .child(
                v_flex()
                    .rounded_lg()
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border)
                    .child(columns)
                    .when_some(self.files.loaded.as_ref(), |list, (path, doc)| {
                        list.child(self.current_file_row(path, doc, cx))
                    })
                    .children(rows),
            )
            .into_any_element()
    }

    fn file_row(
        &self,
        name: impl IntoElement,
        status: impl IntoElement,
        target: String,
        actions: impl IntoElement,
        cx: &Context<Self>,
    ) -> Div {
        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(div().flex_1().min_w_0().overflow_hidden().child(name))
            .child(h_flex().w(px(145.)).flex_none().child(status))
            .child(div().w(px(80.)).truncate().child(target))
            .child(
                h_flex()
                    .w(px(180.))
                    .flex_none()
                    .justify_end()
                    .child(actions),
            )
    }

    fn current_file_row(&self, path: &Path, doc: &FileDocument, cx: &Context<Self>) -> AnyElement {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let name = div().pl_2().font_medium().truncate().child(name);
        let encoding = match doc {
            FileDocument::Text(doc) => doc.converted_encoding,
            FileDocument::Markdown(doc) => doc.converted_encoding(),
            _ => None,
        };
        let mut actions = h_flex().gap_1();
        let mut status = v_flex().w_full().pl_2().gap_1();
        let target = if self.jobs.busy() || self.files.completed.is_some() {
            lang::language_name(&self.files.target)
        } else {
            lang::language_name(self.target_choice.resolve_for(doc.japanese()))
        };
        let mut review = None;
        if self.jobs.busy() {
            let (done, total) = self.files.progress.unwrap_or_default();
            let percent = done.saturating_mul(100).checked_div(total).unwrap_or(0);
            status = status
                .child(
                    if total == 0 {
                        t!("files.starting")
                    } else {
                        t!("files.progress", percent = percent)
                    }
                    .into_owned(),
                )
                .child(
                    Progress::new("file-progress")
                        .accessibility_label(t!("files.progress_label").into_owned())
                        .color(cx.theme().primary)
                        .loading(total == 0 || (total == 1 && done == 0))
                        .value(percent as f32),
                );
            actions = actions.child(
                Button::new("cancel-file")
                    .small()
                    .outline()
                    .label(t!("action.stop").into_owned())
                    .disabled(self.files.cancelling)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_file(cx))),
            );
        } else if let Some(result) = &self.files.completed {
            // Includes the fallback if the history directory cannot be written.
            status = status.child(
                Button::new("result-details")
                    .small()
                    .ghost()
                    .label(if result.has_changes {
                        completion_label(&result.review)
                    } else {
                        t!("files.unchanged").into_owned()
                    })
                    .icon(Lucide::ChevronDown)
                    .tooltip(t!("files.show_review").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.files.expanded_history = None;
                        this.files.show_review = !this.files.show_review;
                        cx.notify();
                    })),
            );
            review = self.files.show_review.then_some(&result.review);
            if result.has_changes {
                actions = actions.child(
                    Button::new("save-file")
                        .small()
                        .outline()
                        .icon(Lucide::Download)
                        .label(t!("files.save").into_owned())
                        .disabled(self.files.io_busy)
                        .on_click(cx.listener(|this, _, _, cx| this.save_file(cx))),
                );
            }
        } else {
            status = status.child(t!("files.ready").into_owned());
        }
        if !self.jobs.busy()
            && self
                .files
                .completed
                .as_ref()
                .is_none_or(|result| !result.has_changes)
        {
            actions = actions.child(
                Button::new("translate-file")
                    .small()
                    .primary()
                    .label(t!("action.translate").into_owned())
                    .disabled(doc.empty() || self.files.io_busy)
                    .on_click(cx.listener(|this, _, window, cx| this.start_file(window, cx))),
            );
        }
        v_flex()
            .child(self.file_row(name, status, target, actions, cx))
            .when_some(encoding, |row, encoding| {
                row.child(
                    div()
                        .px_4()
                        .text_sm()
                        .child(t!("files.encoding", encoding = encoding).into_owned()),
                )
            })
            .when_some(review, |row, review| {
                row.child(self.render_file_review(review, cx))
            })
            .into_any_element()
    }

    fn render_file_drop_zone(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let accent = theme.primary;
        let drop_enabled = self.current_job.is_none() && !self.file_operation_busy();
        let compact = self.files.loaded.is_some() || !self.file_history.is_empty();
        v_flex()
            .id("file-drop-zone")
            .items_center()
            .when(compact, |zone| zone.flex_row().py_5())
            .when(!compact, |zone| zone.justify_center().text_center().py_12())
            .gap_4()
            .px_5()
            .rounded_xl()
            .border_1()
            .border_dashed()
            .border_color(theme.muted_foreground.opacity(0.48))
            .bg(theme.muted.opacity(0.36))
            .when(drop_enabled, |zone| {
                zone.drag_over::<ExternalPaths>(move |style, paths, _, _| {
                    if paths.paths().len() == 1 {
                        style.border_color(accent).bg(accent.opacity(0.10))
                    } else {
                        style
                    }
                })
            })
            .child(
                h_flex()
                    .justify_center()
                    .flex_none()
                    .size(px(48.))
                    .rounded_xl()
                    .bg(accent.opacity(0.12))
                    .child(
                        Icon::new(Lucide::FileInput)
                            .size(px(24.))
                            .text_color(accent),
                    ),
            )
            .child(
                v_flex()
                    .when(compact, |copy| copy.flex_1())
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(t!("files.drop_prompt").into_owned()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Word · PowerPoint · Markdown · TXT"),
                    ),
            )
            .child(
                Button::new("choose-file")
                    .icon(Lucide::FolderOpen)
                    .label(t!("files.translate_file").into_owned())
                    .primary()
                    .disabled(self.file_operation_busy())
                    .on_click(cx.listener(|this, _, window, cx| this.open_file(window, cx))),
            )
            .into_any_element()
    }

    fn render_file_review(&self, review: &FileReview, cx: &Context<Self>) -> AnyElement {
        let guidance = [
            review
                .partial_translation
                .then(|| t!("files.partial_translation")),
            Some(t!("files.machine_translation")),
            review.update_toc.then(|| t!("files.update_toc")),
            review.check_slides.then(|| t!("files.check_slides")),
            review.check_word.then(|| t!("files.check_word")),
        ];
        div()
            .id("file-review-list")
            .max_h(scaled(240.))
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .child(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .children(guidance.into_iter().flatten().map(|line| {
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(line.into_owned())
                    }))
                    .children(
                        review
                            .locations
                            .iter()
                            .map(|line| div().child(line.clone())),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn allow_close(
        &mut self,
        quit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.jobs.busy() && !self.files.io_busy {
            if self.files.unsaved() {
                self.confirm_discard(AfterDiscard::Close { quit }, window, cx);
                return false;
            }
            return true;
        }
        if window.has_active_dialog(cx) {
            return false;
        }
        let view = cx.entity().downgrade();
        let io_busy = self.files.io_busy;
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title(t!("files.close_title").into_owned())
                .child(
                    t!(if io_busy {
                        "files.wait_for_io"
                    } else {
                        "files.close_body"
                    })
                    .into_owned(),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("continue-file")
                                .label(t!("files.continue").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .when(!io_busy, |footer| {
                            footer.child(
                                Button::new("cancel-close-file")
                                    .danger()
                                    .label(t!("files.cancel_close").into_owned())
                                    .on_click(move |_, window, cx| {
                                        let _ = view.update(cx, |this, _| this.worker.cancel());
                                        window.close_dialog(cx);
                                        if quit {
                                            cx.quit();
                                        } else {
                                            window.remove_window();
                                        }
                                    }),
                            )
                        }),
                )
        });
        false
    }
}

#[cfg(test)]
mod file_completion_tests {
    use super::{
        CompletedFile, FileOutput, FilePanel, FileReview, OfficeDocument, UnitResult,
        UnitTranslation, finish_office,
    };

    #[test]
    fn only_a_finished_translation_that_was_never_saved_counts_as_unsaved() {
        let completed = |has_changes| {
            Some(CompletedFile {
                output: FileOutput::Text("Translated.".into()),
                review: FileReview::default(),
                has_changes,
            })
        };
        let mut panel = FilePanel::default();
        assert!(!panel.unsaved());
        panel.completed = completed(false);
        assert!(!panel.unsaved());
        panel.completed = completed(true);
        assert!(panel.unsaved());
        panel.saved = Some("out.en.txt".into());
        assert!(!panel.unsaved());
    }

    #[test]
    fn office_completion_rejects_zero_changes_after_amount_validation() {
        let deck =
            OfficeDocument::open(include_bytes!("../../test-data/file-result.pptx").to_vec())
                .unwrap();
        for outcome in [
            UnitResult::Cut,
            UnitResult::Invalid,
            UnitResult::Complete {
                text: "売上高は100万円です。".into(),
                retried: false,
            },
            UnitResult::Complete {
                text: "Sales were 10 million yen.".into(),
                retried: false,
            },
        ] {
            let done = finish_office(
                &deck,
                UnitTranslation {
                    results: vec![Some(outcome)],
                    completed: 1,
                },
                "en",
            )
            .unwrap();
            assert!(!done.has_changes);
            assert!(done.review.partial_translation);
        }
        let done = finish_office(
            &deck,
            UnitTranslation {
                results: vec![Some(UnitResult::Complete {
                    text: "Sales were 1 million yen.".into(),
                    retried: false,
                })],
                completed: 1,
            },
            "en",
        )
        .unwrap();
        assert!(done.has_changes);
        assert!(!done.review.partial_translation);
        let incomplete = finish_office(&deck, UnitTranslation::new(1), "en");
        assert!(incomplete.is_none());
    }

    #[test]
    fn retained_chart_text_prompts_review_but_slide_templates_do_not() {
        use ::files::office::{RetainedContent, RetentionReason};

        let mut deck =
            OfficeDocument::open(include_bytes!("../../test-data/file-result.pptx").to_vec())
                .unwrap();
        for (part, partial) in [
            ("ppt/slideLayouts/slideLayout1.xml", false),
            ("ppt/charts/chart1.xml", true),
            ("ppt/diagrams/data1.xml", true),
        ] {
            deck.unsupported = vec![RetainedContent {
                part: part.into(),
                paragraph: None,
                in_table: false,
                count: 1,
                source: String::new(),
                reason: RetentionReason::UnsupportedPart,
            }];
            let result = finish_office(
                &deck,
                UnitTranslation {
                    results: vec![Some(UnitResult::Complete {
                        text: "Sales were 1 million yen.".into(),
                        retried: false,
                    })],
                    completed: 1,
                },
                "en",
            )
            .unwrap();
            assert_eq!(result.review.partial_translation, partial, "{part}");
        }
    }
}
