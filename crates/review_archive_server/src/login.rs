//! `review_archive login`: a member's access token from playbook, the way any OAuth client
//! gets one — registered on the fly (RFC 7591), PKCE, the browser sent back to a loopback
//! port (RFC 8252).

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use eyre::WrapErr;
use reqwest::Url;
use sha2::{Digest, Sha256};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
};

fn random() -> String {
	URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// `auth`: playbook's OAuth base, e.g. `https://valeratrades.com/playbook_mcp`. The access
/// token goes to stdout.
pub async fn login(auth: &str) -> eyre::Result<()> {
	let base = auth.trim_end_matches('/');
	let http = reqwest::Client::new();
	let listener = TcpListener::bind("127.0.0.1:0").await.wrap_err("binding a loopback port for the redirect")?;
	let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());

	#[derive(serde::Deserialize)]
	struct Registered {
		client_id: String,
	}
	// registered without the port, which a loopback redirect may change freely
	let registered: Registered = http
		.post(format!("{base}/register"))
		.json(&serde_json::json!({ "redirect_uris": ["http://127.0.0.1/callback"] }))
		.send()
		.await?
		.error_for_status()
		.wrap_err("registering with the authorization server")?
		.json()
		.await?;

	let verifier = random();
	let state = random();
	let mut authorize: Url = format!("{base}/authorize").parse()?;
	authorize
		.query_pairs_mut()
		.append_pair("response_type", "code")
		.append_pair("client_id", &registered.client_id)
		.append_pair("redirect_uri", &redirect)
		.append_pair("code_challenge", &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
		.append_pair("code_challenge_method", "S256")
		.append_pair("state", &state);
	eprintln!("open this in a browser and sign in:\n\n  {authorize}\n");

	let code = loop {
		let (mut conn, _) = listener.accept().await?;
		let mut buf = vec![0u8; 8192];
		let n = conn.read(&mut buf).await?;
		let head = String::from_utf8_lossy(&buf[..n]);
		let Some(target) = head.lines().next().and_then(|l| l.split(' ').nth(1)) else { continue };
		let url: Url = format!("http://127.0.0.1{target}").parse()?;
		if url.path() != "/callback" {
			continue;
		}
		let param = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
		let body = "signed in; this tab can be closed";
		conn.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes())
			.await?;
		eyre::ensure!(param("state").as_deref() == Some(state.as_str()), "the redirect came back with another state: not this login's");
		if let Some(e) = param("error") {
			eyre::bail!("the authorization server refused: {e}");
		}
		break param("code").ok_or_else(|| eyre::eyre!("the redirect came back without a code"))?;
	};

	#[derive(serde::Deserialize)]
	struct Tokens {
		access_token: String,
		expires_in: u64,
	}
	let tokens: Tokens = http
		.post(format!("{base}/token"))
		.form(&[
			("grant_type", "authorization_code"),
			("client_id", &registered.client_id),
			("code", &code),
			("code_verifier", &verifier),
			("redirect_uri", &redirect),
		])
		.send()
		.await?
		.error_for_status()
		.wrap_err("exchanging the code for a token")?
		.json()
		.await?;
	eprintln!("valid for {} min:", tokens.expires_in / 60);
	println!("{}", tokens.access_token);
	Ok(())
}
