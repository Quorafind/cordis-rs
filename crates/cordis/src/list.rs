//! `DisposableList`: insertion-ordered, removable by the handle `push` answers, cleared in
//! reverse — the order teardown runs in.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// An ordered list whose entries can be removed by serial number.
pub struct DisposableList<T> {
    next: Cell<u64>,
    items: RefCell<Vec<(u64, T)>>,
}

impl<T> Default for DisposableList<T> {
    fn default() -> Self {
        Self {
            next: Cell::new(0),
            items: RefCell::new(Vec::new()),
        }
    }
}

impl<T: Clone> DisposableList<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.borrow().is_empty()
    }

    /// Adds `value` at the end; answers its serial number.
    pub fn push(&self, value: T) -> u64 {
        let sn = self.next.get() + 1;
        self.next.set(sn);
        self.items.borrow_mut().push((sn, value));
        sn
    }

    /// Adds `value` at the front (it is cleared last).
    pub fn unshift(&self, value: T) -> u64 {
        let sn = self.next.get() + 1;
        self.next.set(sn);
        self.items.borrow_mut().insert(0, (sn, value));
        sn
    }

    /// Removes the entry `push` numbered `sn`; answers whether there was one.
    pub fn remove(&self, sn: u64) -> bool {
        let mut items = self.items.borrow_mut();
        match items.iter().position(|(s, _)| *s == sn) {
            Some(at) => {
                items.remove(at);
                true
            }
            None => false,
        }
    }

    /// Removes the first entry `matches` accepts; answers whether there was one.
    pub fn remove_where(&self, matches: impl Fn(&T) -> bool) -> bool {
        let mut items = self.items.borrow_mut();
        match items.iter().position(|(_, v)| matches(v)) {
            Some(at) => {
                items.remove(at);
                true
            }
            None => false,
        }
    }

    /// Empties the list, answering what it held last-first.
    pub fn clear(&self) -> Vec<T> {
        let items = std::mem::take(&mut *self.items.borrow_mut());
        items.into_iter().rev().map(|(_, v)| v).collect()
    }

    /// What it holds, first-first.
    pub fn values(&self) -> Vec<T> {
        self.items.borrow().iter().map(|(_, v)| v.clone()).collect()
    }
}

/// A [`DisposableList`] behind an `Rc`, so a remover can outlive the borrow that made it.
pub type SharedList<T> = Rc<DisposableList<T>>;
