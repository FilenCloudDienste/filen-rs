//! What File Explorer shows for a sync root: its name and its icon, as the parent names them in
//! `register`, and the commands its context menu offers on the root's items. Checked here,
//! OS-neutrally, so the protocol's fixtures hold on every host.

#[cfg(test)]
mod tests;

/// The most UTF-16 units a display name or a command label holds.
const MAX_LABEL_UNITS: usize = 255;

/// The most UTF-16 units an icon path or an app path holds.
const MAX_PATH_UNITS: usize = 1024;

/// The extensions an icon path may have: an icon file, or a module holding icon resources.
const ICON_EXTENSIONS: [&str; 3] = ["ico", "exe", "dll"];

/// The name the root shows when the parent names none.
const DEFAULT_DISPLAY_NAME: &str = "Filen";

/// Why a display name, an icon path, an app path or a command label was refused.
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
	/// Not 1 to 1024 UTF-16 units.
	#[error("explorerCommands.appPath is not 1 to 1024 UTF-16 units")]
	AppPathLength,
	/// Not an absolute path on a drive letter (`X:\...`): relative, UNC or a device path.
	#[error("explorerCommands.appPath is not an absolute path on a drive letter")]
	AppPathNotAbsolute,
	/// Holds a control character, a `%` (read as a variable), a `"` (which would end the quoted
	/// path in a command line) or a `/` (a separator Windows also takes, which would hide a `..`).
	#[error("explorerCommands.appPath holds a control character, a %, a quote or a /")]
	AppPathCharacter,
	/// Holds a `.` or `..` component.
	#[error("explorerCommands.appPath holds a . or .. component")]
	AppPathComponent,
	/// Does not end in `.exe`.
	#[error("explorerCommands.appPath does not end in .exe")]
	AppPathExtension,
	/// Not 1 to 255 UTF-16 units.
	#[error("an explorerCommands label is not 1 to 255 UTF-16 units")]
	CommandLabelLength,
	/// Holds a control character (U+0000 to U+001F, U+007F to U+009F).
	#[error("an explorerCommands label holds a control character")]
	CommandLabelControl,
	/// Starts or ends with whitespace.
	#[error("an explorerCommands label starts or ends with whitespace")]
	CommandLabelWhitespace,
	/// Holds a `%`.
	#[error("an explorerCommands label holds a %")]
	CommandLabelPercent,
	/// Starts with `@`, which File Explorer reads as a string to load from a module.
	#[error("an explorerCommands label starts with @")]
	CommandLabelIndirect,
}

/// Whether `c` is a C0 or C1 control character.
fn is_control(c: char) -> bool {
	matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// Which part of the label rule a text breaks.
enum LabelRule {
	Length,
	Control,
	Whitespace,
	Percent,
	Indirect,
}

/// Checks `text` against the rule a display name and a command label share: 1 to 255 UTF-16
/// units, no control character, no leading or trailing whitespace, no `%` and no leading `@`.
fn check_label(text: &str) -> Result<(), LabelRule> {
	let units = text.encode_utf16().count();
	if units == 0 || units > MAX_LABEL_UNITS {
		return Err(LabelRule::Length);
	}
	if text.chars().any(is_control) {
		return Err(LabelRule::Control);
	}
	if text.trim() != text {
		return Err(LabelRule::Whitespace);
	}
	if text.contains('%') {
		return Err(LabelRule::Percent);
	}
	if text.starts_with('@') {
		return Err(LabelRule::Indirect);
	}
	Ok(())
}

/// Whether `path` is 1 to 1024 UTF-16 units.
fn path_length_ok(path: &str) -> bool {
	let units = path.encode_utf16().count();
	units != 0 && units <= MAX_PATH_UNITS
}

/// Whether `path` starts with a drive letter, a colon and a backslash (`X:\`).
fn on_drive_letter(path: &str) -> bool {
	let mut chars = path.chars();
	matches!(
		(chars.next(), chars.next(), chars.next()),
		(Some(letter), Some(':'), Some('\\')) if letter.is_ascii_alphabetic()
	)
}

/// The extension of the file name `path` ends in, if it has one.
fn extension(path: &str) -> Option<&str> {
	path.rsplit_once('\\')
		.and_then(|(_, name)| name.rsplit_once('.'))
		.map(|(_, extension)| extension)
}

/// The name File Explorer shows for a root: 1 to 255 UTF-16 units, no control character, no
/// leading or trailing whitespace, no `%` and no leading `@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayName(String);

impl DisplayName {
	/// `name` as a display name, if it follows the rule on [`DisplayName`].
	pub fn parse(name: &str) -> Result<Self, BrandingError> {
		check_label(name).map_err(|rule| match rule {
			LabelRule::Length => BrandingError::DisplayNameLength,
			LabelRule::Control => BrandingError::DisplayNameControl,
			LabelRule::Whitespace => BrandingError::DisplayNameWhitespace,
			LabelRule::Percent => BrandingError::DisplayNamePercent,
			LabelRule::Indirect => BrandingError::DisplayNameIndirect,
		})?;
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
		if !path_length_ok(path) {
			return Err(BrandingError::IconPathLength);
		}
		if !on_drive_letter(path) {
			return Err(BrandingError::IconPathNotAbsolute);
		}
		if path.chars().any(|c| is_control(c) || c == '%' || c == ',') {
			return Err(BrandingError::IconPathCharacter);
		}
		if !extension(path).is_some_and(|extension| {
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

/// The program File Explorer's commands start, as the parent spells it: an absolute path on a
/// drive letter (`X:\...`, no UNC or `\\?\` path), 1 to 1024 UTF-16 units, no control character,
/// `%`, `"` or `/`, no `.` or `..` component, ending in `.exe` in any case. Quoted as it is in a
/// command line, so it needs no escaping. Whether a file is there is the Windows layer's check,
/// when it writes the commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPath(String);

impl AppPath {
	/// `path` as an app path, if it follows the rule on [`AppPath`].
	pub fn parse(path: &str) -> Result<Self, BrandingError> {
		if !path_length_ok(path) {
			return Err(BrandingError::AppPathLength);
		}
		if !on_drive_letter(path) {
			return Err(BrandingError::AppPathNotAbsolute);
		}
		if path
			.chars()
			.any(|c| is_control(c) || matches!(c, '%' | '"' | '/'))
		{
			return Err(BrandingError::AppPathCharacter);
		}
		if path
			.split('\\')
			.any(|component| component == "." || component == "..")
		{
			return Err(BrandingError::AppPathComponent);
		}
		if !extension(path).is_some_and(|extension| extension.eq_ignore_ascii_case("exe")) {
			return Err(BrandingError::AppPathExtension);
		}
		Ok(Self(path.to_owned()))
	}

	/// The path.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// What a context menu command is called, as File Explorer shows it: the rule on [`DisplayName`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLabel(String);

impl CommandLabel {
	/// `label` as a command label, if it follows the rule on [`DisplayName`].
	pub fn parse(label: &str) -> Result<Self, BrandingError> {
		check_label(label).map_err(|rule| match rule {
			LabelRule::Length => BrandingError::CommandLabelLength,
			LabelRule::Control => BrandingError::CommandLabelControl,
			LabelRule::Whitespace => BrandingError::CommandLabelWhitespace,
			LabelRule::Percent => BrandingError::CommandLabelPercent,
			LabelRule::Indirect => BrandingError::CommandLabelIndirect,
		})?;
		Ok(Self(label.to_owned()))
	}

	/// The label.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// The commands File Explorer's context menu offers on the root's items: a submenu starting `app`
/// with the action and the item's path. Each label the parent left out takes the helper's English
/// default; the submenu's own, the root's display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerCommands {
	/// The program the commands start.
	pub app: AppPath,
	/// The submenu's label.
	pub menu: Option<CommandLabel>,
	/// "Share / create public link".
	pub share: Option<CommandLabel>,
	/// "View on filen.io".
	pub web: Option<CommandLabel>,
	/// "Version history", offered on files only.
	pub versions: Option<CommandLabel>,
}

impl ExplorerCommands {
	/// The commands a `register` names, checked: the app's path and each label given.
	pub fn parse(
		app: &str,
		menu: Option<&str>,
		share: Option<&str>,
		web: Option<&str>,
		versions: Option<&str>,
	) -> Result<Self, BrandingError> {
		let label = |label: Option<&str>| label.map(CommandLabel::parse).transpose();
		Ok(Self {
			app: AppPath::parse(app)?,
			menu: label(menu)?,
			share: label(share)?,
			web: label(web)?,
			versions: label(versions)?,
		})
	}
}

/// What File Explorer shows for a root: its name, and its icon, the `index`th of the file at
/// `icon` (a negative index names a resource id), or the system's cloud icon when `icon` is
/// `None`; and the commands its context menu offers on the root's items, when the parent names
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branding {
	/// The name.
	pub display_name: DisplayName,
	/// The file the icon is in; `None` for the system's cloud icon.
	pub icon: Option<IconPath>,
	/// Which icon of the file.
	pub icon_index: i32,
	/// The context menu's commands; `None` leaves whatever an earlier run wrote. Never stored in
	/// the roots index: the parent names them in every `register`. Boxed, as a branding sits in
	/// every phase of the helper and in each `register`, and most hold none.
	pub commands: Option<Box<ExplorerCommands>>,
}

impl Default for Branding {
	/// The name "Filen", the system's cloud icon and no commands.
	fn default() -> Self {
		Self {
			display_name: DisplayName(DEFAULT_DISPLAY_NAME.to_owned()),
			icon: None,
			icon_index: 0,
			commands: None,
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
		commands: Option<ExplorerCommands>,
	) -> Result<Option<Self>, BrandingError> {
		if display_name.is_none() && icon.is_none() && icon_index.is_none() && commands.is_none() {
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
			commands: commands.map(Box::new),
		}))
	}
}
