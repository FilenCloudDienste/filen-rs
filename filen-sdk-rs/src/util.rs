pub struct PathIterator<'a> {
	path: &'a str,
	split: std::str::MatchIndices<'a, char>,
	last_idx: usize,
}

impl<'a> PathIterator<'a> {
	fn new(path: &'a str) -> Self {
		let path = path.trim_start_matches('/');
		Self {
			path,
			split: path.match_indices('/'),
			last_idx: 0,
		}
	}
}

impl<'a> Iterator for PathIterator<'a> {
	type Item = (&'a str, &'a str);

	fn next(&mut self) -> Option<Self::Item> {
		match self.split.next() {
			None if self.last_idx == self.path.len() => None,
			None => {
				let slice = &self.path[self.last_idx..];
				self.last_idx = self.path.len();
				Some((slice, ""))
			}
			Some((idx, _)) => {
				let slice = &self.path[self.last_idx..idx];
				let rest = &self.path[idx + 1..];
				self.last_idx = idx + 1;
				Some((slice, rest))
			}
		}
	}
}

pub trait PathIteratorExt {
	fn path_iter(&self) -> PathIterator<'_>;
}

impl PathIteratorExt for str {
	fn path_iter(&self) -> PathIterator<'_> {
		PathIterator::new(self)
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub type MaybeSendBoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;
#[cfg(target_arch = "wasm32")]
pub type MaybeSendBoxFuture<'a, T> = futures::future::LocalBoxFuture<'a, T>;

#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSendSync {}

#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync> MaybeSendSync for T {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSendSync for T {}

#[cfg(not(target_arch = "wasm32"))]
pub type MaybeSendCallback<'a, T> = std::sync::Arc<dyn Fn(T) + Send + Sync + 'a>;
#[cfg(target_arch = "wasm32")]
pub type MaybeSendCallback<'a, T> = std::rc::Rc<dyn Fn(T) + 'a>;

#[cfg(not(target_arch = "wasm32"))]
pub type MaybeArc<T> = std::sync::Arc<T>;
#[cfg(target_arch = "wasm32")]
pub type MaybeArc<T> = std::rc::Rc<T>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn path_iterator() {
		assert_eq!(
			"root/dir/file.txt".path_iter().collect::<Vec<_>>(),
			vec![
				("root", "dir/file.txt"),
				("dir", "file.txt"),
				("file.txt", "")
			]
		);
		assert_eq!(
			"root/dir/".path_iter().collect::<Vec<_>>(),
			vec![("root", "dir/"), ("dir", "")]
		);
		assert_eq!(
			"/root/dir/".path_iter().collect::<Vec<_>>(),
			vec![("root", "dir/"), ("dir", "")]
		);
		assert_eq!("root".path_iter().collect::<Vec<_>>(), vec![("root", "")]);
		assert_eq!("/".path_iter().collect::<Vec<_>>(), vec![]);
		assert_eq!("".path_iter().collect::<Vec<_>>(), vec![]);
	}
}
