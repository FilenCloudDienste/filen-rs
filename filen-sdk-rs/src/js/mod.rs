mod categories;
#[cfg(any(feature = "wasm-full", feature = "uniffi", feature = "service-worker"))]
mod managed_futures;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod params;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod returned_types;
#[cfg(all(target_family = "wasm", target_os = "unknown",))]
mod service_worker;
#[cfg(feature = "uniffi")]
mod uniffi;
#[cfg(feature = "wasm-full")]
mod wasm;

#[allow(unused_imports)]
pub(crate) use categories::{
	common::{
		dir::{color::DirColor, meta::DirMeta},
		enums::{
			NonRootItem,
			dir::{
				AnyDirWithContext, AnyLinkedDirWithContext, DirByCategoryWithContext, NonRootDir,
			},
			file::AnyFile,
		},
		file::{File, meta::FileMeta, version::FileVersion},
	},
	linked::{AnyLinkedDir, LinkedDir, LinkedFile},
	normal::{AnyNormalDir, Dir, NonRootNormalItem, Root},
	shared::{AnySharedDir, SharedDir, SharedFile, SharedRootDir, SharedRootItem},
};

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub(crate) use categories::{
	common::enums::{NonRootItemTagged, dir::NonRootDirTagged},
	normal::NonRootNormalItemTagged,
};

#[cfg(any(feature = "wasm-full", feature = "uniffi", feature = "service-worker"))]
pub use managed_futures::*;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub use params::*;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub use returned_types::*;
#[cfg(feature = "wasm-full")]
pub(crate) use service_worker::shared::*;

const HIDDEN_META_KEY: &str = "__hiddenMeta";
