//! Composition root: settings, error monitoring and telemetry, then the CLI over the
//! library's `Archive`.

mod config;
mod settings;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use ev_lib::{alerts, error_monitoring};
use eyre::WrapErr;
use review_archive::{Archive, SCANNER_VERSION, store::export::Destination};
use review_archive_core::{
	TargetId,
	dto::{ExportQuery, NewGmail, NewTarget, StatsQuery, TargetPatch},
	schedule::Schedule,
};
use review_archive_server::{auth::Auth, http, report, worker};
use tokio::sync::watch;

use crate::{config::AppConfig, settings::Settings};

#[derive(Parser)]
#[command(name = "review_archive", version = SCANNER_VERSION, about = "Archive of public place reviews: an AVIF screenshot of every review as it first appears, plus data for statistics")]
struct Cli {
	#[clap(flatten)]
	settings_flags: config::SettingsFlags,
	#[command(subcommand)]
	cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
	/// Write the defaults, diff against them, or export the JSON Schema / Nix module of the config.
	Config {
		#[command(subcommand)]
		cmd: config::SettingsCommand,
	},
	/// Manage the watched places.
	#[command(subcommand)]
	Target(TargetCmd),
	/// One pass now, with a summary per target.
	Scan(ScanArgs),
	/// Scheduler + HTTP API.
	Serve(ServeArgs),
	/// Members' managing gmails.
	#[command(subcommand)]
	Gmail(GmailCmd),
	/// Put a target under a managing gmail, for the member who owns it.
	Track { gmail: i64, target: i64 },
	/// Captures + manifest.json of one target, to a directory or a .zip.
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
		/// YYYY-MM-DD, inclusive.
		#[arg(long)]
		from: Option<String>,
		/// YYYY-MM-DD, inclusive.
		#[arg(long)]
		to: Option<String>,
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
		#[arg(long)]
		interval: Option<String>,
		/// Read reviews through the Business Profile API: `<account>/<location>`.
		#[arg(long)]
		gbp: Option<String>,
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
struct ServeArgs {
	/// Take every request as one made-up person holding these permissions, without the panel:
	/// an alias (`sa:admin`), `sa:review_archive:*` permissions comma-separated, or `none`. A
	/// local dashboard; loopback binds outside production only.
	#[arg(long)]
	dev_member: Option<String>,
}

#[derive(Subcommand)]
enum GmailCmd {
	/// Add a managing gmail to a member (their person id, `GET /members`).
	Add { member: i64, gmail: String },
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
	let cli = Cli::parse();
	if let Cmd::Config { cmd } = cli.cmd {
		AppConfig::handle_settings_command(cmd, cli.settings_flags);
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
	let mut config = AppConfig::load(cli.settings_flags.clone())?;
	if let Cmd::Scan(args) = &cli.cmd {
		config.browser.dump_html = args.dump_html.clone();
	}
	let alerts = match settings.alert_webhooks()? {
		Some(webhooks) => Some(alerts::alerts(alerts::Config {
			artifacts: alerts::Artifacts::open(config.archive(settings.secrets()).artifacts_dir().expect("the server always has a data dir"))?,
			webhooks,
			service: "review_archive".to_owned(),
		})),
		None => None,
	};
	let (alert_layer, deliverer) = alerts.unzip();
	let _otel = init_tracing(&settings.app_env, alert_layer)?;

	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.wrap_err("building the tokio runtime")?
		.block_on(with_alerts(deliverer, run(cli, config, settings)))
}

/// Runs `work`, delivering alerts beside it and, once it ends, the ones still queued.
async fn with_alerts<T>(deliverer: Option<alerts::Deliverer>, work: impl Future<Output = T>) -> T {
	let Some(deliverer) = deliverer else { return work.await };
	let done = tokio::sync::Notify::new();
	let work = async {
		let out = work.await;
		done.notify_one();
		out
	};
	tokio::join!(work, deliverer.run(done.notified())).0
}

/// Logs go to stderr, so a CLI command's stdout stays its output. OTLP export only when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set; alerts only when their webhooks are.
fn init_tracing(environment: &str, alerts: Option<alerts::AlertLayer>) -> eyre::Result<Option<ev_lib::otel::Telemetry>> {
	use tracing_subscriber::{EnvFilter, fmt, prelude::*};

	let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(option_env!("LOG_DIRECTIVES").unwrap_or("info")))?;
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
		.with(alerts)
		.init();
	Ok(otel_guard)
}

async fn run(cli: Cli, config: AppConfig, settings: Settings) -> eyre::Result<()> {
	// The image points TMPDIR into the data volume, which starts out empty; Chromium puts its
	// shared memory there (`--disable-dev-shm-usage`) and dies if the directory is missing.
	let tmp = std::env::temp_dir();
	std::fs::create_dir_all(&tmp).wrap_err_with(|| format!("creating the temp dir {}", tmp.display()))?;
	let archive = Archive::open(config.archive(settings.secrets())).await?;

	match cli.cmd {
		Cmd::Config { .. } => unreachable!("handled before the archive opens"),
		Cmd::Target(cmd) => target_cmd(&archive, cmd).await,
		Cmd::Scan(args) => scan(&archive, &config.schedule, args).await,
		Cmd::Serve(args) => serve(archive, &config, &settings, args).await,
		Cmd::Gmail(GmailCmd::Add { member, gmail }) => {
			let added = archive.add_gmail(review_archive_core::PersonId(member), &NewGmail { gmail }).await?;
			println!("added gmail {} ({})", added.id, added.gmail);
			Ok(())
		}
		Cmd::Track { gmail, target } => archive.assign(gmail, TargetId(target)).await,
		Cmd::Export { target, since, out } => {
			let done = archive.export(TargetId(target), &ExportQuery { since }, Destination::Path(out.clone())).await?;
			println!("{} reviews, {} captures → {}", done.reviews, done.captures, out.display());
			Ok(())
		}
		Cmd::Stats { target, from, to, csv } => {
			let rows = archive.stats(&StatsQuery { target, from, to }).await?;
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
				.add_target(&NewTarget {
					place: Some(place),
					label,
					lang,
					interval,
					gbp,
					..Default::default()
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
				let last = archive.last_run(t.id).await?;
				println!(
					"#{:<3} {:<4} {:<8} {:<24} {} lang={} every {}{}{}",
					t.id,
					t.kind.as_ref(),
					if t.enabled { "enabled" } else { "disabled" },
					t.label,
					t.place_id,
					t.lang,
					t.interval,
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
		TargetCmd::Disable { id } => set_enabled(archive, id, false).await,
		TargetCmd::Enable { id } => set_enabled(archive, id, true).await,
	}
}

async fn set_enabled(archive: &Archive, id: i64, enabled: bool) -> eyre::Result<()> {
	let patch = TargetPatch {
		enabled: Some(enabled),
		..Default::default()
	};
	archive.update_target(TargetId(id), &patch).await?;
	Ok(())
}

async fn scan(archive: &Archive, schedule: &Schedule, args: ScanArgs) -> eyre::Result<()> {
	let targets = match (args.target, args.all) {
		(Some(id), false) => vec![archive.target(TargetId(id)).await?],
		(None, true) => archive.targets().await?.into_iter().filter(|t| t.enabled).collect(),
		_ => eyre::bail!("name a target id, or pass --all"),
	};
	let mut failed = 0;
	for (i, t) in targets.iter().enumerate() {
		if i > 0 {
			tokio::time::sleep(schedule.pause(rand::random::<f64>())).await;
		}
		match archive.scan(t).await {
			Ok(r) => {
				if let Some(e) = r.failure {
					failed += 1;
					report(&e.wrap_err(format!("scan of target {} ({})", t.id, t.label)), "scan failed");
				}
				println!("{}", r.summary);
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

async fn serve(archive: Archive, config: &AppConfig, settings: &Settings, args: ServeArgs) -> eyre::Result<()> {
	let bind = config.bind;
	if args.dev_member.is_some() {
		eyre::ensure!(bind.ip().is_loopback(), "--dev-member signs everyone in: it serves on a loopback address only, not {bind}");
		eyre::ensure!(settings.app_env != "production", "--dev-member is for development, and APP_ENV is production");
	}
	let signals = std::sync::Arc::new(worker::Signals::default());
	// the page at `/` is the dev member's only: behind the panel, the panel's page mounts the bundle
	let (auth, sign_in) = match &args.dev_member {
		Some(held) => (
			Auth::Dev {
				sub: "dev".into(),
				permissions: dev_permissions(held)?,
			},
			Some("/".to_owned()),
		),
		None => (Auth::Panel(settings.panel_keys()?), None),
	};
	let app = http::router(
		http::AppState::new(archive.clone(), auth, signals.clone(), config.http.clone()),
		config.mfe_dir.as_deref(),
		sign_in.as_deref(),
	);
	let listener = tokio::net::TcpListener::bind(bind).await.wrap_err_with(|| format!("binding {bind}"))?;
	tracing::info!(%bind, "serving");
	match (&config.mfe_dir, &sign_in) {
		(Some(_), Some(_)) => println!("▶ dashboard: http://{bind}/{}", args.dev_member.map(|m| format!("  (signed in holding {m})")).unwrap_or_default()),
		_ => println!("▶ API: http://{bind}/"),
	}

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
	let deliver = worker::deliver(&archive, &config.worker, rx.clone());
	let mut stop = rx.clone();
	let work = async {
		let r = worker::run(&archive, &signals, &config.worker, &config.schedule, rx).await;
		archive.close().await;
		r
	};
	// resumable per blob: a shutdown midway leaves the rest for the next boot
	let convert = async {
		tokio::select! {
			r = archive.convert_png_blobs() => r,
			_ = stop.wait_for(|stopped| *stopped) => Ok(()),
		}
	};
	tokio::try_join!(http, work, deliver, convert, signal)?;
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

/// `--dev-member`: an alias's `sa:review_archive:*` part, such permissions listed, or `none`.
fn dev_permissions(held: &str) -> eyre::Result<sa_auth::PermissionSet> {
	let catalog = concierge_iam::Catalog::collect("sa", 0);
	let ours = |p: &String| p.starts_with(sa_auth::Service::ReviewArchive.prefix());
	if held == "none" {
		return Ok(std::iter::empty::<String>().collect());
	}
	if let Some(members) = catalog.aliases.get(held) {
		return Ok(members.iter().filter(|p| ours(p)).cloned().collect());
	}
	let listed: Vec<String> = held.split(',').map(|p| p.trim().to_owned()).collect();
	for p in &listed {
		eyre::ensure!(catalog.permissions.contains(p) && ours(p), "--dev-member: {p} is not a sa:review_archive permission, an alias, or `none`");
	}
	Ok(listed.into_iter().collect())
}
