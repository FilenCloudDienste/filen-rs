//! A key store in memory, with switches that fail its calls and a hatch that stores raw bytes.

use std::sync::{
	Mutex, MutexGuard, PoisonError,
	atomic::{AtomicUsize, Ordering},
};

use crate::{
	auth::{KeyStore, KeyStoreError, SealingKey},
	sealed,
};

/// What a failing switch answers with.
#[derive(Debug, thiserror::Error)]
#[error("the memory key store was told to fail")]
struct SwitchedFailure;

/// A key store holding at most one secret in memory.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
	state: Mutex<MemoryState>,
	stores: AtomicUsize,
	deletes: AtomicUsize,
}

#[derive(Debug, Default)]
struct MemoryState {
	secret: Option<Vec<u8>>,
	fail_load: bool,
	fail_store: bool,
	fail_delete: bool,
}

impl MemoryKeyStore {
	/// An empty store.
	pub fn new() -> Self {
		Self::default()
	}

	fn state(&self) -> MutexGuard<'_, MemoryState> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Fails every later load.
	pub fn fail_load(&self) {
		self.state().fail_load = true;
	}

	/// Fails every later store.
	pub fn fail_store(&self) {
		self.state().fail_store = true;
	}

	/// Fails every later delete.
	pub fn fail_delete(&self) {
		self.state().fail_delete = true;
	}

	/// Stores `secret` as it is, which need be no valid key.
	pub fn put_raw(&self, secret: Vec<u8>) {
		self.state().secret = Some(secret);
	}

	/// The secret stored, as bytes.
	pub fn raw(&self) -> Option<Vec<u8>> {
		self.state().secret.clone()
	}

	/// How many stores succeeded.
	pub fn store_count(&self) -> usize {
		self.stores.load(Ordering::Relaxed)
	}

	/// How many deletes succeeded.
	pub fn delete_count(&self) -> usize {
		self.deletes.load(Ordering::Relaxed)
	}
}

impl sealed::Sealed for MemoryKeyStore {}

impl KeyStore for MemoryKeyStore {
	fn load(&self) -> Result<Option<SealingKey>, KeyStoreError> {
		let state = self.state();
		if state.fail_load {
			return Err(KeyStoreError::Backend(Box::new(SwitchedFailure)));
		}
		Ok(state
			.secret
			.as_deref()
			.map(SealingKey::from_bytes)
			.transpose()?)
	}

	fn store(&self, key: &SealingKey) -> Result<(), KeyStoreError> {
		let mut state = self.state();
		if state.fail_store {
			return Err(KeyStoreError::Backend(Box::new(SwitchedFailure)));
		}
		state.secret = Some(key.as_bytes().to_vec());
		self.stores.fetch_add(1, Ordering::Relaxed);
		Ok(())
	}

	fn delete(&self) -> Result<(), KeyStoreError> {
		let mut state = self.state();
		if state.fail_delete {
			return Err(KeyStoreError::Backend(Box::new(SwitchedFailure)));
		}
		state.secret = None;
		self.deletes.fetch_add(1, Ordering::Relaxed);
		Ok(())
	}
}
