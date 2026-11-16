//! What File Explorer shows for a sync root: its name and its icon, as the parent names them in
//! `register`. Checked here, OS-neutrally, so the protocol's fixtures hold on every host.

#[cfg(test)]
mod tests;

/// The most UTF-16 units a display name holds.
const MAX_DISPLAY_NAME_UNITS: usize = 255;

/// The most UTF-16 units an icon path holds.
const MAX_ICON_PATH_UNITS: usize = 1024;

/// The extensions an icon path may have: an icon file, or a module holding icon resources.
const ICON_EXTENSIONS: [&str; 3] = ["ico", "exe", "dll"];

/// The name the root shows when the parent names none.
const DEFAULT_DISPLAY_NAME: &str = "Filen";

/// Why a display name or an icon path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BrandingError {
	/// Not 1 to 255 UTF-16 units.
	#[error("displayName is not 1 to 255 UTF-16 units")]
	DisplayNameLength,
	/// Holds a control character (U+0000 to U+001F, U+007F to U+009F).
	#[error("displayName holds a control character")]
	DisplayNameControl,
	/// Starts or ends with whitespace.
	#[error("displayName starts or ends with whitespace")]
	DisplayNameWhitespace,
	/// Holds a `%`, which the registry's expandable strings would read as a variable.
	#[error("displayName holds a %")]
	DisplayNamePercent,
	/// Starts with `@`, which File Explorer reads as a string to load from a module
	/// (`@<module>,-<id>`).
	#[error("displayName starts with @")]
	DisplayNameIndirect,
	/// Not 1 to 1024 UTF-16 units.
	#[error("iconPath is not 1 to 1024 UTF-16 units")]
	IconPathLength,
	/// Not an absolute path on a drive letter (`X:\...`): relative, UNC or a device path.
	#[error("iconPath is not an absolute path on a drive letter")]
	IconPathNotAbsolute,
	/// Holds a control character, a `%` (read as a variable) or a `,` (which ends the path in an
	/// icon resource string).
	#[error("iconPath holds a control character, a % or a comma")]
	IconPathCharacter,
	/// Does not end in `.ico`, `.exe` or `.dll`.
	#[error("iconPath does not end in .ico, .exe or .dll")]
	IconPathExtension,
	/// An icon index without an icon path.
	#[error("iconIndex is given without an iconPath")]
	IndexWithoutPath,
}

/// Whether `c` is a C0 or C1 control character.
fn is_control(c: char) -> bool {
	matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// The name File Explorer shows for a root: 1 to 255 UTF-16 units, no control character, no
/// leading or trailing whitespace, no `%` and no leading `@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayName(String);

impl DisplayName {
	/// `name` as a display name, if it follows the rule on [`DisplayName`].
	pub fn parse(name: &str) -> Result<Self, BrandingError> {
		let units = name.encode_utf16().count();
		if units == 0 || units > MAX_DISPLAY_NAME_UNITS {
			return Err(BrandingError::DisplayNameLength);
		}
		if name.chars().any(is_control) {
			return Err(BrandingError::DisplayNameControl);
		}
		if name.trim() != name {
			return Err(BrandingError::DisplayNameWhitespace);
		}
		if name.contains('%') {
			return Err(BrandingError::DisplayNamePercent);
		}
		if name.starts_with('@') {
			return Err(BrandingError::DisplayNameIndirect);
		}
		Ok(Self(name.to_owned()))
	}

	/// The name.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// The file File Explorer takes a root's icon from, as the parent spells it: an absolute path on
/// a drive letter (`X:\...`, no UNC or `\\?\` path), 1 to 1024 UTF-16 units, no control character,
/// `%` or `,`, ending in `.ico`, `.exe` or `.dll`. Whether a file is there is the Windows layer's
/// check, when it writes the icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconPath(String);

impl IconPath {
	/// `path` as an icon path, if it follows the rule on [`IconPath`].
	pub fn parse(path: &str) -> Result<Self, BrandingError> {
		let units = path.encode_utf16().count();
		if units == 0 || units > MAX_ICON_PATH_UNITS {
			return Err(BrandingError::IconPathLength);
		}
		let mut chars = path.chars();
		let drive = matches!(
			(chars.next(), chars.next(), chars.next()),
			(Some(letter), Some(':'), Some('\\')) if letter.is_ascii_alphabetic()
		);
		if !drive {
			return Err(BrandingError::IconPathNotAbsolute);
		}
		if path.chars().any(|c| is_control(c) || c == '%' || c == ',') {
			return Err(BrandingError::IconPathCharacter);
		}
		let extension = path
			.rsplit_once('\\')
			.and_then(|(_, name)| name.rsplit_once('.'))
			.map(|(_, extension)| extension);
		if !extension.is_some_and(|extension| {
			ICON_EXTENSIONS
				.iter()
				.any(|known| extension.eq_ignore_ascii_case(known))
		}) {
			return Err(BrandingError::IconPathExtension);
		}
		Ok(Self(path.to_owned()))
	}

	/// The path.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// What File Explorer shows for a root: its name, and its icon, the `index`th of the file at
/// `icon` (a negative index names a resource id), or the system's cloud icon when `icon` is
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branding {
	/// The name.
	pub display_name: DisplayName,
	/// The file the icon is in; `None` for the system's cloud icon.
	pub icon: Option<IconPath>,
	/// Which icon of the file.
	pub icon_index: i32,
}

impl Default for Branding {
	/// The name "Filen" and the system's cloud icon.
	fn default() -> Self {
		Self {
			display_name: DisplayName(DEFAULT_DISPLAY_NAME.to_owned()),
			icon: None,
			icon_index: 0,
		}
	}
}

impl Branding {
	/// The branding a `register` names: each field the parent left out takes its default. `None`
	/// when the parent named none of them.
	pub fn parse(
		display_name: Option<&str>,
		icon: Option<&str>,
		icon_index: Option<i32>,
	) -> Result<Option<Self>, BrandingError> {
		if display_name.is_none() && icon.is_none() && icon_index.is_none() {
			return Ok(None);
		}
		if icon.is_none() && icon_index.is_some() {
			return Err(BrandingError::IndexWithoutPath);
		}
		let default = Self::default();
		Ok(Some(Self {
			display_name: display_name
				.map(DisplayName::parse)
				.transpose()?
				.unwrap_or(default.display_name),
			icon: icon.map(IconPath::parse).transpose()?,
			icon_index: icon_index.unwrap_or(default.icon_index),
		}))
	}
}
