//! File Explorer's thumbnail requests over COM: the helper exe as an out-of-process server of this
//! provider's thumbnail class ([`THUMBNAIL_CLSID`]), registered with `CoRegisterClassObject` on a
//! thread of its own, answering `IThumbnailProvider::GetThumbnail` through the handler it is
//! started with.
//!
//! STUB: this build has no COM layer. [`is_implemented`] is `false`, so the helper writes no
//! `ThumbnailProvider` value and Explorer keeps its own thumbnails, and [`ThumbnailServer::start`]
//! refuses with [`ComError::NotImplemented`]. The interface below is what the rest of the crate
//! calls; filling this file in, with every `unsafe` of the COM layer in it, is all that changes
//! when it is written. What it must do is specified outside the repository.
//!
//! [`THUMBNAIL_CLSID`]: crate::os::cfapi::explorer::THUMBNAIL_CLSID

use windows_sys::core::GUID;

use crate::thumbnails::ThumbnailHandler;

/// Whether this build serves File Explorer's thumbnail requests: the one switch the helper's
/// registration of the class and of the root's `ThumbnailProvider` value follows. `true` only once
/// [`ThumbnailServer::start`] is written.
pub(crate) fn is_implemented() -> bool {
	false
}

/// Why the thumbnail server did not start.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ComError {
	/// This build has no COM layer.
	#[error("this build does not serve thumbnails")]
	NotImplemented,
}

/// The thumbnail class, registered with COM for the life of the value; [`ThumbnailServer::stop`]
/// revokes it.
#[derive(Debug)]
pub struct ThumbnailServer {
	_private: (),
}

impl ThumbnailServer {
	/// Serves the class `clsid` from this process, a COM local server, answering each
	/// `GetThumbnail` with `handler` called on the server's own thread, never on the helper's
	/// runtime.
	pub(crate) fn start(_clsid: GUID, _handler: ThumbnailHandler) -> Result<Self, ComError> {
		Err(ComError::NotImplemented)
	}

	/// Revokes the class and ends the server's thread; a request in progress is answered first.
	pub(crate) fn stop(self) {}
}
