# rant-transcoder

Republishes the video on this host's Rant nodes as H264 that any viewer can play, only
where that changes it: `Parent/Video` unless the source is H264 on the mesh already,
`Parent/Preview` at 720p and 24 fps when the source is bigger than 720p.

```
rant-transcoder --domain 7
```

## Build

```
cmake --preset windows    # or linux, macos
cmake --build --preset windows
```
