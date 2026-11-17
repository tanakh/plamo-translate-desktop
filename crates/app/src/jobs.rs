//! Ownership of the app's one inference worker. File jobs cannot be superseded by typing.

#[derive(Default)]
pub struct Jobs {
    file: Option<u64>,
    deferred_text: bool,
}

impl Jobs {
    pub fn file(&self) -> Option<u64> {
        self.file
    }
    pub fn busy(&self) -> bool {
        self.file.is_some()
    }
    pub fn start_file(&mut self, job: u64) {
        self.file = Some(job);
    }
    pub fn defer_text(&mut self) -> bool {
        if self.busy() {
            self.deferred_text = true;
            true
        } else {
            false
        }
    }
    /// A stale completion must neither release a new job nor run its deferred text.
    pub fn finish_file(&mut self, job: u64) -> Option<bool> {
        if self.file != Some(job) {
            return None;
        }
        self.file = None;
        Some(std::mem::take(&mut self.deferred_text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_completion_does_not_release_the_worker() {
        let mut jobs = Jobs::default();
        jobs.start_file(3);
        assert!(jobs.defer_text());
        assert!(jobs.defer_text());
        assert_eq!(jobs.finish_file(2), None);
        assert!(jobs.busy());
        assert_eq!(jobs.finish_file(3), Some(true));
        assert!(!jobs.busy());
        assert!(!jobs.defer_text());
    }
}
