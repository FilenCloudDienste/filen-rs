pub mod dir;
pub mod file;
pub mod meta;

pub trait FSObject {
	fn name(&self) -> &str;
	fn uuid(&self) -> &uuid::Uuid;
}

pub trait NonRootFSObject: FSObject {
	fn parent(&self) -> &uuid::Uuid;
	fn get_meta(&self) -> impl Metadata<'_>;
}

pub trait Metadata<'a> {
	fn make_string(&self) -> String;
}
