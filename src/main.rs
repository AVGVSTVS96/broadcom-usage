use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use broadcom_usage::app::{self, App};
use broadcom_usage::config::Config;
use broadcom_usage::event::Device;
use broadcom_usage::pricing::Pricing;
use broadcom_usage::server;
use broadcom_usage::store::Store;
use broadcom_usage::deck::Deck;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "broadcom-usage [serve|dump <out.ndjson>] [--config <file.json>]";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let config_path = match args.iter().position(|a| a == "--config") {
        Some(i) => {
            args.remove(i);
            Some(PathBuf::from(args.remove(i)))
        }
        None => None,
    };
    let config = Config::load(config_path.as_deref())?;
    std::fs::create_dir_all(&config.state_dir)?;
    let app = Arc::new(open(config)?);
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(app),
        Some("dump") => dump(&app, args.get(1).context(USAGE)?),
        _ => anyhow::bail!(USAGE),
    }
}

fn open(config: Config) -> Result<App> {
    let seed = config.go_usage_dir.as_ref().map(|d| d.join("pricing-state.json"));
    let pricing = Pricing::open(&config.state_dir, seed.as_deref());
    let local = Device { id: config.device_id()?, label: config.device_label(), synced_at: None };
    let store = Arc::new(Store::open(&config.state_dir.join("usage.db"), local, pricing.policy())?);
    let key = std::fs::read_to_string(&config.key_file).map(|k| k.trim().to_string()).unwrap_or_default();
    let deck = Arc::new(Deck::new(config.clone(), store.clone()));
    Ok(App { config, store, pricing: Arc::new(pricing), deck, key })
}

/// Catches up on every input, then writes the resolved records, as Go's TestDumpStore does.
fn dump(app: &App, out: &str) -> Result<()> {
    let started = std::time::Instant::now();
    let read = app.catch_up()?;
    eprintln!("caught up {read} lines in {:?}", started.elapsed());
    let started = std::time::Instant::now();
    let events = app.store.events()?;
    eprintln!("read {} records in {:?}", events.len(), started.elapsed());
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    for e in &events {
        serde_json::to_writer(&mut w, e)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    std::fs::write(format!("{out}.devices.json"), serde_json::to_vec_pretty(&app.store.devices()?)?)?;
    Ok(())
}

#[tokio::main]
async fn serve(app: Arc<App>) -> Result<()> {
    app::start(app.clone());
    let listener = tokio::net::TcpListener::bind(&app.config.listen).await?;
    tracing::info!("serving on {}", app.config.listen);
    axum::serve(listener, server::router(app)).await?;
    Ok(())
}
