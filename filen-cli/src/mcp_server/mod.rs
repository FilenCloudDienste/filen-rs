//! [cli-doc] mcp-server
//! You can use the Filen CLI to run a local MCP server (https://modelcontextprotocol.io)
//! that allows AI agents to interact with your Filen drive.
//!
use crate::ui::UI;
use anyhow::{Context as _, Result};

mod endpoints;
pub(crate) mod server;

pub(crate) async fn mcp_cmd(ui: &mut UI) -> Result<()> {
	let filen_cli_binary = std::env::current_exe().context("Failed to get current executable")?;
	ui.print("The MCP server runs via stdio transport, and is included in the Filen CLI binary.");
	ui.print("");
	ui.print_warning(
		"Important! Since it communicates on stdout, make sure there is no excess output like update or login prompts by keeping your installation updated and using an auth config for login.",
	);
	// todo: make sure this is more user-friendly
	ui.print("");
	ui.print("Install the MCP server in your AI agent's environment, e.g. for Claude Code:");
	ui.print(&format!(
		"  claude mcp add Filen --transport stdio -- {} --mcp-server",
		filen_cli_binary.display()
	));

	Ok(())
}
