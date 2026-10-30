# Testing

## Tests

```sh
cargo test
```

- Unit tests sit beside the code they test.
- `every_path_keeps_black_and_white` in `src/av/color.rs` sends black and white through every
  decoder and encoder this machine has, on the GPU where an encoder takes pictures in place,
  and checks the H264 holds 16 and 235. `cargo test every_path -- --nocapture` lists the
  paths it took.
- `every_encoder_makes_constrained_baseline` in `src/av/output.rs` checks the profile each
  encoder's stream states. `rant-transcoder --list-encoders` shows the same on any host.
- `transcoder/tests/transcode.rs` runs a camera, the transcoder and a viewer in one process
  and checks which children exist, at what size and rate, once for H264 and once for MJPEG. It
  takes about a minute.
- `transcoder/tests/survive.rs` runs the transcoder as a process of its own and tries to break
  it: codecs, formats, sizes and clocks changing under a viewer, garbage, hung and short lived
  URLs, an RTSP camera of its own that restarts and hangs, killed cameras and transcoders, a
  second transcoder, viewers that come, go or never read, a flood of topics. It takes about
  three minutes.
- The camera and the viewer live in `transcoder/tests/common`, shared with the bench. The
  clips come from the pinned FFmpeg's own `ffmpeg` tool.
- Every node runs on domain 151 and on loopback only.

One more runs only when asked, since it takes minutes. Cameras and viewers come and go while it
prints memory, threads and handles. `CHURN_CYCLES` sets how long:

```sh
cargo test --release --test survive churn -- --ignored --nocapture
```

## Linux

Run the tests in `quay.io/pypa/manylinux_2_28_x86_64`, the image the Linux package builds in,
with rustup's minimal profile and `CARGO_TARGET_DIR` inside the container. Every encoder and
decoder is software there, unless `--gpus all` and `NVIDIA_DRIVER_CAPABILITIES=all` give the
container an NVIDIA GPU, which NVENC and CUDA then use. VAAPI cannot be reached from a
container on Windows.

## Costs

`examples/bench.rs` publishes the test's sources and watches each child, then prints the
rate each reached. Run a transcoder beside it as a process of its own and read that
process's CPU time:

```sh
cargo build --release --bin rant-transcoder --example bench
target/release/examples/bench 45 &
target/release/rant-transcoder --domain 151 --interface 127.0.0.1
```

The bench makes its clips first and prints "watching" when its sources start. Measure after
that, once the children have settled. `bench 45 mjpeg` watches the MJPEG children instead.

On a CPU with performance and efficiency cores, pin the transcoder to the performance cores
and the bench to the others. Unpinned, the same build measured 0.46 to 0.82 cores in five runs
on an i9-13900HX, pinned 0.37 to 0.39.
