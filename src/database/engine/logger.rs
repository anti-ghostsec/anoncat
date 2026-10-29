use rocksdb::LogLevel;
use tuwunel_core::{debug, error, trace, warn};

#[tracing::instrument(
	parent = None,
	name = "rocksdb",
	level = "trace"
	skip(msg),
)]
pub(crate) fn handle(level: LogLevel, msg: &str) {
	let msg = msg.trim();
	let is_options = msg.starts_with("Options") || msg.starts_with("table_factory options");

	match level {
		| _ if is_options => trace!("{msg}"),
		| LogLevel::Header | LogLevel::Debug => debug!("{msg}"),
		| LogLevel::Error | LogLevel::Fatal => error!("{msg}"),
		| LogLevel::Info => debug!("{msg}"),
		| LogLevel::Warn => warn!("{msg}"),
	}
}
