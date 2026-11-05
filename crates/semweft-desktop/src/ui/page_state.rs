use crate::client::Cancellation;
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct PageState<T> {
    pub result: T,
    pub busy: bool,
    pub error: String,
    pub generation: u64,
    pub cancellation: Cancellation,
    seen: BTreeSet<String>,
}

impl<T> PageState<T> {
    pub fn invalidate(&mut self) {
        self.cancellation.cancel();
        self.cancellation = Cancellation::default();
        self.generation += 1;
        self.busy = false;
    }

    pub fn begin(&mut self) -> u64 {
        self.seen.clear();
        self.start()
    }

    pub fn start(&mut self) -> u64 {
        self.invalidate();
        self.busy = true;
        self.error.clear();
        self.generation
    }

    pub fn finish(&mut self, generation: u64) -> bool {
        if self.generation != generation || !self.busy {
            return false;
        }
        self.busy = false;
        true
    }

    pub fn cancel(&mut self, message: &str) {
        self.invalidate();
        self.error = message.into();
    }

    pub fn next_cursor(&mut self, cursor: Option<String>, message: &str) -> Option<String> {
        if self.busy {
            return None;
        }
        let cursor = cursor?;
        if !self.seen.insert(cursor.clone()) {
            self.error = message.into();
            return None;
        }
        Some(cursor)
    }
}

impl<T: Default> PageState<T> {
    pub fn reset(&mut self) {
        self.invalidate();
        self.result = T::default();
        self.error.clear();
        self.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_request_rejects_stale_and_duplicate_completions() {
        let mut state = PageState::<Vec<String>>::default();
        let previous = state.begin();
        let previous_cancellation = state.cancellation.clone();
        let current = state.begin();
        assert!(previous_cancellation.check().is_err());
        assert!(!state.finish(previous));
        assert!(state.busy);
        assert!(state.finish(current));
        assert!(!state.busy);
        assert!(!state.finish(current));
    }

    #[test]
    fn cancel_preserves_results_and_allows_another_request() {
        let mut state = PageState::<Vec<String>>::default();
        let generation = state.begin();
        let cancellation = state.cancellation.clone();
        state.result.push("loaded evidence".into());
        state.cancel("cancelled");
        assert!(cancellation.check().is_err());
        assert_eq!(state.result, ["loaded evidence"]);
        assert_eq!(state.error, "cancelled");
        assert!(!state.finish(generation));
        let resumed = state.start();
        assert!(state.cancellation.check().is_ok());
        assert!(state.error.is_empty());
        assert!(state.finish(resumed));
    }

    #[test]
    fn cursors_are_guarded_across_pages_and_cleared_for_new_queries() {
        let mut state = PageState::<Vec<String>>::default();
        let cursor = Some("next".into());
        let generation = state.begin();
        assert_eq!(state.next_cursor(cursor.clone(), "repeated"), None);
        assert!(state.finish(generation));
        assert_eq!(state.next_cursor(cursor.clone(), "repeated"), cursor);
        let generation = state.start();
        assert!(state.finish(generation));
        assert_eq!(state.next_cursor(cursor.clone(), "repeated"), None);
        assert_eq!(state.error, "repeated");
        let generation = state.begin();
        assert!(state.finish(generation));
        assert_eq!(state.next_cursor(cursor.clone(), "repeated"), cursor);
        assert_eq!(state.next_cursor(None, "repeated"), None);
    }

    #[test]
    fn reset_discards_results_and_invalidates_inflight_request() {
        let mut state = PageState::<Vec<String>>::default();
        let generation = state.begin();
        let cancellation = state.cancellation.clone();
        state.result.push("old workspace".into());
        state.error = "old error".into();
        state.reset();
        assert!(cancellation.check().is_err());
        assert!(!state.finish(generation));
        assert!(!state.busy);
        assert!(state.result.is_empty());
        assert!(state.error.is_empty());
        assert!(state.cancellation.check().is_ok());
    }
}
