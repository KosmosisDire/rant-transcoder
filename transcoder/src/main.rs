use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Republishes the video on this host's Rant nodes as H264 any viewer can play, only where
/// that changes it.
#[derive(Parser)]
#[command(name = "rant-transcoder", version)]
struct Args {}

fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let _args = Args::parse();
    rant_transcoder::av::check().map_err(anyhow::Error::msg)?;
    info!("rant-transcoder {} on FFmpeg {}", env!("CARGO_PKG_VERSION"), rant_transcoder::av::version());
    Ok(())
}
