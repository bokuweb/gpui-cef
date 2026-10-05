//! Requests from the page that wait on the application's answer.
//!
//! A JavaScript dialog, a file chooser and a download each come with a CEF
//! callback that has to be called exactly once, possibly long after the
//! handler returned. They wait here under an id the application answers by.
//! Whatever is still waiting when CEF says the page has moved on, or when the
//! webview goes away, is cancelled rather than left hanging — an unanswered
//! `alert()` keeps the page frozen.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

/// A request that has to be answered exactly once.
pub(crate) trait Cancel {
    /// Answers "no": dismisses the dialog, abandons the choice, cancels the
    /// download.
    fn cancel(self);
}

/// Requests waiting on the application, by id.
pub(crate) struct Pending<T: Cancel> {
    next_id: Cell<u64>,
    waiting: RefCell<BTreeMap<u64, T>>,
}

impl<T: Cancel> Default for Pending<T> {
    fn default() -> Self {
        Self {
            next_id: Cell::new(1),
            waiting: RefCell::default(),
        }
    }
}

impl<T: Cancel> Pending<T> {
    /// Keeps `request` under a fresh id. Ids start at 1 and are never reused.
    pub(crate) fn insert(&self, request: T) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.waiting.borrow_mut().insert(id, request);
        id
    }

    /// Takes the request waiting under `id`, for the caller to answer.
    pub(crate) fn take(&self, id: u64) -> Option<T> {
        self.waiting.borrow_mut().remove(&id)
    }

    /// Cancels everything still waiting, oldest first.
    pub(crate) fn cancel_all(&self) {
        // Emptied before anything is called: a cancellation that calls straight
        // back in has to find the map consistent and unborrowed.
        let waiting = std::mem::take(&mut *self.waiting.borrow_mut());
        for request in waiting.into_values() {
            request.cancel();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.waiting.borrow().len()
    }
}

impl<T: Cancel> Drop for Pending<T> {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// A download waiting to be told where to go. Dropping it unanswered cancels
/// it, as CEF does with its callback.
pub(crate) trait Proceed {
    /// Starts writing it to `path`.
    fn proceed(self, path: &std::path::Path);
}

/// A download under way, which can still be stopped.
pub(crate) trait Abort {
    fn abort(&self);
}

struct Download<D, C> {
    /// Where to go, until the application says.
    decision: Option<D>,
    /// How to stop it, from CEF's latest progress report.
    control: Option<C>,
    /// Whether the application has heard of it. Progress on a download it has
    /// not been asked about yet is not reported.
    announced: bool,
    /// Turned down: stop it at the next chance.
    canceled: bool,
}

impl<D, C> Default for Download<D, C> {
    fn default() -> Self {
        Self {
            decision: None,
            control: None,
            announced: false,
            canceled: false,
        }
    }
}

/// The page's downloads, by CEF's id.
///
/// CEF reports a download in two ways that can come in either order: once to
/// ask where it goes (`on_before_download`, answered at most once) and
/// repeatedly with its progress (`on_download_updated`, which also carries the
/// way to cancel it). This keeps both until the download finishes.
pub(crate) struct Downloads<D, C> {
    entries: RefCell<BTreeMap<u64, Download<D, C>>>,
}

impl<D, C> Default for Downloads<D, C> {
    fn default() -> Self {
        Self {
            entries: RefCell::default(),
        }
    }
}

impl<D: Proceed, C: Abort> Downloads<D, C> {
    /// CEF asks where a download goes. From here on its progress is reported.
    pub(crate) fn request(&self, id: u64, decision: D) {
        let mut entries = self.entries.borrow_mut();
        let entry = entries.entry(id).or_default();
        entry.announced = true;
        // A second question about the same download replaces the first, which
        // is dropped (cancelled) here.
        entry.decision = Some(decision);
    }

    /// Turns a download down without asking anyone — the webview is gone.
    pub(crate) fn reject(&self, id: u64) {
        self.entries.borrow_mut().entry(id).or_default().canceled = true;
    }

    /// CEF reports progress. Returns whether to pass it on to the application.
    pub(crate) fn update(&self, id: u64, control: C, finished: bool) -> bool {
        let (announced, canceled) = {
            let mut entries = self.entries.borrow_mut();
            if finished {
                return entries.remove(&id).is_some_and(|entry| entry.announced);
            }
            let entry = entries.entry(id).or_default();
            entry.control = Some(control);
            (entry.announced, entry.canceled)
        };
        if canceled {
            self.with_control(id, Abort::abort);
        }
        announced
    }

    /// The application's answer: a path to write to, or `None` to cancel.
    /// `false` when the download is not waiting for one.
    pub(crate) fn resolve(&self, id: u64, path: Option<&std::path::Path>) -> bool {
        let decision = {
            let mut entries = self.entries.borrow_mut();
            let Some(entry) = entries.get_mut(&id) else {
                return false;
            };
            let Some(decision) = entry.decision.take() else {
                return false;
            };
            entry.canceled = path.is_none();
            decision
        };
        match path {
            Some(path) => decision.proceed(path),
            None => {
                drop(decision);
                self.with_control(id, Abort::abort);
            }
        }
        true
    }

    /// Cancels every download still waiting for an answer. Those already
    /// under way carry on.
    pub(crate) fn cancel_waiting(&self) {
        let waiting: Vec<u64> = self
            .entries
            .borrow()
            .iter()
            .filter(|(_, entry)| entry.decision.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in waiting {
            self.resolve(id, None);
        }
    }

    fn with_control(&self, id: u64, f: impl FnOnce(&C)) {
        // Taken out for the call, so nothing is borrowed while CEF runs, and
        // put back after unless a newer one arrived meanwhile.
        let control = self
            .entries
            .borrow_mut()
            .get_mut(&id)
            .and_then(|entry| entry.control.take());
        if let Some(control) = control {
            f(&control);
            if let Some(entry) = self.entries.borrow_mut().get_mut(&id) {
                entry.control.get_or_insert(control);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    /// Records which requests were cancelled.
    struct Request {
        name: &'static str,
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Cancel for Request {
        fn cancel(self) {
            self.log.borrow_mut().push(self.name);
        }
    }

    fn request(name: &'static str, log: &Rc<RefCell<Vec<&'static str>>>) -> Request {
        Request {
            name,
            log: log.clone(),
        }
    }

    #[test]
    fn ids_are_fresh_and_answer_once() {
        let log = Rc::default();
        let pending = Pending::default();
        let first = pending.insert(request("first", &log));
        let second = pending.insert(request("second", &log));
        assert_eq!((first, second), (1, 2));

        // Answering takes it out, so a second answer finds nothing.
        assert_eq!(pending.take(first).map(|r| r.name), Some("first"));
        assert!(pending.take(first).is_none());
        // An id that never existed is not an error either.
        assert!(pending.take(99).is_none());
        assert_eq!(pending.len(), 1);
        assert!(log.borrow().is_empty(), "nothing was cancelled");

        // Ids are not reused once one is free.
        assert_eq!(pending.insert(request("third", &log)), 3);
    }

    #[test]
    fn whatever_is_left_is_cancelled_in_order() {
        let log = Rc::default();
        let pending = Pending::default();
        pending.insert(request("first", &log));
        let answered = pending.insert(request("answered", &log));
        pending.insert(request("last", &log));
        pending.take(answered);

        pending.cancel_all();
        assert_eq!(*log.borrow(), ["first", "last"]);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn dropping_the_store_cancels_what_is_waiting() {
        let log = Rc::default();
        {
            let pending = Pending::default();
            pending.insert(request("dialog", &log));
        }
        // Left alone, the page would wait for an answer forever.
        assert_eq!(*log.borrow(), ["dialog"]);
    }

    /// Logs what happens to a download's decision and control.
    struct Decision {
        log: Rc<RefCell<Vec<String>>>,
        answered: bool,
    }

    impl Proceed for Decision {
        fn proceed(mut self, path: &std::path::Path) {
            self.answered = true;
            self.log
                .borrow_mut()
                .push(format!("proceed {}", path.display()));
        }
    }

    impl Drop for Decision {
        fn drop(&mut self) {
            if !self.answered {
                self.log.borrow_mut().push("dropped".into());
            }
        }
    }

    type Log = Rc<RefCell<Vec<String>>>;

    struct Control(Log);

    impl Abort for Control {
        fn abort(&self) {
            self.0.borrow_mut().push("abort".into());
        }
    }

    fn decision(log: &Rc<RefCell<Vec<String>>>) -> Decision {
        Decision {
            log: log.clone(),
            answered: false,
        }
    }

    #[test]
    fn a_download_goes_where_the_application_says() {
        let log: Log = Rc::default();
        let downloads = Downloads::default();
        // CEF may report progress before it asks; the application has not
        // heard of the download yet, so that is not passed on.
        assert!(!downloads.update(4, Control(log.clone()), false));

        downloads.request(4, decision(&log));
        assert!(downloads.update(4, Control(log.clone()), false));
        assert!(downloads.resolve(4, Some("/tmp/report.docx".as_ref())));
        assert_eq!(*log.borrow(), ["proceed /tmp/report.docx"]);

        // Answered once; the second answer finds nothing to answer.
        assert!(!downloads.resolve(4, None));
        // Finishing is reported, and forgets the download.
        assert!(downloads.update(4, Control(log.clone()), true));
        assert!(!downloads.update(4, Control(log.clone()), true));
        assert!(!downloads.resolve(4, Some("/tmp/again".as_ref())));
    }

    #[test]
    fn declining_cancels_now_and_at_every_report_after() {
        let log: Log = Rc::default();
        let downloads = Downloads::default();
        downloads.request(1, decision(&log));
        downloads.update(1, Control(log.clone()), false);

        assert!(downloads.resolve(1, None));
        // The question is dropped unanswered, which is CEF's cancel, and the
        // download is stopped through its progress callback too.
        assert_eq!(*log.borrow(), ["dropped", "abort"]);

        // Should it carry on regardless, the next report stops it again.
        assert!(downloads.update(1, Control(log.clone()), false));
        assert_eq!(log.borrow().last().map(String::as_str), Some("abort"));
        // And its end still reaches the application.
        assert!(downloads.update(1, Control(log.clone()), true));
    }

    #[test]
    fn closing_cancels_only_downloads_still_waiting() {
        let log: Log = Rc::default();
        let downloads = Downloads::default();
        downloads.request(1, decision(&log));
        downloads.request(2, decision(&log));
        downloads.resolve(2, Some("/tmp/kept".as_ref()));
        log.borrow_mut().clear();

        downloads.cancel_waiting();
        assert_eq!(*log.borrow(), ["dropped"]);
        // The one under way is left to finish.
        assert!(downloads.update(2, Control(log.clone()), false));
        assert_eq!(*log.borrow(), ["dropped"]);
    }

    #[test]
    fn a_rejected_download_is_stopped_without_being_reported() {
        let log: Log = Rc::default();
        let downloads = Downloads::<Decision, Control>::default();
        downloads.reject(9);
        assert!(!downloads.update(9, Control(log.clone()), false));
        assert_eq!(*log.borrow(), ["abort"]);
        assert!(!downloads.update(9, Control(log.clone()), true));
    }

    /// A cancellation that reaches back into the store, as CEF may when a
    /// callback runs synchronously.
    struct Reentrant {
        pending: Rc<Pending<Reentrant>>,
    }

    impl Cancel for Reentrant {
        fn cancel(self) {
            assert!(self.pending.take(1).is_none());
        }
    }

    #[test]
    fn cancelling_does_not_hold_the_store_borrowed() {
        let pending = Rc::new(Pending::default());
        pending.insert(Reentrant {
            pending: pending.clone(),
        });
        pending.cancel_all();
    }
}
