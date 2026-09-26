//! The Business Profile client against a stub server: pagination and token refresh.

use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use axum::{
	Form, Json, Router,
	extract::{Query, State},
	http::{HeaderMap, StatusCode},
	response::{IntoResponse, Response},
	routing::{get, post},
};
use review_archive::{
	domain::GbpLocation,
	sources::gbp::{Client, Credentials},
};
use serde_json::json;

#[derive(Default)]
struct Stub {
	tokens_issued: AtomicUsize,
	/// The access token the API accepts; bumped to expire the old one.
	valid_generation: AtomicUsize,
	pages_served: AtomicUsize,
}

async fn token(State(s): State<Arc<Stub>>, Form(f): Form<Vec<(String, String)>>) -> Response {
	let get = |k: &str| f.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
	if get("grant_type") != Some("refresh_token") || get("refresh_token") != Some("refresh-me") || get("client_secret") != Some("shh") {
		return (StatusCode::BAD_REQUEST, "bad grant").into_response();
	}
	let n = s.tokens_issued.fetch_add(1, Ordering::SeqCst);
	Json(json!({ "access_token": format!("tok{n}"), "expires_in": 3599, "token_type": "Bearer" })).into_response()
}

async fn reviews(State(s): State<Arc<Stub>>, headers: HeaderMap, Query(q): Query<Vec<(String, String)>>) -> Response {
	let want = format!("Bearer tok{}", s.valid_generation.load(Ordering::SeqCst));
	if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
		return StatusCode::UNAUTHORIZED.into_response();
	}
	assert!(q.iter().any(|(k, v)| k == "pageSize" && v == "50"));
	let page = q.iter().find(|(k, _)| k == "pageToken").map(|(_, v)| v.as_str());
	s.pages_served.fetch_add(1, Ordering::SeqCst);
	match page {
		None => {
			// the first token expires right after the first page
			s.valid_generation.fetch_add(1, Ordering::SeqCst);
			Json(json!({
				"reviews": [
					{ "reviewId": "r1", "reviewer": { "displayName": "Ann" }, "starRating": "FIVE", "comment": "Lovely", "createTime": "2026-09-01T10:00:00Z" },
					{ "reviewId": "r2", "reviewer": { "displayName": "Bob" }, "starRating": "TWO", "createTime": "2026-08-01T10:00:00Z",
					  "reviewReply": { "comment": "Sorry, Bob" } }
				],
				"nextPageToken": "p2"
			}))
			.into_response()
		}
		Some("p2") => Json(json!({
			"reviews": [ { "reviewId": "r3", "reviewer": { "displayName": "Cy" }, "starRating": "THREE", "comment": "Fine" } ],
			"totalReviewCount": 3
		}))
		.into_response(),
		Some(other) => (StatusCode::BAD_REQUEST, format!("unknown page {other}")).into_response(),
	}
}

#[tokio::test]
async fn paginates_and_refreshes_an_expired_token() {
	let stub = Arc::new(Stub::default());
	let app = Router::new()
		.route("/token", post(token))
		.route("/v4/accounts/{a}/locations/{l}/reviews", get(reviews))
		.with_state(stub.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

	let client = Client::with_endpoints(
		reqwest::Client::new(),
		Credentials {
			client_id: "id".into(),
			client_secret: "shh".into(),
			refresh_token: "refresh-me".into(),
		},
		&format!("http://{addr}/token"),
		&format!("http://{addr}"),
	);
	let got = client
		.reviews(&GbpLocation {
			account: "1".into(),
			location: "2".into(),
		})
		.await
		.unwrap();
	server.abort();

	assert_eq!(got.total, Some(3));
	let got = got.reviews;
	assert_eq!(got.iter().map(|r| r.review_id.as_str()).collect::<Vec<_>>(), ["r1", "r2", "r3"]);
	assert_eq!(stub.tokens_issued.load(Ordering::SeqCst), 2, "one token, then one refresh after the 401");
	// page 1 and the retried page 2; the rejected attempt is refused before it counts
	assert_eq!(stub.pages_served.load(Ordering::SeqCst), 2);
	let o = review_archive::sources::gbp::observed(got[1].clone());
	assert_eq!((o.rating, o.reply.as_deref(), o.text), (Some(2), Some("Sorry, Bob"), None));
}

#[tokio::test]
async fn a_bad_refresh_token_is_an_error_not_a_loop() {
	let stub = Arc::new(Stub::default());
	let app = Router::new().route("/token", post(token)).with_state(stub.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	let client = Client::with_endpoints(
		reqwest::Client::new(),
		Credentials {
			client_id: "id".into(),
			client_secret: "wrong".into(),
			refresh_token: "refresh-me".into(),
		},
		&format!("http://{addr}/token"),
		&format!("http://{addr}"),
	);
	let err = client
		.reviews(&GbpLocation {
			account: "1".into(),
			location: "2".into(),
		})
		.await
		.unwrap_err();
	server.abort();
	assert!(format!("{err:#}").contains("400"), "{err:#}");
}
