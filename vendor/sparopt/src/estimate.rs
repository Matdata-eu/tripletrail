//! tripletrail: cardinality estimates from the data, for the join order.
//!
//! The optimizer guesses the size of a triple pattern from which of its positions are bound
//! (1,000 rows for `?s :p :o`, 100 for `?s :p ?o` with `?o` bound...). A store that knows its
//! data can install an [`Estimator`] for the current thread with [`with_estimator`]; triple
//! patterns with a constant predicate are then estimated by it, others as before.

use oxrdf::NamedNode;
use spargebra::term::GroundTermPattern;
use std::cell::RefCell;
use std::rc::Rc;

/// A position of a triple pattern, for [`Estimator::triples`].
#[derive(Clone, Copy, Debug)]
pub enum Slot<'a> {
    /// A constant term.
    Constant(&'a GroundTermPattern),
    /// A variable bound before the pattern is evaluated (a join key of a nested-loop join).
    Bound,
    /// A variable the pattern binds.
    Free,
}

/// Estimates how many rows a triple pattern with a constant predicate gives.
pub trait Estimator {
    /// The rows of `subject predicate object`, or `None` to keep the optimizer's guess. With a
    /// [`Slot::Bound`] position, the rows for one value of it (on average over the rows that
    /// bind it).
    fn triples(&self, subject: Slot<'_>, predicate: &NamedNode, object: Slot<'_>) -> Option<usize>;
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<dyn Estimator>>> = const { RefCell::new(None) };
}

/// Runs `f` with `estimator` answering the triple-pattern estimates of the optimizations it
/// makes on this thread.
pub fn with_estimator<R>(estimator: Rc<dyn Estimator>, f: impl FnOnce() -> R) -> R {
    let _restore = Restore(CURRENT.with(|c| c.borrow_mut().replace(estimator)));
    f()
}

pub(crate) fn triples(subject: Slot<'_>, predicate: &NamedNode, object: Slot<'_>) -> Option<usize> {
    let estimator = CURRENT.with(|c| c.borrow().clone())?;
    estimator.triples(subject, predicate, object)
}

/// Whether an estimator is installed on this thread.
pub(crate) fn active() -> bool {
    CURRENT.with(|c| c.borrow().is_some())
}

/// Runs `f` with no estimator (for patterns the store does not answer, such as the inside of
/// a `SERVICE`).
pub(crate) fn without<R>(f: impl FnOnce() -> R) -> R {
    let _restore = Restore(CURRENT.with(|c| c.borrow_mut().take()));
    f()
}

thread_local! {
    static GIVE_UP: RefCell<Option<Rc<dyn Fn() -> bool>>> = const { RefCell::new(None) };
}

/// Runs `f` with `give_up` asked, during the optimizations it makes on this thread, whether
/// to stop looking for a better join order (the query's timeout passed, #483): the parts left
/// are then joined in the order they come, and the evaluation that follows sees the timeout.
pub fn with_give_up<R>(give_up: Rc<dyn Fn() -> bool>, f: impl FnOnce() -> R) -> R {
    let previous = GIVE_UP.with(|g| g.borrow_mut().replace(give_up));
    let _restore = RestoreGiveUp(previous);
    f()
}

/// Whether the optimizer should stop looking for a better join order.
pub(crate) fn given_up() -> bool {
    GIVE_UP.with(|g| g.borrow().as_ref().is_some_and(|f| f()))
}

struct RestoreGiveUp(Option<Rc<dyn Fn() -> bool>>);

impl Drop for RestoreGiveUp {
    fn drop(&mut self) {
        let previous = self.0.take();
        GIVE_UP.with(|g| *g.borrow_mut() = previous);
    }
}

/// Puts back the estimator it holds when dropped (also on a panic).
struct Restore(Option<Rc<dyn Estimator>>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}
