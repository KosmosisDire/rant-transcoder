// The pinned FFmpeg, shared by build.rs and ffigen so both read the same build. It is BtbN's
// LGPL shared 8.1, a month end build since those are kept. FFMPEG_DIR names another.

const FFMPEG_RELEASE: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-09-30-13-08";
const FFMPEG_NAME: &str = "ffmpeg-n8.1.3-9-g29e619e767";

/// The archive's platform word and extension for a target, and its SHA-256 from the
/// release's checksums.sha256.
fn ffmpeg_asset(os: &str, arch: &str) -> Result<(&'static str, &'static str, &'static str), String> {
    Ok(match (os, arch) {
        ("windows", "x86_64") => ("win64", "zip", "3e47bda1607740550141e37c0e49d1e5182b34699f15adfd137ee266d346811a"),
        ("windows", "aarch64") => ("winarm64", "zip", "7674d11915c95c8f65eeec36b329917c3023f6bae5786549cc47aed52cc1cea7"),
        ("linux", "x86_64") => ("linux64", "tar.xz", "d4d7b6936f492c0b866b1cb4a29a1bbae09d6a064b4ec4eb90c95bed7969535a"),
        ("linux", "aarch64") => ("linuxarm64", "tar.xz", "d44838f375c66cf31a8b1473b6f8bad76e5c7dac55870c4d96a75c907a52ba2d"),
        _ => return Err(format!("no pinned FFmpeg for {os} {arch}: set FFMPEG_DIR to an FFmpeg 8.1 with include and lib")),
    })
}

/// The FFmpeg to build against: FFMPEG_DIR when set, else the pinned build for the target
/// under cache, downloaded and checked the first time.
fn ffmpeg_dir(os: &str, arch: &str, cache: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if let Some(dir) = std::env::var_os("FFMPEG_DIR") {
        return Ok(dir.into());
    }
    let (platform, ext, sha256) = ffmpeg_asset(os, arch)?;
    let stem = format!("{FFMPEG_NAME}-{platform}-lgpl-shared-8.1");
    let dir = cache.join(&stem);
    if dir.join("include").is_dir() {
        return Ok(dir);
    }
    std::fs::create_dir_all(cache).map_err(|e| format!("cannot make {}: {e}", cache.display()))?;
    // names of this process alone, so two builds fetching at once never meet
    let archive = cache.join(format!("{stem}.{}.{ext}", std::process::id()));
    let unpacked = cache.join(format!("{stem}.{}", std::process::id()));
    let url = format!("{FFMPEG_RELEASE}/{stem}.{ext}");
    let fetched = ffmpeg_run(&ffmpeg_tool("curl"), &["-fsSL", "--retry", "3", "-o", &archive.to_string_lossy(), &url]);
    let result = fetched
        .and_then(|()| ffmpeg_check(&archive, sha256))
        .and_then(|()| std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string()))
        .and_then(|()| ffmpeg_run(&ffmpeg_tool("tar"), &["-xf", &archive.to_string_lossy(), "-C", &unpacked.to_string_lossy()]))
        .and_then(|()| match std::fs::rename(unpacked.join(&stem), &dir) {
            Ok(()) => Ok(()),
            Err(_) if dir.join("include").is_dir() => Ok(()),
            Err(e) => Err(format!("cannot move the unpacked FFmpeg to {}: {e}", dir.display())),
        });
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&unpacked);
    result.map_err(|e| format!("fetching {url}: {e}"))?;
    Ok(dir)
}

/// curl and tar as Windows ships them, since a Git Bash PATH puts a tar first that cannot read zip.
fn ffmpeg_tool(name: &str) -> std::path::PathBuf {
    match std::env::var_os("SystemRoot") {
        Some(root) if cfg!(windows) => std::path::Path::new(&root).join("System32").join(format!("{name}.exe")),
        _ => name.into(),
    }
}

fn ffmpeg_run(tool: &std::path::Path, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new(tool).args(args).status().map_err(|e| format!("cannot run {}: {e}", tool.display()))?;
    if status.success() { Ok(()) } else { Err(format!("{} {} failed with {status}", tool.display(), args.join(" "))) }
}

fn ffmpeg_check(archive: &std::path::Path, sha256: &str) -> Result<(), String> {
    use sha2::Digest;
    let bytes = std::fs::read(archive).map_err(|e| e.to_string())?;
    let digest = sha2::Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if hex == sha256 { Ok(()) } else { Err(format!("SHA-256 {hex}, the pin says {sha256}")) }
}
