#!/bin/sh
# Packs the release build as the archive rant install unpacks: package.rant, the group, and
# bin/ with the exe and FFmpeg's libraries beside it, plus FFmpeg's licence.
# Usage: ci/archive.sh <platform>, such as linux-x64. Writes out/rant-transcoder-<version>-<platform>.tar.gz
set -e
platform=$1
version=$(sed -n 's/^version = "\(.*\)"/\1/p' transcoder/Cargo.toml | head -1)
name="rant-transcoder-$version-$platform"
rm -rf pkg && mkdir -p pkg/bin out
for f in target/release/rant-transcoder target/release/rant-transcoder.exe target/release/*.dll target/release/lib*.so.*; do
  if [ -f "$f" ]; then cp "$f" pkg/bin/; fi
done
test -f pkg/bin/rant-transcoder || test -f pkg/bin/rant-transcoder.exe
cp package.rant Transcoder.run pkg/
cp target/ffmpeg/*/LICENSE.txt pkg/FFmpeg-LICENSE.txt
tar -czf "out/$name.tar.gz" -C pkg package.rant Transcoder.run FFmpeg-LICENSE.txt bin
echo "out/$name.tar.gz"
