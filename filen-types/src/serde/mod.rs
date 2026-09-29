pub(crate) mod boolean;
pub mod cow;
pub mod number;
pub(crate) mod option;
pub(crate) mod parent_uuid;
pub mod rsa;
// The fixed-size string newtypes cast already-validated bytes in place, and check archived
// bytes for rkyv (an unsafe trait).
#[allow(unsafe_code)]
pub mod str;
pub mod time;
pub(crate) mod uuid;
