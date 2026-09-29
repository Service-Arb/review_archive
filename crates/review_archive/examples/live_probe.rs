//! Repeats the live test's scan with a kept data dir per run, to look at how each run ended.
//! `cargo r --example live_probe -- <out_dir> <runs>`

use review_archive::{Archive, config::Config, core::dto::NewTarget};

#[tokio::main]
async fn main() {
	let mut args = std::env::args().skip(1);
	let out: std::path::PathBuf = args.next().expect("out dir").into();
	let runs: u32 = args.next().expect("run count").parse().unwrap();
	for i in 0..runs {
		let dir = out.join(format!("{}-{i}", jiff::Timestamp::now().as_second()));
		let mut config = Config {
			data_dir: Some(dir.clone()),
			..Config::default()
		};
		config.browser.executable = std::env::var_os("REVIEW_ARCHIVE_CHROME").map(Into::into);
		config.defaults.max_reviews_initial = 12;
		config.defaults.max_reviews_per_scan = 12;
		config.defaults.lang = "fr".into();
		let archive = Archive::open(config).await.unwrap();
		let added = archive
			.add_target(&NewTarget {
				place: Some("ChIJLU7jZClu5kcR4PcOOO6p3I0".into()),
				label: Some("Tour Eiffel".into()),
				interval: Some("6h".into()),
				..Default::default()
			})
			.await
			.unwrap();
		let summary = archive.scan_target(added.target.id).await.unwrap();
		archive.close().await;
		println!("=== run {i} [{}]\n{summary}", dir.display());
	}
}
