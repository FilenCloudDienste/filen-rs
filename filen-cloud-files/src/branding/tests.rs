use super::*;

#[test]
fn display_names_follow_the_rule() {
	let cases: [(&str, Result<(), BrandingError>); 12] = [
		("Filen", Ok(())),
		("Filen (2)", Ok(())),
		("Filen – Arbeit", Ok(())),
		("", Err(BrandingError::DisplayNameLength)),
		("a\u{1}b", Err(BrandingError::DisplayNameControl)),
		("a\u{7f}b", Err(BrandingError::DisplayNameControl)),
		("a\u{85}b", Err(BrandingError::DisplayNameControl)),
		("a\nb", Err(BrandingError::DisplayNameControl)),
		(" Filen", Err(BrandingError::DisplayNameWhitespace)),
		("Filen\u{a0}", Err(BrandingError::DisplayNameWhitespace)),
		("%USERNAME%", Err(BrandingError::DisplayNamePercent)),
		("100% Filen", Err(BrandingError::DisplayNamePercent)),
	];
	for (name, expected) in cases {
		assert_eq!(DisplayName::parse(name).map(|_| ()), expected, "{name:?}");
	}
}

#[test]
fn a_display_name_naming_a_resource_string_is_refused() {
	for name in ["@shell32.dll,-21769", "@C:\\x\\y.dll,-1", "@Filen"] {
		assert_eq!(
			DisplayName::parse(name),
			Err(BrandingError::DisplayNameIndirect),
			"{name:?}"
		);
	}
	assert!(DisplayName::parse("Filen @ home").is_ok());
}

#[test]
fn a_display_name_holds_255_utf16_units_and_no_more() {
	assert!(DisplayName::parse(&"a".repeat(255)).is_ok());
	assert_eq!(
		DisplayName::parse(&"a".repeat(256)),
		Err(BrandingError::DisplayNameLength)
	);
	// 128 characters outside the BMP are 256 units.
	assert_eq!(
		DisplayName::parse(&"\u{1f600}".repeat(128)),
		Err(BrandingError::DisplayNameLength)
	);
	assert!(DisplayName::parse(&"\u{1f600}".repeat(127)).is_ok());
}

#[test]
fn icon_paths_follow_the_rule() {
	let cases: [(&str, Result<(), BrandingError>); 14] = [
		("C:\\Program Files\\Filen\\Filen.exe", Ok(())),
		("d:\\icons\\filen.ICO", Ok(())),
		("C:\\Windows\\system32\\imageres.dll", Ok(())),
		("", Err(BrandingError::IconPathLength)),
		("Filen.exe", Err(BrandingError::IconPathNotAbsolute)),
		("C:Filen.exe", Err(BrandingError::IconPathNotAbsolute)),
		(
			"\\\\server\\share\\Filen.exe",
			Err(BrandingError::IconPathNotAbsolute),
		),
		(
			"\\\\?\\C:\\Filen.exe",
			Err(BrandingError::IconPathNotAbsolute),
		),
		("1:\\Filen.exe", Err(BrandingError::IconPathNotAbsolute)),
		("C:\\a,b\\Filen.exe", Err(BrandingError::IconPathCharacter)),
		(
			"%ProgramFiles%\\Filen.exe",
			Err(BrandingError::IconPathNotAbsolute),
		),
		(
			"C:\\%TEMP%\\Filen.exe",
			Err(BrandingError::IconPathCharacter),
		),
		(
			"C:\\a\u{0}b\\Filen.exe",
			Err(BrandingError::IconPathCharacter),
		),
		(
			"C:\\icons\\filen.png",
			Err(BrandingError::IconPathExtension),
		),
	];
	for (path, expected) in cases {
		assert_eq!(IconPath::parse(path).map(|_| ()), expected, "{path:?}");
	}
}

#[test]
fn an_icon_path_needs_an_extension_on_its_file_name() {
	assert_eq!(
		IconPath::parse("C:\\a.exe\\icon"),
		Err(BrandingError::IconPathExtension)
	);
	assert_eq!(
		IconPath::parse("C:\\icons\\exe"),
		Err(BrandingError::IconPathExtension)
	);
}

#[test]
fn an_icon_path_holds_1024_utf16_units_and_no_more() {
	let path = |units: usize| format!("C:\\{}.ico", "a".repeat(units - 7));
	assert_eq!(path(1024).encode_utf16().count(), 1024);
	assert!(IconPath::parse(&path(1024)).is_ok());
	assert_eq!(
		IconPath::parse(&path(1025)),
		Err(BrandingError::IconPathLength)
	);
}

#[test]
fn a_register_naming_nothing_has_no_branding_of_its_own() {
	assert_eq!(Branding::parse(None, None, None), Ok(None));
}

#[test]
fn a_register_naming_only_a_name_keeps_the_system_icon() {
	let branding = Branding::parse(Some("Filen (2)"), None, None)
		.unwrap()
		.unwrap();
	assert_eq!(branding.display_name.as_str(), "Filen (2)");
	assert_eq!(branding.icon, None);
	assert_eq!(branding.icon_index, 0);
}

#[test]
fn a_register_naming_only_an_icon_keeps_the_default_name() {
	let branding = Branding::parse(None, Some("C:\\Filen\\Filen.exe"), Some(-3))
		.unwrap()
		.unwrap();
	assert_eq!(branding.display_name.as_str(), "Filen");
	assert_eq!(
		branding.icon.as_ref().map(IconPath::as_str),
		Some("C:\\Filen\\Filen.exe")
	);
	assert_eq!(branding.icon_index, -3);
}

#[test]
fn an_icon_index_without_an_icon_path_is_refused() {
	assert_eq!(
		Branding::parse(Some("Filen"), None, Some(1)),
		Err(BrandingError::IndexWithoutPath)
	);
}
