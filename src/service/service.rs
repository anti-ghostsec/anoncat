use std::{any::Any, fmt::Write, sync::Arc};

use async_trait::async_trait;
use tuwunel_core::{Result, Server, utils::string::SplitInfallible};
use tuwunel_database::Database;

use crate::services::OnceServices;

/// Abstract interface for a Service
#[async_trait]
pub(crate) trait Service: Any + Send + Sync {
	/// Implement the construction of the service instance. Services are
	/// generally singletons so expect this to only be called once for a
	/// service type. Note that it may be called again after a server reload,
	/// but the prior instance will have been dropped first. Failure will
	/// shutdown the server with an error.
	fn build(args: &Args<'_>) -> Result<Arc<impl Service>>
	where
		Self: Sized;

	/// Implement the service's worker loop. The service manager spawns a
	/// task and calls this function after all services have been built.
	async fn worker(self: Arc<Self>) -> Result { Ok(()) }

	/// Interrupt the service. This is sent to initiate a graceful shutdown.
	/// The service worker should return from its work loop.
	async fn interrupt(&self) {}

	/// Clear any caches or similar runtime state.
	async fn clear_cache(&self) {}

	/// Memory usage report in a markdown string.
	async fn memory_usage(&self, _out: &mut (dyn Write + Send)) -> Result { Ok(()) }

	/// Return the name of the service.
	/// i.e. `crate::service::make_name(std::module_path!())`
	fn name(&self) -> &str;

	/// Return true if the service worker opts out of the tokio cooperative
	/// budgeting. This can reduce tail latency at the risk of event loop
	/// starvation.
	fn unconstrained(&self) -> bool { false }
}

/// Args are passed to `Service::build` when a service is constructed. This
/// allows for arguments to change with limited impact to the many services.
pub(crate) struct Args<'a> {
	pub(crate) server: &'a Arc<Server>,
	pub(crate) db: &'a Arc<Database>,
	pub(crate) services: &'a Arc<OnceServices>,
}

/// Utility for service implementations; see Service::name() in the trait.
#[inline]
pub(crate) fn make_name(module_path: &str) -> &str { module_path.split_once_infallible("::").1 }
