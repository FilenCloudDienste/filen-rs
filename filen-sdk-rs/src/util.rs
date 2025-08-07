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
