//! A place id from what a person pastes: the id itself, or a Maps URL. Resolving a URL
//! without an id takes a Places API search, which is the engine's job.

use eyre::WrapErr;

/// What the input says without asking anyone.
#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
	/// The input is, or contains, a place id.
	PlaceId(String),
	/// A URL with no id in it; searching needs the name and, when present, where the map was.
	Search {
		/// The place's name from the URL path.
		query: String,
		/// `(lat, lng)` of the map the URL was showing.
		near: Option<(f64, f64)>,
	},
}

/// Reads a place id or a Google Maps URL.
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
pub fn looks_like_place_id(s: &str) -> bool {
	s.len() >= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn percent_decode(s: &str) -> String {
	url::form_urlencoded::parse(format!("x={s}").as_bytes())
		.next()
		.map(|(_, v)| v.into_owned())
		.unwrap_or_else(|| s.to_owned())
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
