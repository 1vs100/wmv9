#!/bin/sh
# Writes tests/reference/<sample>.md5: FFmpeg's MD5 of each decoded video
# frame of every testdata sample, in display order, one per line.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$root/tests/reference"
ffmpeg -version | head -n 1 >"$root/tests/reference/ffmpeg_version.txt"
cd "$root/testdata"
find . -name '*.wmv' | sort | while read -r f; do
    name=$(echo "${f#./}" | sed 's/[/.]/_/g')
    ffmpeg -nostdin -v error -threads 1 -i "$f" -map 0:v -fps_mode passthrough \
        -enc_time_base -1 -f framemd5 - |
        grep -v '^#' | awk -F', *' '{print $6}' >"$root/tests/reference/$name.md5"
    echo "$name: $(wc -l <"$root/tests/reference/$name.md5") frames"
done
