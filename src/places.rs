//! A place id from what a person pastes: the id itself, or a Maps URL.

use eyre::WrapErr;
use serde::Deserialize;

pub const SEARCH_TEXT: &str = "https://places.googleapis.com/v1/places:searchText";
const FIELD_MASK: &str = "places.id,places.displayName,places.formattedAddress";

/// What the input says without asking anyone.
#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
	PlaceId(String),
	/// A URL with no id in it; searching needs the name and, when present, where the map was.
	Search {
		query: String,
		near: Option<(f64, f64)>,
	},
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
	pub place_id: String,
	pub name: Option<String>,
	pub address: Option<String>,
}

pub fn parse(input: &str) -> eyre::Result<Parsed> {
	let input = input.trim();
	if !input.contains("://") {
		eyre::ensure!(looks_like_place_id(input), "{input:?} is neither a place id nor a URL");
		return Ok(Parsed::PlaceId(input.to_owned()));
	}
	let url = url::Url::parse(input).wrap_err_with(|| format!("parsing {input:?}"))?;
	for (k, v) in url.query_pairs() {
		let candidate = match k.as_ref() {
			"query_place_id" | "place_id" => Some(v.to_string()),
			"q" | "query" => v.strip_prefix("place_id:").map(str::to_owned),
			_ => None,
		};
		if let Some(id) = candidate.filter(|c| looks_like_place_id(c)) {
			return Ok(Parsed::PlaceId(id));
		}
	}

	// /maps/place/<Name>/@<lat>,<lng>,<zoom>z/...
	let segments: Vec<String> = url.path_segments().map(|s| s.map(|p| percent_decode(p).replace('+', " ")).collect()).unwrap_or_default();
	let name = segments
		.iter()
		.position(|s| s == "place")
		.and_then(|i| segments.get(i + 1))
		.filter(|s| !s.is_empty() && !s.starts_with('@'))
		.cloned();
	let near = segments.iter().find_map(|s| {
		let mut it = s.strip_prefix('@')?.split(',');
		Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
	});
	let query = name.ok_or_else(|| eyre::eyre!("no place id and no place name in {input:?}"))?;
	Ok(Parsed::Search { query, near })
}

/// Place ids are URL-safe base64-ish tokens, `ChIJ…` for most places.
fn looks_like_place_id(s: &str) -> bool {
	s.len() >= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn percent_decode(s: &str) -> String {
	url::form_urlencoded::parse(format!("x={s}").as_bytes())
		.next()
		.map(|(_, v)| v.into_owned())
		.unwrap_or_else(|| s.to_owned())
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
		.wrap_err("Places text search")?;
	let status = resp.status();
	if !status.is_success() {
		let text = resp.text().await.unwrap_or_default();
		eyre::bail!("Places text search for {query:?}: {status}: {text}");
	}
	let resp: Resp = resp.json().await.wrap_err("decoding Places response")?;
	let place = resp.places.into_iter().next().ok_or_else(|| eyre::eyre!("Places found nothing for {query:?}"))?;
	Ok(Resolved {
		place_id: place.id,
		name: place.display_name.map(|t| t.text),
		address: place.formatted_address,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ids_and_urls() {
		let id = "ChIJLU7jZClu5kcR4PcOOO6p3I0";
		assert_eq!(parse(id).unwrap(), Parsed::PlaceId(id.into()));
		assert_eq!(parse(&format!("https://www.google.com/maps/place/?q=place_id:{id}&hl=fr")).unwrap(), Parsed::PlaceId(id.into()));
		assert_eq!(
			parse(&format!("https://www.google.com/maps/search/?api=1&query=Eiffel&query_place_id={id}")).unwrap(),
			Parsed::PlaceId(id.into())
		);
		assert!(parse("cafe").is_err());
	}

	#[test]
	fn url_without_id_becomes_a_search() {
		let got = parse("https://www.google.com/maps/place/Le+Procope/@48.8530,2.3389,17z/data=!3m1!4b1").unwrap();
		assert_eq!(
			got,
			Parsed::Search {
				query: "Le Procope".into(),
				near: Some((48.853, 2.3389)),
			}
		);
		let accented = parse("https://www.google.com/maps/place/Caf%C3%A9+de+Flore/@48.854,2.332,17z").unwrap();
		assert!(matches!(accented, Parsed::Search { ref query, .. } if query == "Café de Flore"));
	}
}
