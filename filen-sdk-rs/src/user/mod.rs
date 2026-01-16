use crate::{api, auth::Client};

impl Client {
	pub async fn get_user_info(
		&self,
	) -> Result<filen_types::api::v3::user::info::Response<'_>, crate::error::Error> {
		api::v3::user::info::get(self.client()).await
	}
}
