//! People: the concierge accounts the panel vouches for, by `sub`, and the addresses that held
//! rows before people had ids, waiting for their owner to sign in.

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{PersonId, fmt_ts};

use super::Store;

/// Who the panel says is calling.
#[derive(Clone, Debug)]
pub struct Seen<'a> {
	/// The concierge user id.
	pub sub: &'a str,
	/// Their concierge email.
	pub email: &'a str,
	/// Whether concierge verified it.
	pub email_verified: bool,
	/// Their concierge name.
	pub name: &'a str,
}

/// Who a sign-in is here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Claim {
	/// Who they are here.
	Person(PersonId),
	/// Rows wait under this address, and this sign-in cannot prove it owns them.
	Refused(Refusal),
}

/// Why a sign-in may not claim an address's rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Refusal {
	/// The address is not verified at concierge.
	#[error("the address is not verified")]
	Unverified,
	/// Another account already signed in with the same address.
	#[error("another account holds the same address")]
	Ambiguous,
}

/// A person, as the members' list shows them.
#[derive(sqlx::FromRow)]
pub struct Person {
	/// Their person id.
	pub id: i64,
	/// Their email as of their last sign-in.
	pub email: String,
	/// Empty until they sign in.
	pub name: String,
	/// `false`: an address from before people had ids, nobody signed in as it yet.
	pub claimed: bool,
}

impl Store {
	/// The person behind `seen`, made on first sight; an address's earlier rows go to the first
	/// verified sign-in with it, unless another account already holds it.
	pub async fn person(&self, seen: &Seen<'_>, now: Timestamp) -> eyre::Result<Claim> {
		let email = seen.email.trim().to_lowercase();
		let mut tx = self.write().await?;
		let known: Option<(i64, String, String)> = sqlx::query_as("SELECT id, email, name FROM people WHERE sub = ?")
			.bind(seen.sub)
			.fetch_optional(&mut *tx)
			.await
			.wrap_err("looking up a person")?;
		let id = match known {
			Some((id, old_email, old_name)) => {
				if old_email != email || old_name != seen.name {
					sqlx::query("UPDATE people SET email = ?, name = ? WHERE id = ?")
						.bind(&email)
						.bind(seen.name)
						.bind(id)
						.execute(&mut *tx)
						.await
						.wrap_err("renaming a person")?;
				}
				id
			}
			None => {
				let waiting: Option<i64> = sqlx::query_scalar("SELECT id FROM people WHERE sub IS NULL AND email = ?")
					.bind(&email)
					.fetch_optional(&mut *tx)
					.await
					.wrap_err("looking up an unclaimed address")?;
				match waiting {
					None => sqlx::query_scalar("INSERT INTO people (sub, email, name, first_seen) VALUES (?, ?, ?, ?) RETURNING id")
						.bind(seen.sub)
						.bind(&email)
						.bind(seen.name)
						.bind(fmt_ts(now))
						.fetch_one(&mut *tx)
						.await
						.wrap_err("adding a person")?,
					Some(_) if !seen.email_verified => return Ok(Claim::Refused(Refusal::Unverified)),
					Some(waiting) => {
						let held: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM people WHERE sub IS NOT NULL AND email = ?)")
							.bind(&email)
							.fetch_one(&mut *tx)
							.await
							.wrap_err("looking for another holder of an address")?;
						if held {
							return Ok(Claim::Refused(Refusal::Ambiguous));
						}
						sqlx::query("UPDATE people SET sub = ?, name = ? WHERE id = ?")
							.bind(seen.sub)
							.bind(seen.name)
							.bind(waiting)
							.execute(&mut *tx)
							.await
							.wrap_err("claiming an address")?;
						waiting
					}
				}
			}
		};
		tx.commit().await.wrap_err("committing a person")?;
		Ok(Claim::Person(PersonId(id)))
	}

	/// Everyone, by address.
	pub async fn people(&self) -> eyre::Result<Vec<Person>> {
		sqlx::query_as("SELECT id, email, name, sub IS NOT NULL AS claimed FROM people ORDER BY email, id")
			.fetch_all(&self.pool)
			.await
			.wrap_err("listing people")
	}

	/// `None`: nobody has this id.
	pub async fn person_by_id(&self, id: PersonId) -> eyre::Result<Option<Person>> {
		sqlx::query_as("SELECT id, email, name, sub IS NOT NULL AS claimed FROM people WHERE id = ?")
			.bind(id.0)
			.fetch_optional(&self.pool)
			.await
			.wrap_err("looking up a person by id")
	}
}
