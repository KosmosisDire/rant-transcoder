use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use rant_transcoder::{Options, Transcoder, av};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Republishes the video on this host's Rant nodes as H264 and MJPEG any viewer can play, only
/// where that changes it: Parent/Video and Parent/Mjpeg at the source's size and rate unless the
/// source is that codec on the mesh already, Parent/Preview at 720p and Parent/MjpegPreview at 640
/// a side, lower quality too, each at 24 fps and only when the source is bigger.
#[derive(Parser)]
#[command(name = "rant-transcoder", version)]
struct Args {
    /// The Rant domain. 0 takes RANT_DOMAIN, else 0.
    #[arg(long, default_value_t = 0)]
    domain: u16,
    /// Use only this encoder, one of those --list-encoders shows.
    #[arg(long)]
    encoder: Option<String>,
    /// Show the H264 encoders it tries, best first, and what each makes on this host.
    #[arg(long)]
    list_encoders: bool,
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
    if a.list_encoders {
        list_encoders();
        return Ok(());
    }
    if let Some(e) = &a.encoder
        && !av::encoder_names().any(|n| n == e)
    {
        anyhow::bail!("there is no encoder {e}, it is one of {}", av::encoder_names().collect::<Vec<_>>().join(", "));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let on_signal = stop.clone();
    ctrlc::set_handler(move || on_signal.store(true, Ordering::Relaxed))?;
    let domain = a.domain;
    let mut t = Transcoder::new(Options { domain, encoder: a.encoder, interface: a.interface })?;
    info!("rant-transcoder {} on domain {domain}, FFmpeg {}", env!("CARGO_PKG_VERSION"), av::version());
    t.run(|| stop.load(Ordering::Relaxed));
    Ok(())
}

fn list_encoders() {
    for s in av::survey() {
        // how names the encoder first, then any device it uploads to
        let made = match &s.opened {
            None => "does not open here".to_string(),
            Some((how, Some(profile))) => format!("opens{}, {profile}", how.strip_prefix(s.name).unwrap_or(how)),
            Some((how, None)) => format!("opens{}, makes nothing", how.strip_prefix(s.name).unwrap_or(how)),
        };
        let in_place = s.in_place.map_or(String::new(), |d| format!(", takes {d} pictures in place"));
        println!("{:<14}{made}{in_place}", s.name);
    }
}
