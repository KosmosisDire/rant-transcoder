// Fetches the pinned FFmpeg into the target folder, links its shared libraries and puts them
// beside every exe the build makes, as the package ships them.

include!("ffmpeg.rs");

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=ffmpeg.rs");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // the target folder, which cargo marks with CACHEDIR.TAG, keeps one download for every profile
    let cache = out.ancestors().find(|p| p.join("CACHEDIR.TAG").is_file()).unwrap_or(&out).join("ffmpeg");
    let dir = ffmpeg_dir(&os, &arch, &cache).unwrap_or_else(|e| panic!("rant-transcoder: {e}"));
    // a removed or changed FFmpeg is fetched and placed again
    println!("cargo:rerun-if-changed={}", dir.display());
    // the tests make their clips with this FFmpeg's own ffmpeg tool
    println!("cargo:rustc-env=TRANSCODER_FFMPEG_DIR={}", dir.display());
    println!("cargo:rustc-link-search=native={}", dir.join("lib").display());
    for lib in ["avformat", "avfilter", "avcodec", "swscale", "avutil"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    if os == "linux" {
        // An RPATH, unlike a RUNPATH, also serves the libraries' own needs, such as avcodec's swresample
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags");
    }
    // OUT_DIR is <profile>/build/<package>/out, and exes land in <profile>, deps and examples
    let profile = out.ancestors().nth(3).expect("cargo's OUT_DIR layout");
    for exes in [profile.to_path_buf(), profile.join("deps"), profile.join("examples")] {
        place_runtime(&dir, &os, &exes).unwrap_or_else(|e| panic!("rant-transcoder: placing FFmpeg in {}: {e}", exes.display()));
    }
}

/// The libraries an exe loads at run time, linked or copied into `exes`.
fn place_runtime(ffmpeg: &Path, os: &str, exes: &Path) -> std::io::Result<()> {
    let (from, wanted): (_, fn(&str) -> bool) = match os {
        "windows" => (ffmpeg.join("bin"), |n| n.ends_with(".dll")),
        // the names the loader asks for, such as libavcodec.so.62
        _ => (ffmpeg.join("lib"), |n| n.split_once(".so.").is_some_and(|(_, v)| v.bytes().all(|b| b.is_ascii_digit()))),
    };
    std::fs::create_dir_all(exes)?;
    for entry in std::fs::read_dir(&from)? {
        let name = entry?.file_name();
        if !wanted(&name.to_string_lossy()) {
            continue;
        }
        // a Linux soname is a symlink, and the copy beside the exe is the file it names
        let source = std::fs::canonicalize(from.join(&name))?;
        let target = exes.join(&name);
        if std::fs::metadata(&target).is_ok_and(|m| m.len() == std::fs::metadata(&source).map_or(0, |s| s.len())) {
            continue;
        }
        let _ = std::fs::remove_file(&target);
        if std::fs::hard_link(&source, &target).is_err() {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}
