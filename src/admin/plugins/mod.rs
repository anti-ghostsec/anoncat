mod commands;

use clap::Subcommand;
use tuwunel_core::Result;

use crate::admin_command_dispatch;

#[admin_command_dispatch(handler_prefix = "plugins")]
#[derive(Debug, Subcommand)]
pub(super) enum PluginsCommand {
	/// - List plugins: loaded ones and those in the plugins directory
	List,

	/// - Load a plugin from the plugins directory (or reload it if loaded)
	Load {
		/// The plugin's name: <name>.wasm with its manifest <name>.toml.
		name: String,
	},

	/// - Unload a plugin
	Unload {
		name: String,
	},

	/// - Reload a plugin from its file, e.g. after replacing it
	Reload {
		name: String,
	},

	/// - Call a plugin's own command
	Call {
		name: String,

		/// Arguments handed to the plugin.
		args: Vec<String>,
	},
}
