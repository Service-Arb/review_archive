//! Places API (New) text search against a stub: what `target add <maps-url>` does when the
//! URL carries no place id.

use std::sync::{Arc, Mutex};

use axum::{
	Json, Router,
	extract::State,
	http::{HeaderMap, StatusCode},
	response::{IntoResponse, Response},
	routing::post,
};
use review_archive::{Rejected, places::search};
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Stub {
	got: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

async fn search_text(State(s): State<Stub>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
	let query = body["textQuery"].as_str().unwrap_or_default().to_owned();
	s.got.lock().unwrap().push((headers, body));
	if query == "Nowhere" {
		return Json(json!({})).into_response();
	}
	if query == "Broken" {
		return (StatusCode::FORBIDDEN, "API key not valid").into_response();
	}
	Json(json!({ "places": [
		{ "id": "ChIJprocopeprocopeproc", "displayName": { "text": "Le Procope" }, "formattedAddress": "13 Rue de l'Ancienne Comédie, Paris" },
		{ "id": "ChIJsecondsecondsecond" }
	] }))
	.into_response()
}

async fn stub() -> (Stub, String, tokio::task::JoinHandle<()>) {
	let s = Stub::default();
	let app = Router::new().route("/v1/places:searchText", post(search_text)).with_state(s.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}/v1/places:searchText", listener.local_addr().unwrap());
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	(s, url, server)
}

#[tokio::test]
async fn the_first_place_is_taken_and_the_search_is_biased_to_the_map() {
	let (s, url, server) = stub().await;
	let got = search(&reqwest::Client::new(), &url, "key-123", "Le Procope", Some((48.853, 2.3389))).await.unwrap();
	server.abort();
	assert_eq!(
		(got.place_id.as_str(), got.name.as_deref(), got.query.as_str()),
		("ChIJprocopeprocopeproc", Some("Le Procope"), "Le Procope")
	);
	let (headers, body) = s.got.lock().unwrap()[0].clone();
	assert_eq!(headers["x-goog-api-key"], "key-123");
	assert_eq!(headers["x-goog-fieldmask"], "places.id,places.displayName,places.formattedAddress");
	assert_eq!(body["locationBias"]["circle"]["center"], json!({ "latitude": 48.853, "longitude": 2.3389 }));
}

#[tokio::test]
async fn nothing_found_is_the_callers_mistake_and_a_refusal_is_not() {
	let (_s, url, server) = stub().await;
	let http = reqwest::Client::new();
	let nothing = search(&http, &url, "k", "Nowhere", None).await.unwrap_err();
	let refused = search(&http, &url, "k", "Broken", None).await.unwrap_err();
	server.abort();
	assert!(matches!(nothing.downcast_ref::<Rejected>(), Some(Rejected::Invalid(_))), "{nothing:#}");
	assert!(refused.downcast_ref::<Rejected>().is_none(), "{refused:#}");
	assert!(format!("{refused:#}").contains("403"), "{refused:#}");
}
