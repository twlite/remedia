#!/usr/bin/env python3
"""Deterministic source archives: stream files, normalize metadata, retain source symlinks."""
import gzip
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1]).resolve()
destination = pathlib.Path(sys.argv[2]).resolve()
with destination.open('wb') as output:
    with gzip.GzipFile(filename='', mode='wb', fileobj=output, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode='w', format=tarfile.PAX_FORMAT) as archive:
            for path in [root, *sorted(root.rglob('*'))]:
                info = archive.gettarinfo(str(path), arcname='.' if path == root else './' + path.relative_to(root).as_posix())
                info.uid = info.gid = info.mtime = 0
                info.uname = info.gname = ''
                info.pax_headers = {}
                if info.isfile():
                    with path.open('rb') as source:
                        archive.addfile(info, source)
                else:
                    archive.addfile(info)
