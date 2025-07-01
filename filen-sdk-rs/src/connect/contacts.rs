use filen_types::{
	api::v3::contacts::{
		Contact,
		requests::{r#in::ContactRequestIn, out::ContactRequestOut},
	},
	fs::UuidStr,
};

use crate::{api, auth::Client, error::Error};

impl Client {
	pub async fn get_contacts(&self) -> Result<Vec<Contact>, Error> {
		api::v3::contacts::get(self.client()).await.map(|r| r.0)
	}

	pub async fn send_contact_request(&self, email: &str) -> Result<UuidStr, Error> {
		Ok(api::v3::contacts::requests::send::post(
			self.client(),
			&api::v3::contacts::requests::send::Request {
				email: std::borrow::Cow::Borrowed(email),
			},
		)
		.await?
		.uuid)
	}

	pub async fn cancel_contact_request(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		api::v3::contacts::requests::out::delete::post(
			self.client(),
			&api::v3::contacts::requests::out::delete::Request { uuid: contact_uuid },
		)
		.await
	}

	pub async fn accept_contact_request(&self, contact_uuid: UuidStr) -> Result<UuidStr, Error> {
		Ok(api::v3::contacts::requests::accept::post(
			self.client(),
			&api::v3::contacts::requests::accept::Request { uuid: contact_uuid },
		)
		.await?
		.uuid)
	}

	pub async fn deny_contact_request(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		api::v3::contacts::requests::deny::post(
			self.client(),
			&api::v3::contacts::requests::deny::Request { uuid: contact_uuid },
		)
		.await
	}

	pub async fn delete_contact(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		api::v3::contacts::delete::post(
			self.client(),
			&api::v3::contacts::delete::Request { uuid: contact_uuid },
		)
		.await
	}

	pub async fn list_incoming_contact_requests(&self) -> Result<Vec<ContactRequestIn>, Error> {
		api::v3::contacts::requests::r#in::get(self.client())
			.await
			.map(|r| r.0)
	}

	pub async fn list_outgoing_contact_requests(&self) -> Result<Vec<ContactRequestOut>, Error> {
		api::v3::contacts::requests::out::get(self.client())
			.await
			.map(|r| r.0)
	}
}
