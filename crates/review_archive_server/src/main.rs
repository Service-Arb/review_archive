//! Composition root: settings, error monitoring and telemetry, then the CLI over the
//! library's `Archive`.

mod config;
mod settings;

use std::{path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand};
use ev_lib::error_monitoring;
use eyre::WrapErr;
use review_archive::{AddTarget, Archive};
use review_archive_core::{GbpLocation, TargetId, dto::RunStatus, parse_interval, parse_since, schedule};
use review_archive_server::{http, worker};
use tokio::sync::watch;

use crate::{config::Config, settings::Settings};

#[derive(Parser)]
#[command(name = "review_archive", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"), about = "Archive of public place reviews: a PNG of every review as it first appears, plus data for statistics")]
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

// Sentry must be initialised before the async runtime starts, hence no #[tokio::main].
fn main() -> eyre::Result<()> {
	color_eyre::install()?;

	// The deploy contract, straight out of the image: the gitops preflight diffs it with the
	// cluster Secret's keys, so a missing variable is caught before the rollout.
	if let Some(profile) = settings::print_required_vars_for() {
		for var in Settings::required_var_names(&profile) {
			println!("{var}");
		}
		return Ok(());
	}
	// Exits 78 (EX_CONFIG) on a bad environment, before anything else is built.
	let settings = ev_lib::settings::or_exit(Settings::from_env());

	// Held for the life of main: dropping it flushes. A no-op without SENTRY_DSN.
	let _sentry = error_monitoring::init(&error_monitoring::Config {
		dsn: settings.sentry_dsn.clone(),
		environment: settings.app_env.clone(),
		release: error_monitoring::release_name!().map(|r| r.into_owned()),
		// the same name OTEL uses, so an issue and its trace agree on the service
		service: std::env::var("OTEL_SERVICE_NAME").ok().filter(|s| !s.trim().is_empty()),
		traces_sample_rate: error_monitoring::Config::traces_sample_rate_for(&settings.app_env),
	});
	let _otel = init_tracing(&settings.app_env)?;

	let cli = Cli::parse();
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.wrap_err("building the tokio runtime")?
		.block_on(run(cli, settings))
}

/// Logs go to stderr, so a CLI command's stdout stays its output. OTLP export only when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
fn init_tracing(environment: &str) -> eyre::Result<Option<ev_lib::otel::Telemetry>> {
	use tracing_subscriber::{EnvFilter, fmt, prelude::*};

	let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(option_env!("LOG_DIRECTIVES").unwrap_or("info,chromiumoxide=error")))?;
	let (otel_guard, otel_layers) = ev_lib::otel::telemetry(&ev_lib::otel::Config {
		environment: environment.to_owned(),
		traces_sample_rate: ev_lib::otel::Config::traces_sample_rate_for(environment),
	})
	.unzip();
	tracing_subscriber::registry()
		.with(filter)
		.with(fmt::layer().with_writer(std::io::stderr))
		.with(error_monitoring::tracing_layer())
		.with(otel_layers)
		.init();
	Ok(otel_guard)
}

async fn run(cli: Cli, settings: Settings) -> eyre::Result<()> {
	// The image points TMPDIR into the data volume, which starts out empty; Chromium puts its
	// shared memory there (`--disable-dev-shm-usage`) and dies if the directory is missing.
	let tmp = std::env::temp_dir();
	std::fs::create_dir_all(&tmp).wrap_err_with(|| format!("creating the temp dir {}", tmp.display()))?;
	let mut config = Config::load(cli.config.as_deref())?;
	if let Cmd::Scan(args) = &cli.cmd {
		config.browser.dump_html = args.dump_html.clone();
	}
	let archive = Archive::open(config.archive(settings.secrets())).await?;

	match cli.cmd {
		Cmd::Target(cmd) => target_cmd(&archive, cmd).await,
		Cmd::Scan(args) => scan(&archive, args).await,
		Cmd::Serve => serve(archive, &config, settings.api_token()?).await,
		Cmd::Export { target, since, out } => {
			let since = since.as_deref().map(parse_since).transpose()?;
			let done = archive.export(TargetId(target), since, &out).await?;
			println!("{} reviews, {} PNGs → {}", done.reviews, done.pngs, out.display());
			Ok(())
		}
		Cmd::Stats { target, from, to, csv } => {
			let rows = archive.stats(target.map(TargetId), from, to).await?;
			if csv {
				print!("{}", review_archive_core::dto::stats_csv(&rows)?);
			} else {
				println!("{}", serde_json::to_string_pretty(&rows)?);
			}
			Ok(())
		}
	}
}

async fn target_cmd(archive: &Archive, cmd: TargetCmd) -> eyre::Result<()> {
	match cmd {
		TargetCmd::Add { place, label, lang, interval, gbp } => {
			let added = archive
				.add_target(AddTarget {
					place,
					label,
					lang,
					interval,
					gbp,
					disabled: false,
				})
				.await?;
			if let Some(r) = &added.resolved {
				println!(
					"resolved {:?} → {} ({})",
					r.query,
					r.place_id,
					[r.name.as_deref(), r.address.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(", ")
				);
			}
			println!("added target {}", added.target.id);
			Ok(())
		}
		TargetCmd::List => {
			for t in archive.targets().await? {
				let last = archive.store()?.last_run(t.id).await?;
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
		TargetCmd::Disable { id } => archive.set_enabled(TargetId(id), false).await,
		TargetCmd::Enable { id } => archive.set_enabled(TargetId(id), true).await,
	}
}

async fn scan(archive: &Archive, args: ScanArgs) -> eyre::Result<()> {
	let targets = match (args.target, args.all) {
		(Some(id), false) => vec![archive.target(TargetId(id)).await?],
		(None, true) => archive.targets().await?.into_iter().filter(|t| t.enabled).collect(),
		_ => eyre::bail!("name a target id, or pass --all"),
	};
	let mut failed = 0;
	for (i, t) in targets.iter().enumerate() {
		if i > 0 {
			tokio::time::sleep(schedule::pause(rand::random::<f64>())).await;
		}
		match archive.scan(t).await {
			Ok(s) => {
				if s.status == RunStatus::Failed {
					failed += 1;
				}
				println!("{s}");
			}
			Err(e) => {
				archive.close().await;
				return Err(e);
			}
		}
	}
	archive.close().await;
	eyre::ensure!(failed == 0, "{failed} of {} scans failed", targets.len());
	Ok(())
}

async fn serve(archive: Archive, config: &Config, token: &str) -> eyre::Result<()> {
	let bind = config.bind;
	let signals = std::sync::Arc::new(worker::Signals::default());
	let app = http::router(http::AppState::new(archive.clone(), token, signals.clone()));
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
	let deliver = worker::deliver(&archive, rx.clone());
	let work = async {
		let r = worker::run(&archive, &signals, rx).await;
		archive.close().await;
		r
	};
	tokio::try_join!(http, work, deliver, signal)?;
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
