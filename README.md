# rant-transcoder

Republishes the video on this host's Rant nodes as H264 that any viewer can play:
`Parent/Video` at the source's size and rate, `Parent/Preview` at 720p and 24 fps.

```
rant-transcoder --domain 7
```

## Build

```
cmake --preset windows    # or linux, macos
cmake --build --preset windows
```
