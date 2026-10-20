use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use rant_transcoder::{Options, Transcoder, av};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Republishes the video on this host's Rant nodes as H264 any viewer can play, only where
/// that changes it: Parent/Video at the source's size and rate unless the source is H264 on
/// the mesh already, Parent/Preview at 720p and 24 fps when the source is bigger than 720p.
#[derive(Parser)]
#[command(name = "rant-transcoder", version)]
struct Args {
    /// The Rant domain. 0 takes RANT_DOMAIN, else 0.
    #[arg(long, default_value_t = 0)]
    domain: u16,
    /// Use only this FFmpeg encoder, such as h264_nvenc or libopenh264.
    #[arg(long)]
    encoder: Option<String>,
    /// Join the mesh on one interface only, 127.0.0.1 for this host alone.
    #[arg(long)]
    interface: Option<String>,
    /// Let FFmpeg print its warnings.
    #[arg(long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let a = Args::parse();
    av::check().map_err(anyhow::Error::msg)?;
    av::set_log_level(a.verbose);
    let stop = Arc::new(AtomicBool::new(false));
    let on_signal = stop.clone();
    ctrlc::set_handler(move || on_signal.store(true, Ordering::Relaxed))?;
    let domain = a.domain;
    let mut t = Transcoder::new(Options { domain, encoder: a.encoder, interface: a.interface })?;
    info!("rant-transcoder {} on domain {domain}, FFmpeg {}", env!("CARGO_PKG_VERSION"), av::version());
    t.run(|| stop.load(Ordering::Relaxed));
    Ok(())
}
