mod categories;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod js_param;
#[cfg(any(
	feature = "wasm-full",
	feature = "uniffi",
	feature = "service-worker",
	feature = "napi"
))]
mod managed_futures;
mod meta_changes;
// napi-derive's generated FFI glue (the raw N-API calls in the conversion impls and callbacks it
// writes for each `#[napi]` item) lands in this module, which holds nothing but napi items and
// what they use. napi-derive registers what it exports only outside `cfg(test)`, so the lib test
// build reaches none of it; the lib build still reports dead code there.
#[cfg(feature = "napi")]
#[allow(unsafe_code)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod napi;
#[cfg(any(feature = "wasm-full", feature = "uniffi", feature = "napi"))]
mod params;
#[cfg(any(feature = "wasm-full", feature = "uniffi", feature = "napi"))]
mod returned_types;
#[cfg(all(target_family = "wasm", target_os = "unknown",))]
mod service_worker;
#[cfg(any(
	all(target_family = "wasm", target_os = "unknown", feature = "wasm-full"),
	test
))]
pub(crate) mod stream_pump;
#[cfg(all(test, feature = "uniffi"))]
pub(crate) mod test_support;
#[cfg(feature = "uniffi")]
mod uniffi;
#[cfg(feature = "wasm-full")]
mod wasm;

#[allow(unused_imports)]
pub(crate) use categories::{
	common::{
		dir::{color::DirColor, meta::DirMeta},
		enums::{
			AnyItemWithContext, NonRootItem,
			dir::{
				AnyDirWithContext, AnyLinkedDirWithContext, AnySharedDirWithContext,
				DirByCategoryWithContext, NonRootDir,
			},
			file::AnyFile,
		},
		file::{File, meta::FileMeta, version::FileVersion},
	},
	linked::{AnyLinkedDir, DirPublicLink, DirPublicLinkRW, LinkedDir, LinkedFile, LinkedRootDir},
	normal::{AnyNormalDir, Dir, NonRootNormalItem, Root},
	shared::{AnySharedDir, SharedDir, SharedFile, SharedRootDir, SharedRootItem},
};

#[cfg(any(feature = "wasm-full", feature = "uniffi", feature = "napi"))]
pub(crate) use categories::common::enums::{NonRootItemTagged, dir::NonRootDirTagged};
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub(crate) use categories::normal::NonRootNormalItemTagged;

#[cfg(feature = "uniffi")]
pub(crate) use self::uniffi::spawn_ordered_dispatch;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use js_param::{JsParam, parse_tsify};
#[cfg(any(
	feature = "wasm-full",
	feature = "uniffi",
	feature = "service-worker",
	feature = "napi"
))]
pub use managed_futures::*;
pub use meta_changes::*;
#[cfg(any(feature = "wasm-full", feature = "uniffi", feature = "napi"))]
pub use params::*;
#[cfg(any(feature = "wasm-full", feature = "uniffi", feature = "napi"))]
pub use returned_types::*;
#[cfg(feature = "service-worker")]
pub(crate) use service_worker::impls::*;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) use service_worker::shared::*;
#[cfg(feature = "wasm-full")]
pub(crate) use wasm::call_callback;
