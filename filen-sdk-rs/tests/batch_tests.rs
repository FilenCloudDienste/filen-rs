use std::{
	collections::HashSet,
	fmt::Debug,
	sync::{Arc, Mutex, PoisonError},
	time::Duration,
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::{Client, http::ClientConfig, unauth::UnauthClient},
	fs::{
		HasName, HasParent, HasRemoteInfo, HasUUID,
		categories::{DirType, Normal},
		dir::RemoteDirectory,
	},
};
use filen_types::{api::v3::contacts::Contact, fs::ParentUuid};
use futures::future::join_all;
use tracing::{
	Subscriber,
	field::{Field, Visit},
	span::{Attributes, Id},
	subscriber::DefaultGuard,
};
use tracing_subscriber::{
	Layer,
	filter::LevelFilter,
	layer::{Context, SubscriberExt},
};

const BATCH_WINDOW: Duration = Duration::from_millis(5);
/// share_dir encrypts each item's metadata with RSA on the rayon pool before posting it, which
/// spreads the posts past a 5 ms window; this one outlasts that spread.
const SHARE_WINDOW: Duration = Duration::from_millis(500);

/// The burst creates this many dirs; the last one takes the first one's name.
const CREATES: usize = 20;
/// The dir deleted permanently before the favorite burst.
const DELETED: usize = 1;
/// The sub dirs of the shared dir.
const SHARED_SUB_DIRS: usize = 5;

/// Records the `endpoint` of every `http_request` span (`auth/http/logging.rs`) created while it
/// is the thread's default subscriber. Spans of spawned tasks are not recorded: they run on the
/// runtime's worker threads, where it is not the default.
#[derive(Clone, Default)]
struct EndpointRecorder(Arc<Mutex<Vec<String>>>);

impl EndpointRecorder {
	fn endpoints(&self) -> Vec<String> {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}
}

impl<S: Subscriber> Layer<S> for EndpointRecorder {
	fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
		if attrs.metadata().name() != "http_request" {
			return;
		}
		let mut visitor = EndpointVisitor(None);
		attrs.record(&mut visitor);
		if let Some(endpoint) = visitor.0 {
			self.0
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.push(endpoint);
		}
	}
}

/// Makes `recorder` record on this thread until the guard drops. The burst's logs, the batcher's
/// fallback warnings among them, still go to the test output: this replaces test-utils' global
/// subscriber here.
fn record_endpoints(recorder: &EndpointRecorder) -> DefaultGuard {
	tracing::subscriber::set_default(
		tracing_subscriber::registry()
			.with(
				tracing_subscriber::fmt::layer()
					.with_test_writer()
					.with_filter(LevelFilter::DEBUG),
			)
			.with(recorder.clone()),
	)
}

struct EndpointVisitor(Option<String>);

impl Visit for EndpointVisitor {
	fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
		// The span records `endpoint = %endpoint`, whose Debug is the Display text.
		if field.name() == "endpoint" {
			self.0 = Some(format!("{value:?}"));
		}
	}
}

/// A client of the test account, signed in as `resources.client` is, with its own batch window.
fn client_with_window(signed_in: &Client, window: Duration) -> Client {
	UnauthClient::from_config(ClientConfig::default().with_batch_window(window))
		.expect("building an unauthenticated client")
		.from_stringified(signed_in.to_stringified())
		.expect("restoring the signed-in client")
}

type Failure = (ErrorKind, Option<String>);

fn failure(e: &Error) -> Failure {
	(e.kind(), e.server_code())
}

/// What a burst answered, without the uuids, so runs with and without batching compare equal.
#[derive(Debug, PartialEq)]
struct Summary {
	/// Whether the two creates of one name answered the same dir.
	duplicate_merged: bool,
	distinct_dirs: usize,
	/// Per distinct dir: whether the server shows it favorited afterwards.
	favorites: Vec<Result<bool, Failure>>,
	/// Per distinct dir still there: whether the server shows it in the trash afterwards.
	trashes: Vec<Result<bool, Failure>>,
}

async fn burst(client: &Client, parent: &RemoteDirectory) -> Summary {
	let names = (0..CREATES)
		.map(|i| format!("d{:02}", if i == CREATES - 1 { 0 } else { i }))
		.collect::<Vec<_>>();
	let parent_dir: DirType<'_, Normal> = parent.into();
	let created = join_all(
		names
			.iter()
			.map(|name| client.create_dir(&parent_dir, name)),
	)
	.await;
	let mut dirs = created
		.into_iter()
		.map(|r| r.expect("creating a dir of the burst, as the single route does"))
		.collect::<Vec<_>>();
	let duplicate = dirs.pop().expect("the burst created dirs");
	let duplicate_merged = duplicate.uuid() == dirs[0].uuid();
	let distinct_dirs = dirs.iter().map(HasUUID::uuid).collect::<HashSet<_>>().len();

	// A clone: the deleted dir stays in `dirs` for the favorite burst.
	client
		.delete_dir_permanently(dirs[DELETED].clone())
		.await
		.expect("deleting a dir permanently");

	let favorited = join_all(
		dirs.iter_mut()
			.map(|dir| client.set_dir_favorite(dir, true)),
	)
	.await;
	let mut favorites = Vec::with_capacity(dirs.len());
	for (dir, result) in dirs.iter().zip(favorited) {
		favorites.push(match result {
			Ok(()) => Ok(client
				.get_dir(dir.uuid())
				.await
				.expect("looking up a favorited dir")
				.favorited()),
			Err(e) => Err(failure(&e)),
		});
	}

	dirs.remove(DELETED);
	// Hold the trash lock across trash -> get_dir: another leg's account-global empty-trash
	// (serialized on this lock) would permanently delete the dirs before they are looked up. It is
	// taken before the drive lock, which each trash_dir takes itself.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.expect("taking the trash lock");
	let trashed = join_all(dirs.iter_mut().map(|dir| client.trash_dir(dir))).await;
	let mut trashes = Vec::with_capacity(dirs.len());
	for (dir, result) in dirs.iter().zip(trashed) {
		trashes.push(match result {
			Ok(()) => Ok(*client
				.get_dir(dir.uuid())
				.await
				.expect("looking up a trashed dir")
				.parent() == ParentUuid::Trash(parent.uuid())),
			Err(e) => Err(failure(&e)),
		});
	}

	Summary {
		duplicate_merged,
		distinct_dirs,
		favorites,
		trashes,
	}
}

/// Runs [`burst`] in a fresh dir of `test_dir` with a client batching over `window`, and the
/// endpoints it called.
async fn recorded_burst(
	signed_in: &Client,
	test_dir: &RemoteDirectory,
	window: Duration,
	dir_name: &str,
) -> (Summary, Vec<String>) {
	let client = client_with_window(signed_in, window);
	let parent = client
		.create_dir(&test_dir.into(), dir_name)
		.await
		.expect("creating the burst's dir");
	let recorder = EndpointRecorder::default();
	// The test body runs on this thread (block_on), and join_all polls every call here, so each
	// request's span is created under this subscriber.
	let _subscriber = record_endpoints(&recorder);
	let summary = burst(&client, &parent).await;
	(summary, recorder.endpoints())
}

fn count(endpoints: &[String], endpoint: &str) -> usize {
	endpoints.iter().filter(|e| *e == endpoint).count()
}

#[shared_test_runtime]
async fn bursts_answer_with_batching_as_they_do_without() {
	// Without the drive lock held across the test: it is cached per client, so holding it on
	// `resources.client` would keep the burst clients' `lock_drive` waiting.
	let resources = test_utils::RESOURCES.get_resources().await;

	let (unbatched, unbatched_endpoints) = recorded_burst(
		&resources.client,
		&resources.dir,
		Duration::ZERO,
		"window_0",
	)
	.await;
	let (batched, batched_endpoints) = recorded_burst(
		&resources.client,
		&resources.dir,
		BATCH_WINDOW,
		"window_5ms",
	)
	.await;

	assert_eq!(unbatched, batched);

	// What the single route answers for a dir deleted permanently.
	let deleted_favorite = Err((ErrorKind::Server, Some("folder_in_trash".to_owned())));
	let mut favorites = vec![Ok(true); CREATES - 1];
	favorites[DELETED] = deleted_favorite;
	assert_eq!(
		unbatched,
		Summary {
			duplicate_merged: true,
			distinct_dirs: CREATES - 1,
			favorites,
			trashes: vec![Ok(true); CREATES - 2],
		}
	);

	assert!(
		!unbatched_endpoints
			.iter()
			.any(|e| e.starts_with("v3/bulk/")),
		"{unbatched_endpoints:?}"
	);
	// The favorites are sent in one poll. Creates and trashes first wait on one shared drive
	// lock, and each create then encrypts its metadata on the rayon pool before it posts, a hop
	// far shorter than the window for these small names, so each burst arrives within it.
	// A refused bulk request, a result-count mismatch or a per-item internal_error sends the
	// calls alone, which answer as above too: only the single calls the batched run avoided show
	// that the server took its bulk requests.
	for (bulk, single, calls) in [
		("v3/bulk/dir/create", "v3/dir/create", CREATES),
		("v3/bulk/favorite", "v3/item/favorite", CREATES - 1),
		("v3/bulk/trash", "v3/dir/trash", CREATES - 2),
	] {
		assert_eq!(
			count(&unbatched_endpoints, single),
			calls,
			"{unbatched_endpoints:?}"
		);
		assert!(
			count(&batched_endpoints, bulk) >= 1,
			"no {bulk} in {batched_endpoints:?}"
		);
		assert!(
			count(&batched_endpoints, single) < calls,
			"every {single} call went alone: {batched_endpoints:?}"
		);
	}
	// The favorites are sent in one poll: the first goes alone, the rest join one bulk request.
	assert_eq!(
		(
			count(&batched_endpoints, "v3/item/favorite"),
			count(&batched_endpoints, "v3/bulk/favorite")
		),
		(1, 1),
		"{batched_endpoints:?}"
	);
}

/// What the share account sees of a shared dir, without the uuids, so runs with and without
/// batching compare equal.
#[derive(Debug, PartialEq)]
struct SharedView {
	/// Whether the recipient's sub dirs are the ones created, by uuid.
	sub_dirs_match: bool,
	sub_dir_names: Vec<Option<String>>,
	files: usize,
}

/// Shares a fresh dir of `test_dir` holding [`SHARED_SUB_DIRS`] sub dirs with `contact`, from a
/// client batching over `window`, and returns what `share_client` sees and the endpoints called.
async fn recorded_share(
	signed_in: &Client,
	test_dir: &RemoteDirectory,
	share_client: &Client,
	contact: &Contact<'_>,
	window: Duration,
	dir_name: &str,
) -> (SharedView, Vec<String>) {
	let client = client_with_window(signed_in, window);
	let dir = client
		.create_dir(&test_dir.into(), dir_name)
		.await
		.expect("creating the shared dir");
	let shared_parent: DirType<'_, Normal> = (&dir).into();
	let mut sub_dirs = HashSet::with_capacity(SHARED_SUB_DIRS);
	for i in 0..SHARED_SUB_DIRS {
		let sub_dir = client
			.create_dir(&shared_parent, &format!("s{i}"))
			.await
			.expect("creating a sub dir");
		sub_dirs.insert(sub_dir.uuid());
	}

	let recorder = EndpointRecorder::default();
	let subscriber = record_endpoints(&recorder);
	client
		.share_dir::<fn(u64, Option<u64>)>(&dir, contact, None)
		.await
		.expect("sharing the dir");
	drop(subscriber);

	let (shared_in, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.expect("listing the recipient's shares");
	let shared_dir = shared_in
		.iter()
		.find(|d| d.get_dir().uuid() == dir.uuid())
		.expect("the recipient sees the shared dir");
	let (shared_sub_dirs, shared_files) = share_client
		.list_shared_dir::<fn(u64, Option<u64>)>(
			&shared_dir.into(),
			shared_dir.sharing_role(),
			None,
		)
		.await
		.expect("listing the shared dir as the recipient");
	let mut sub_dir_names = shared_sub_dirs
		.iter()
		.map(|d| d.get_dir().name().map(str::to_owned))
		.collect::<Vec<_>>();
	sub_dir_names.sort_unstable();
	let view = SharedView {
		sub_dirs_match: shared_sub_dirs
			.iter()
			.map(|d| d.get_dir().uuid())
			.collect::<HashSet<_>>()
			== sub_dirs,
		sub_dir_names,
		files: shared_files.len(),
	};
	(view, recorder.endpoints())
}

#[shared_test_runtime]
async fn sharing_a_dir_with_batching_shares_every_item() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;

	let (contact, _contact_locks) =
		test_utils::ensure_contact(&resources.client, &share_client).await;

	let (unbatched, unbatched_endpoints) = recorded_share(
		&resources.client,
		&resources.dir,
		&share_client,
		&contact,
		Duration::ZERO,
		"shared_window_0",
	)
	.await;
	let (batched, batched_endpoints) = recorded_share(
		&resources.client,
		&resources.dir,
		&share_client,
		&contact,
		SHARE_WINDOW,
		"shared_window_500ms",
	)
	.await;

	assert_eq!(unbatched, batched);
	assert_eq!(
		unbatched,
		SharedView {
			sub_dirs_match: true,
			sub_dir_names: (0..SHARED_SUB_DIRS)
				.map(|i| Some(format!("s{i}")))
				.collect(),
			files: 0,
		}
	);

	// The shared dir and each sub dir.
	let items = SHARED_SUB_DIRS + 1;
	assert_eq!(
		(
			count(&unbatched_endpoints, "v3/item/share"),
			count(&unbatched_endpoints, "v3/bulk/share")
		),
		(items, 0),
		"{unbatched_endpoints:?}"
	);
	assert!(
		count(&batched_endpoints, "v3/bulk/share") >= 1,
		"no v3/bulk/share in {batched_endpoints:?}"
	);
	assert!(
		count(&batched_endpoints, "v3/item/share") < items,
		"every v3/item/share call went alone: {batched_endpoints:?}"
	);
}
