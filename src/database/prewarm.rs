//! Startup block-cache prewarm.
//!
//! After a restart every first read of a room's state comes off the disk, and
//! on a spinning disk each of those is a seek; the first message into a room
//! can take half a second. Reading each column once, in key order, turns those
//! scattered seeks into a few long sequential reads and leaves the blocks in
//! the block cache.

use std::{
	ffi::CStr,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	thread::JoinHandle,
	time::Instant,
};

use tuwunel_core::{Result, debug, info, warn};

use crate::{Map, map::iter_options_default, util::map_err};

const CACHE_USAGE: &CStr = c"rocksdb.block-cache-usage";
const CACHE_CAPACITY: &CStr = c"rocksdb.block-cache-capacity";
const SST_SIZE: &CStr = c"rocksdb.total-sst-files-size";

/// Readahead for the scan. Large enough that the disk spends its time
/// transferring rather than seeking.
const READAHEAD: usize = 4 << 20;

/// Keys scanned between checks for a stop request.
const STOP_CHECK_KEYS: usize = 1024;

/// The running prewarm. Dropping it stops the scan and waits for the thread,
/// so the maps it holds are released before the database closes.
pub(crate) struct Prewarm {
	stop: Arc<AtomicBool>,
	thread: Option<JoinHandle<()>>,
}

impl Drop for Prewarm {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		if let Some(thread) = self.thread.take() {
			thread.join().ok();
		}
	}
}

/// Starts the prewarm on its own thread; the server does not wait for it.
pub(crate) fn spawn(maps: Vec<Arc<Map>>) -> Option<Prewarm> {
	let stop = Arc::new(AtomicBool::new(false));
	let stopping = stop.clone();
	let spawned = std::thread::Builder::new()
		.name("prewarm".into())
		.spawn(move || {
			if let Err(e) = prewarm(&maps, &stopping) {
				warn!("Database prewarm stopped: {e}");
			}
		});

	match spawned {
		| Ok(thread) => Some(Prewarm { stop, thread: Some(thread) }),
		| Err(e) => {
			warn!("Database prewarm not started: {e}");
			None
		},
	}
}

/// Scans columns smallest first, skipping any that would not fit in what is
/// left of its block cache, so the scan never evicts what it already loaded.
fn prewarm(maps: &[Arc<Map>], stop: &AtomicBool) -> Result {
	let started = Instant::now();
	let mut maps = maps
		.iter()
		.map(|map| Ok((map.engine().property_integer(&map.cf(), SST_SIZE)?, map)))
		.collect::<Result<Vec<_>>>()?;

	maps.sort_by_key(|(size, _)| *size);

	let (mut columns, mut bytes, mut skipped) = (0_usize, 0_u64, 0_usize);
	for (size, map) in maps {
		let engine = map.engine();
		if stop.load(Ordering::Relaxed) || !engine.ctx.server.is_running() {
			return Ok(());
		}

		if size == 0 {
			continue;
		}

		let usage = engine.property_integer(&map.cf(), CACHE_USAGE)?;
		let capacity = engine.property_integer(&map.cf(), CACHE_CAPACITY)?;
		if usage.saturating_add(size) > capacity {
			debug!(column = map.name(), size, usage, capacity, "Prewarm skipped column");
			skipped = skipped.saturating_add(1);
			continue;
		}

		let mut opts = iter_options_default(engine);
		opts.set_readahead_size(READAHEAD);
		opts.fill_cache(true);

		let mut it = engine.db.raw_iterator_cf_opt(&map.cf(), opts);
		it.seek_to_first();
		let mut keys = 0_usize;
		while it.valid() {
			it.next();
			keys = keys.wrapping_add(1);
			if keys % STOP_CHECK_KEYS == 0
				&& (stop.load(Ordering::Relaxed) || !engine.ctx.server.is_running())
			{
				debug!(column = map.name(), "Prewarm stopped for shutdown");
				return Ok(());
			}
		}
		it.status().map_err(map_err)?;

		// Blocks are cached uncompressed, so a column can fill its cache even
		// though its files fit; the rest of it is read from disk on demand.
		let after = engine.property_integer(&map.cf(), CACHE_USAGE)?;
		if after >= capacity.saturating_sub(capacity / 20) {
			info!(column = map.name(), size, capacity, "Prewarm filled this column's cache");
		}

		columns = columns.saturating_add(1);
		bytes = bytes.saturating_add(size);
	}

	info!(
		columns,
		skipped,
		mib = bytes >> 20,
		time = ?started.elapsed(),
		"Database prewarm finished."
	);

	Ok(())
}
