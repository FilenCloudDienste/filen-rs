use jsonrpsee::server::{RpcModule, Server};
use std::{net::SocketAddr, sync::Arc};
use tokio::select;

pub(crate) async fn run_json_rpc_server(
	_client: Arc<filen_sdk_rs::auth::Client>,
	mut print_addr: impl FnMut(SocketAddr),
) -> anyhow::Result<()> {
	let server = Server::builder()
		.build("127.0.0.1:0".parse::<SocketAddr>()?)
		.await?;
	let mut module = RpcModule::new(());
	module.register_method("say_hello", |_, _, _| "lo")?;

	let addr = server.local_addr()?;
	print_addr(addr);

	let handle = server.start(module);

	let mut stop_rx = crate::CTRLC_TX.subscribe();
	select! {
		_ = handle.clone().stopped() => {}
		_ = stop_rx.recv() => {
			handle.stop().unwrap_or_else(|error| {
				eprintln!("Failed to stop server: {}", error);
			});
		}
	}

	Ok(())
}
