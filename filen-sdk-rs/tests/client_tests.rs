mod test_utils;

// all tests must be multi_threaded, otherwise drop will deadlock for TestResources
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_login() {
	test_utils::RESOURCES.client().await;
}
