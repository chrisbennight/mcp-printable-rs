//! A time limit and cancellation for geometry work.
//!
//! Some analyses query a bounding-volume hierarchy once per surface sample,
//! and a crafted mesh can make every query visit every face. The caller
//! applies a [`WorkLimit`] around the work, and those loops call [`check`]
//! once per sample, so such work stops soon after its time runs out or its
//! caller gives up.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::GeometryError;

/// When geometry work must stop.
#[derive(Clone)]
pub struct WorkLimit {
    time_limit: Duration,
    deadline: Option<Instant>,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl WorkLimit {
    /// Work may run for `time_limit` from now, and stops early once
    /// `cancelled` returns true.
    pub fn new(time_limit: Duration, cancelled: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            time_limit,
            deadline: Instant::now().checked_add(time_limit),
            cancelled: Arc::new(cancelled),
        }
    }

    /// An error once the work must stop.
    pub fn check(&self) -> Result<(), GeometryError> {
        if (self.cancelled)() {
            return Err(GeometryError::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(GeometryError::TimeLimit {
                seconds: self.time_limit.as_secs_f64(),
            });
        }
        Ok(())
    }

    /// Run `work` on this thread with [`check`] enforcing this limit.
    pub fn apply<T>(&self, work: impl FnOnce() -> T) -> T {
        struct Restore(Option<WorkLimit>);
        impl Drop for Restore {
            fn drop(&mut self) {
                ACTIVE.with(|active| *active.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(ACTIVE.with(|active| active.replace(Some(self.clone()))));
        work()
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<WorkLimit>> = const { RefCell::new(None) };
}

/// An error once the limit applied to this thread has passed or its work
/// was cancelled. Work with no limit applied always continues.
pub fn check() -> Result<(), GeometryError> {
    ACTIVE.with(|active| active.borrow().as_ref().map_or(Ok(()), WorkLimit::check))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_limit_applies_only_inside_its_work() {
        assert!(check().is_ok());
        let expired = WorkLimit::new(Duration::ZERO, || false);
        let inside = expired.apply(check);
        assert!(matches!(inside, Err(GeometryError::TimeLimit { .. })));
        assert!(check().is_ok());
        let unbounded = WorkLimit::new(Duration::MAX, || false);
        assert!(unbounded.apply(check).is_ok());
    }

    #[test]
    fn cancellation_stops_work_before_its_time_runs_out() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let limit = WorkLimit::new(Duration::from_secs(3600), move || {
            flag.load(Ordering::Relaxed)
        });
        limit.apply(|| {
            assert!(check().is_ok());
            cancelled.store(true, Ordering::Relaxed);
            assert!(matches!(check(), Err(GeometryError::Cancelled)));
        });
    }

    #[test]
    fn the_outer_limit_returns_after_nested_work() {
        let outer = WorkLimit::new(Duration::ZERO, || false);
        let inner = WorkLimit::new(Duration::MAX, || false);
        outer.apply(|| {
            assert!(inner.apply(check).is_ok());
            assert!(check().is_err());
        });
    }
}
