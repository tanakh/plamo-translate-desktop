//! Local copies of finished file translations. Publish the metadata after the document.
use chrono::{DateTime, Local, Utc};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct FileReview {
    pub needs_review: bool,
    pub partial_translation: bool,
    pub update_toc: bool,
    pub check_slides: bool,
    pub check_word: bool,
    pub locations: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Metadata {
    filename: String,
    target: String,
    extension: String,
    created: i64,
    #[serde(default)]
    review: Option<FileReview>,
}

pub(crate) struct Entry {
    metadata: Metadata,
    pub output: PathBuf,
}

impl Entry {
    pub fn filename(&self) -> &str {
        &self.metadata.filename
    }

    pub fn review(&self) -> Option<&FileReview> {
        self.metadata.review.as_ref()
    }

    pub fn target(&self) -> &str {
        &self.metadata.target
    }

    pub fn export_name(&self) -> String {
        let stem = Path::new(self.filename())
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let target = &self.metadata.target;
        let extension = &self.metadata.extension;
        format!("{stem}.{target}.{extension}")
    }

    pub fn completed_at(&self) -> String {
        DateTime::from_timestamp_micros(self.metadata.created)
            .map(|date| {
                date.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default()
    }

    fn from_dir(dir: &Path, metadata: Metadata) -> io::Result<Self> {
        if !matches!(metadata.extension.as_str(), "txt" | "md" | "docx" | "pptx") {
            return Err(io::Error::other("Unsupported history file type"));
        }
        if metadata.target.is_empty()
            || !metadata
                .target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_()".contains(&b))
        {
            return Err(io::Error::other("Invalid history language code"));
        }
        let stem = Path::new(&metadata.filename)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let output = dir.join(format!("{stem}.translated.{}", metadata.extension));
        Ok(Self { metadata, output })
    }
}

pub(crate) fn load(root: &Path) -> io::Result<Vec<Entry>> {
    let dirs = match fs::read_dir(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result?,
    };
    let mut entries = Vec::new();
    for dir in dirs {
        let dir = dir?;
        if !dir.file_type()?.is_dir()
            || !dir
                .file_name()
                .to_string_lossy()
                .starts_with("translation-")
        {
            continue;
        }
        let dir = dir.path();
        let Ok(bytes) = fs::read(dir.join("entry.json")) else {
            continue;
        };
        if let Ok(metadata) = serde_json::from_slice(&bytes)
            && let Ok(entry) = Entry::from_dir(&dir, metadata)
            && entry.output.symlink_metadata().is_ok_and(|m| m.is_file())
        {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.metadata.created));
    Ok(entries)
}

pub(crate) fn record(
    root: &Path,
    source: &Path,
    target: String,
    extension: &str,
    bytes: &[u8],
    review: FileReview,
) -> io::Result<Entry> {
    fs::create_dir_all(root)?;
    let dir = tempfile::Builder::new()
        .prefix("translation-")
        .tempdir_in(root)?;
    let entry = Entry::from_dir(
        dir.path(),
        Metadata {
            filename: source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            target,
            extension: extension.to_owned(),
            review: Some(review),
            created: Utc::now().timestamp_micros(),
        },
    )?;
    files::save_new(source, &entry.output, bytes).map_err(io::Error::other)?;
    files::save_new(
        source,
        &dir.path().join("entry.json"),
        &serde_json::to_vec(&entry.metadata)?,
    )
    .map_err(io::Error::other)?;
    let _ = dir.keep();
    Ok(entry)
}

/// Delete only the cached result owned by this entry.
pub(crate) fn remove(output: &Path) -> io::Result<()> {
    let dir = output
        .parent()
        .ok_or_else(|| io::Error::other("Missing history directory"))?;
    if !dir.symlink_metadata()?.is_dir() {
        return Err(io::Error::other("Invalid history directory"));
    }
    match fs::remove_file(output) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    // Without the output this entry is already gone. Leave unexpected files alone.
    let _ = fs::remove_file(dir.join("entry.json"));
    let _ = fs::remove_dir(dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_named_results_survive_reloading_without_the_original() {
        let root = tempfile::tempdir().unwrap();
        let source = Path::new("example.txt");
        let first = record(
            root.path(),
            source,
            "ja".into(),
            "txt",
            b"first",
            FileReview::default(),
        )
        .unwrap();
        let second = record(
            root.path(),
            source,
            "en".into(),
            "txt",
            b"second",
            FileReview::default(),
        )
        .unwrap();
        let entries = load(root.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].output, second.output);
        assert_eq!(fs::read(&entries[1].output).unwrap(), b"first");
        assert_eq!(entries[1].output, first.output);
        assert_eq!(entries[1].filename(), "example.txt");
    }

    #[test]
    fn review_survives_restart_and_older_entries_still_open() {
        let root = tempfile::tempdir().unwrap();
        let entry = record(
            root.path(),
            Path::new("report.txt"),
            "en".into(),
            "txt",
            b"result",
            FileReview {
                needs_review: true,
                locations: vec!["Line 3".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let entries = load(root.path()).unwrap();
        let review = entries[0].review().unwrap();
        assert!(review.needs_review);
        assert_eq!(review.locations, ["Line 3"]);
        let metadata_path = entry.output.parent().unwrap().join("entry.json");
        let mut metadata = serde_json::to_value(&entry.metadata).unwrap();
        metadata.as_object_mut().unwrap().remove("review");
        fs::write(metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let entries = load(root.path()).unwrap();
        assert!(entries[0].review().is_none());
        assert_eq!(entries[0].output, entry.output);
    }

    #[test]
    fn history_removal_preserves_the_source_and_exported_copy() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("example.txt");
        let exported = root.path().join("example.en.txt");
        fs::write(&source, b"source").unwrap();
        let entry = record(
            root.path(),
            &source,
            "en".into(),
            "txt",
            b"result",
            FileReview::default(),
        )
        .unwrap();
        assert_eq!(entry.export_name(), "example.en.txt");
        fs::copy(&entry.output, &exported).unwrap();
        remove(&entry.output).unwrap();
        assert!(!entry.output.exists());
        assert!(load(root.path()).unwrap().is_empty());
        assert_eq!(fs::read(source).unwrap(), b"source");
        assert_eq!(fs::read(exported).unwrap(), b"result");
    }

    #[test]
    fn invalid_archive_extensions_are_not_loaded() {
        let root = tempfile::tempdir().unwrap();
        let entry = record(
            root.path(),
            Path::new("example.txt"),
            "en".into(),
            "txt",
            b"result",
            FileReview::default(),
        )
        .unwrap();
        let dir = entry.output.parent().unwrap();
        let mut metadata = entry.metadata;
        metadata.extension = "command".into();
        fs::write(
            dir.join("entry.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        fs::write(dir.join("example.translated.command"), b"result").unwrap();
        assert!(load(root.path()).unwrap().is_empty());
    }
}
