# Testing

## Tests

```sh
cargo test
```

- Unit tests sit beside the code they test.
- `transcoder/tests/transcode.rs` runs a camera, the transcoder and a viewer in one process
  and checks which children exist, at what size and rate. It takes about 20 s.
- The camera and the viewer live in `transcoder/tests/common`, shared with the bench. The
  clips come from the pinned FFmpeg's own `ffmpeg` tool.
- Every node runs on domain 151 and on loopback only.

## Linux

Run the tests in `quay.io/pypa/manylinux_2_28_x86_64`, the image the Linux package builds in,
with rustup's minimal profile and `CARGO_TARGET_DIR` inside the container. There every
encoder and decoder is software.

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
that, once the children have settled.
