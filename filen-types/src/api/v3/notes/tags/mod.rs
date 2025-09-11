pub mod create;
pub mod delete;
pub mod favorite;
pub mod rename;

use serde::{Deserialize, Serialize};

use crate::api::v3::notes::NoteTag;

pub const ENDPOINT: &str = "v3/notes/tags";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response<'a>(pub Vec<NoteTag<'a>>);
