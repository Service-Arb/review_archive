use std::{path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand};
use eyre::WrapErr;
use jiff::Timestamp;
use review_archive::{
	config::Config,
	domain::{GbpLocation, TargetId, TargetKind, parse_interval, schedule},
	export, http, places,
	runner::Runner,
	scheduler,
	store::{NewTarget, Store},
};
use tokio::sync::watch;

#[derive(Parser)]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"), about)]
struct Cli {
	/// TOML config: data dir, bind address, browser, defaults. Secrets come from the environment only.
	#[arg(long, global = true)]
	config: Option<PathBuf>,
	#[command(subcommand)]
	cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
	/// Manage the watched places.
	#[command(subcommand)]
	Target(TargetCmd),
	/// One pass now, with a summary per target.
	Scan(ScanArgs),
	/// Scheduler + HTTP API.
	Serve,
	/// PNGs + manifest.json of one target, to a directory or a .zip.
	Export {
		#[arg(long)]
		target: i64,
		/// YYYY-MM-DD or RFC 3339; filters on when a review was first seen.
		#[arg(long)]
		since: Option<String>,
		#[arg(long)]
		out: PathBuf,
	},
	/// Per target and per day: new, changed, gone, mean rating, rating histogram.
	Stats {
		#[arg(long)]
		target: Option<i64>,
		#[arg(long)]
		from: Option<jiff::civil::Date>,
		#[arg(long)]
		to: Option<jiff::civil::Date>,
		#[arg(long)]
		csv: bool,
	},
}

#[derive(Subcommand)]
enum TargetCmd {
	/// Watch a place, by place id or Google Maps URL.
	Add {
		place: String,
		#[arg(long)]
		label: Option<String>,
		/// UI language of the Maps page, e.g. fr.
		#[arg(long)]
		lang: Option<String>,
		/// e.g. 6h, 12h, 1d; at least 1h.
		#[arg(long, value_parser = parse_interval)]
		interval: Option<Duration>,
		/// Read reviews through the Business Profile API: <account>/<location>.
		#[arg(long)]
		gbp: Option<GbpLocation>,
	},
	List,
	Disable {
		id: i64,
	},
	Enable {
		id: i64,
	},
}

#[derive(Args)]
struct ScanArgs {
	target: Option<i64>,
	#[arg(long, conflicts_with = "target")]
	all: bool,
	/// Save the review cards' HTML of every step here, to refresh the parser fixtures.
	#[arg(long)]
	dump_html: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
	color_eyre::install()?;
	let filter =
		tracing_subscriber::EnvFilter::try_from_default_env().or_else(|_| tracing_subscriber::EnvFilter::try_new(option_env!("LOG_DIRECTIVES").unwrap_or("info,chromiumoxide=error")))?;
	tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();

	let cli = Cli::parse();
	let config = Config::load(cli.config.as_deref())?;
	let store = Store::open(&config.db_path()).await?;

	match cli.cmd {
		Cmd::Target(cmd) => target_cmd(&store, &config, cmd).await,
		Cmd::Scan(args) => scan(store, config, args).await,
		Cmd::Serve => serve(store, config).await,
		Cmd::Export { target, since, out } => {
			let since = since.as_deref().map(http::parse_since).transpose()?;
			let blobs = review_archive::store::blobs::BlobStore::new(config.blob_dir());
			let done = export::export(&store, &blobs, TargetId(target), since, &out, Timestamp::now()).await?;
			println!("{} reviews, {} PNGs → {}", done.reviews, done.pngs, out.display());
			Ok(())
		}
		Cmd::Stats { target, from, to, csv } => {
			let rows = store.stats(target.map(TargetId), from, to).await?;
			if csv {
				print!("{}", http::stats_csv(&rows)?);
			} else {
				println!("{}", serde_json::to_string_pretty(&rows)?);
			}
			Ok(())
		}
	}
}

async fn target_cmd(store: &Store, config: &Config, cmd: TargetCmd) -> eyre::Result<()> {
	match cmd {
		TargetCmd::Add { place, label, lang, interval, gbp } => {
			let interval = interval.unwrap_or(config.defaults.interval);
			eyre::ensure!(interval >= schedule::MIN_INTERVAL, "--interval must be at least 1h");
			let (place_id, found_name) = match places::parse(&place)? {
				places::Parsed::PlaceId(id) => (id, None),
				places::Parsed::Search { query, near } => {
					let key = std::env::var("GOOGLE_MAPS_KEY").wrap_err("the URL has no place id; resolving it needs GOOGLE_MAPS_KEY")?;
					let found = places::search(&reqwest::Client::new(), places::SEARCH_TEXT, &key, &query, near).await?;
					println!(
						"resolved {query:?} → {} ({})",
						found.place_id,
						[found.name.as_deref(), found.address.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(", ")
					);
					(found.place_id, found.name)
				}
			};
			let kind = if gbp.is_some() { TargetKind::Gbp } else { TargetKind::Maps };
			let label = label.or(found_name).unwrap_or_else(|| place_id.clone());
			let id = store
				.add_target(
					&NewTarget {
						label,
						kind,
						place_id,
						gbp,
						lang: lang.unwrap_or_else(|| config.defaults.lang.clone()),
						interval,
					},
					Timestamp::now(),
				)
				.await?;
			println!("added target {id}");
			Ok(())
		}
		TargetCmd::List => {
			for t in store.targets().await? {
				let last = store.last_run(t.id).await?;
				println!(
					"#{:<3} {:<4} {:<8} {:<24} {} lang={} every {}h{}{}",
					t.id,
					t.kind.as_str(),
					if t.enabled { "enabled" } else { "disabled" },
					t.label,
					t.place_id,
					t.lang,
					t.interval.as_secs() / 3600,
					t.gbp.map(|g| format!(" gbp={}/{}", g.account, g.location)).unwrap_or_default(),
					last.map(|l| format!(
						" last run {}{}",
						l.finished_at,
						if l.consecutive_failures > 0 {
							format!(" ({} failed in a row)", l.consecutive_failures)
						} else {
							String::new()
						}
					))
					.unwrap_or_default(),
				);
			}
			Ok(())
		}
		TargetCmd::Disable { id } => store.set_enabled(TargetId(id), false).await,
		TargetCmd::Enable { id } => store.set_enabled(TargetId(id), true).await,
	}
}

async fn scan(store: Store, config: Config, args: ScanArgs) -> eyre::Result<()> {
	let targets = match (args.target, args.all) {
		(Some(id), false) => vec![store.target(TargetId(id)).await?],
		(None, true) => store.targets().await?.into_iter().filter(|t| t.enabled).collect(),
		_ => eyre::bail!("name a target id, or pass --all"),
	};
	let runner = Runner::new(store, config, reqwest::Client::new(), args.dump_html);
	let mut failed = 0;
	for (i, t) in targets.iter().enumerate() {
		if i > 0 {
			tokio::time::sleep(schedule::pause(rand::random::<f64>())).await;
		}
		let summary = runner.scan(t).await;
		match summary {
			Ok(s) => {
				if s.status == review_archive::store::RunStatus::Failed {
					failed += 1;
				}
				println!("{s}");
			}
			Err(e) => {
				runner.end_pass().await;
				return Err(e);
			}
		}
	}
	runner.end_pass().await;
	eyre::ensure!(failed == 0, "{failed} of {} scans failed", targets.len());
	Ok(())
}

async fn serve(store: Store, config: Config) -> eyre::Result<()> {
	let token = std::env::var("REVIEW_ARCHIVE_TOKEN").wrap_err("REVIEW_ARCHIVE_TOKEN must be set for serve")?;
	eyre::ensure!(token.len() >= 16, "REVIEW_ARCHIVE_TOKEN is too short to be a secret (16+ characters)");
	let bind = config.bind;
	let runner = Runner::new(store.clone(), config, reqwest::Client::new(), None);
	let app = http::router(http::AppState::new(store, runner.blobs.clone(), &token));
	let listener = tokio::net::TcpListener::bind(bind).await.wrap_err_with(|| format!("binding {bind}"))?;
	tracing::info!(%bind, "serving");

	let (tx, rx) = watch::channel(false);
	let mut http_rx = rx.clone();
	let http = async move {
		axum::serve(listener, app)
			.with_graceful_shutdown(async move {
				// a dropped sender means shutdown too
				let _ = http_rx.wait_for(|stop| *stop).await;
			})
			.await
			.wrap_err("HTTP server")
	};
	let signal = async move {
		shutdown_signal().await;
		tracing::info!("shutting down");
		// every receiver is alive until both futures below end, so this cannot fail meaningfully
		let _ = tx.send(true);
		Ok::<_, eyre::Report>(())
	};
	let sched = async {
		let r = scheduler::run(&runner, rx).await;
		runner.end_pass().await;
		r
	};
	tokio::try_join!(http, sched, signal)?;
	Ok(())
}

async fn shutdown_signal() {
	let ctrl_c = async {
		if let Err(e) = tokio::signal::ctrl_c().await {
			tracing::error!(error = %e, "listening for Ctrl-C");
			std::future::pending::<()>().await;
		}
	};
	#[cfg(unix)]
	let term = async {
		match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
			Ok(mut s) => {
				s.recv().await;
			}
			Err(e) => {
				tracing::error!(error = %e, "listening for SIGTERM");
				std::future::pending::<()>().await;
			}
		}
	};
	#[cfg(not(unix))]
	let term = std::future::pending::<()>();
	tokio::select! {
		() = ctrl_c => {}
		() = term => {}
	}
}
