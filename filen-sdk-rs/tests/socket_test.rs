use filen_macros::shared_test_runtime;

#[shared_test_runtime]
async fn socket_parses_everything() {
	let client = test_utils::RESOURCES.client().await;
	let _handle = client
		.add_socket_listener(
			None,
			Box::new(|event| {
				Box::pin(async move {
					println!("event: {:?}", event);
				})
			}),
		)
		.await
		.unwrap();

	tokio::time::sleep(std::time::Duration::from_secs(6000)).await;
}
