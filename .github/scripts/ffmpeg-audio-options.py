#!/usr/bin/env python3
"""Keep built-in audio codecs and audio filters, plus all demuxers/protocols.

Derive the list from the pinned FFmpeg source rather than maintaining a lossy codec
allowlist. Shared dependencies are resolved by FFmpeg's configure script.
"""
import re
import sys
from pathlib import Path

source = Path(sys.argv[1])
codecs = {"decoder": set(), "encoder": set()}
externs = re.findall(r"extern const FFCodec ff_(\w+)_(decoder|encoder);", (source / "libavcodec/allcodecs.c").read_text())
for path in (source / "libavcodec").rglob("*.[ch]"):
    text = path.read_text(errors="replace")
    if path.name in ["pcm.c", "adpcm.c", "adpcmenc.c", "dpcm.c", "dsddec.c"]:
        for name, kind in externs:
            if re.search(r"\b" + re.escape(name) + r"\b", text) or (path.name == "pcm.c" and name.startswith("pcm_")):
                codecs[kind].add(name)
    for match in re.finditer(r"const FFCodec ff_(\w+)_(decoder|encoder)\s*=\s*\{(.*?)\n\};", text, re.S):
        if "AVMEDIA_TYPE_AUDIO" in match[3]:
            codecs[match[2]].add(match[1])

if not all(codecs.values()):
    raise SystemExit("Failed to enumerate pinned FFmpeg audio codecs")
print("--disable-decoders\n--disable-encoders\n--disable-filters")
for kind, names in codecs.items():
    print(f"--enable-{kind}=" + ",".join(sorted(names)))
filters = re.findall(r"extern const FFFilter ff_a(?:f|src|sink)_(\w+);", (source / "libavfilter/allfilters.c").read_text())
if not filters:
    raise SystemExit("Failed to enumerate pinned FFmpeg audio filters")
print("--enable-filter=" + ",".join(sorted(filters)))
