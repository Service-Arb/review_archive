//! A place id from what a person pastes. The id, or a Maps URL carrying it, needs nothing;
//! a URL with only a name in it is looked up with the Places API (New).

use review_archive_core::{
	Rejected,
	place::{self, Parsed},
};
use serde::Deserialize;

use crate::PlacesError;

/// Places API (New) text search.
pub const SEARCH_TEXT: &str = "https://places.googleapis.com/v1/places:searchText";
const FIELD_MASK: &str = "places.id,places.displayName,places.formattedAddress";

/// What a search found.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
	/// The Google place id.
	pub place_id: String,
	/// Its display name.
	pub name: Option<String>,
	/// Its address.
	pub address: Option<String>,
	/// What was searched for.
	pub query: String,
}

/// The place id in `input`, searching for it when the input has only a name. `Some`
/// resolution when a search happened. `key` is the Places API key, needed only then.
pub async fn resolve(http: &reqwest::Client, key: Option<&str>, input: &str) -> eyre::Result<(String, Option<Resolved>)> {
	let expanded;
	let input = if input.trim().starts_with("https://maps.app.goo.gl/") {
		expanded = expand_short_link(input.trim()).await?;
		expanded.as_str()
	} else {
		input
	};
	match place::parse(input).map_err(|e| Rejected::invalid(format!("{e:#}")))? {
		Parsed::PlaceId(id) => Ok((id, None)),
		Parsed::Search { query, near } => {
			let key = key.ok_or_else(|| Rejected::invalid("the URL has no place id; resolving it needs GOOGLE_MAPS_KEY"))?;
			let found = search(http, SEARCH_TEXT, key, &query, near).await?;
			Ok((found.place_id.clone(), Some(found)))
		}
	}
}

/// One hop only: following further can land on consent.google.com instead of the place.
async fn expand_short_link(url: &str) -> eyre::Result<String> {
	let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().map_err(PlacesError::from)?;
	let resp = client.get(url).send().await.map_err(PlacesError::from)?;
	let location = resp
		.headers()
		.get(reqwest::header::LOCATION)
		.ok_or_else(|| Rejected::invalid(format!("{url:?} answered {} without a redirect", resp.status())))?;
	Ok(location
		.to_str()
		.map_err(|e| Rejected::invalid(format!("{url:?} redirected to a non-UTF-8 location: {e}")))?
		.to_owned())
}

/// Places API (New) text search, first hit.
pub async fn search(http: &reqwest::Client, endpoint: &str, key: &str, query: &str, near: Option<(f64, f64)>) -> eyre::Result<Resolved> {
	#[derive(Deserialize)]
	struct Resp {
		#[serde(default)]
		places: Vec<Place>,
	}
	#[derive(Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Place {
		id: String,
		display_name: Option<Text>,
		formatted_address: Option<String>,
	}
	#[derive(Deserialize)]
	struct Text {
		text: String,
	}

	let mut body = serde_json::json!({ "textQuery": query, "pageSize": 1 });
	if let Some((lat, lng)) = near {
		body["locationBias"] = serde_json::json!({ "circle": { "center": { "latitude": lat, "longitude": lng }, "radius": 500.0 } });
	}
	let resp = http
		.post(endpoint)
		.header("X-Goog-Api-Key", key)
		.header("X-Goog-FieldMask", FIELD_MASK)
		.json(&body)
		.send()
		.await
		.map_err(PlacesError::from)?;
	let status = resp.status();
	if !status.is_success() {
		let body = resp.text().await.map_err(PlacesError::from)?;
		return Err(PlacesError::new_api(query.to_owned(), status.as_u16(), body).into());
	}
	let resp: Resp = resp.json().await.map_err(PlacesError::from)?;
	let place = resp.places.into_iter().next().ok_or_else(|| Rejected::invalid(format!("Places found nothing for {query:?}")))?;
	Ok(Resolved {
		place_id: place.id,
		name: place.display_name.map(|t| t.text),
		address: place.formatted_address,
		query: query.to_owned(),
	})
}
