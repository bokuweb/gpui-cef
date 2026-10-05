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
