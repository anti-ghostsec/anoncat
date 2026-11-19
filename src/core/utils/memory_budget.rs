//! Memory set aside for request and response bodies too big to be held
//! without a limit: uploads above `max_request_size` and files downloaded from
//! other servers.
//!
//! Each body takes room as its bytes arrive and gives it back when its
//! `Reservation` is dropped. Room is never waited for: a body that doesn't fit
//! is refused at once, so two half-read bodies can't wait on each other.

use std::{
	cell::RefCell,
	collections::HashMap,
	sync::{Arc, Mutex, PoisonError},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Room is handed out in whole MiB.
const UNIT: usize = 1 << 20;

#[derive(Clone)]
/// Room for big bodies, shared by every request and fetch; cheap to clone.
pub struct MemoryBudget(Arc<Inner>);

struct Inner {
	total: Arc<Semaphore>,
	owners: Mutex<HashMap<String, usize>>,
	per_owner: usize,
}

/// Room held for one body, given back when dropped.
pub struct Reservation {
	budget: Arc<Inner>,
	owner: String,
	permit: Option<OwnedSemaphorePermit>,
	units: usize,
}

impl MemoryBudget {
	/// `total` bytes for every body together, `per_owner` bytes for the
	/// bodies of one owner (an account, a server). The total is never less
	/// than one owner's share, so one body of the biggest size always fits.
	#[must_use]
	pub fn new(total: usize, per_owner: usize) -> Self {
		let per_owner = per_owner.div_ceil(UNIT).max(1);
		let total = total.div_ceil(UNIT).max(per_owner);

		Self(Arc::new(Inner {
			total: Arc::new(Semaphore::new(total)),
			owners: Mutex::default(),
			per_owner,
		}))
	}

	/// An empty reservation for `owner`, to grow as a body arrives.
	#[must_use]
	pub fn reserve(&self, owner: &str) -> Reservation {
		Reservation {
			budget: self.0.clone(),
			owner: owner.to_owned(),
			permit: None,
			units: 0,
		}
	}

	/// Room left for every body together, in bytes.
	#[must_use]
	pub fn available(&self) -> usize { self.0.total.available_permits().saturating_mul(UNIT) }
}

impl Reservation {
	/// Makes room for `bytes` in all. False when the server, or this owner,
	/// has no more to give; what was held before stays held.
	pub fn grow_to(&mut self, bytes: usize) -> bool {
		let want = bytes.div_ceil(UNIT);
		if want <= self.units {
			return true;
		}

		let extra = want.saturating_sub(self.units);
		let Ok(extra_permits) = u32::try_from(extra) else {
			return false;
		};

		let mut owners = self
			.budget
			.owners
			.lock()
			.unwrap_or_else(PoisonError::into_inner);

		let used = owners.get(&self.owner).copied().unwrap_or(0);
		if used.saturating_add(extra) > self.budget.per_owner {
			return false;
		}

		let Ok(permit) = self
			.budget
			.total
			.clone()
			.try_acquire_many_owned(extra_permits)
		else {
			return false;
		};

		owners.insert(self.owner.clone(), used.saturating_add(extra));
		drop(owners);

		match self.permit.as_mut() {
			| Some(held) => held.merge(permit),
			| None => self.permit = Some(permit),
		}
		self.units = want;

		true
	}
}

impl Drop for Reservation {
	fn drop(&mut self) {
		if self.units == 0 {
			return;
		}

		let mut owners = self
			.budget
			.owners
			.lock()
			.unwrap_or_else(PoisonError::into_inner);

		if let Some(used) = owners.get_mut(&self.owner) {
			*used = used.saturating_sub(self.units);
			if *used == 0 {
				owners.remove(&self.owner);
			}
		}
	}
}

impl std::fmt::Debug for Reservation {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Reservation")
			.field("units", &self.units)
			.finish_non_exhaustive()
	}
}

tokio::task_local! {
	static HELD: RefCell<Reservation>;
}

/// Runs `future` with `reservation` as the room any body it reads with
/// [`grow_held`] grows into, held until the future is done. Boxed, so the
/// fetches it wraps don't nest their callers' futures too deep.
pub async fn hold<F: Future>(reservation: Reservation, future: F) -> F::Output {
	HELD.scope(RefCell::new(reservation), Box::pin(future))
		.await
}

/// Grows the reservation [`hold`] set for this task to `bytes`. `None` when
/// none was set, `Some(false)` when there's no more room.
#[must_use]
pub fn grow_held(bytes: usize) -> Option<bool> {
	HELD.try_with(|held| held.borrow_mut().grow_to(bytes))
		.ok()
}

#[cfg(test)]
mod tests {
	use super::{MemoryBudget, UNIT};

	#[test]
	fn owner_and_total_limits() {
		let budget = MemoryBudget::new(3 * UNIT, 2 * UNIT);

		let mut a = budget.reserve("a");
		assert!(a.grow_to(2 * UNIT));
		assert!(!a.grow_to(2 * UNIT + 1), "one owner can't pass its share");

		let mut a2 = budget.reserve("a");
		assert!(!a2.grow_to(1), "the share counts every body of the owner");

		let mut b = budget.reserve("b");
		assert!(b.grow_to(UNIT));
		assert!(!b.grow_to(2 * UNIT), "the total is used up");

		drop(a);
		assert!(b.grow_to(2 * UNIT), "dropping gives the room back");
		assert!(a2.grow_to(UNIT));
		assert_eq!(budget.available(), 0);
	}
}
