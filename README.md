# rant-transcoder

Republishes the video on this host's Rant nodes as H264 and MJPEG any viewer can play.

```
rant install transcoder
```

## Build

```
cargo build --release
```

The first build fetches the pinned LGPL FFmpeg 8.1, whose libraries ship beside the exe.
