# Distribution terms

Remedia's original Rust, TypeScript and JavaScript source remains available under
MIT (LICENSE). FFmpeg-independent npm packages remain MIT licensed.

The distributed native addons and server runtime executables statically incorporate
FFmpeg configured with --enable-gpl --enable-version3. These combined binaries are
distributed under GPL version 3 or later, with the obligations in COPYING.GPLv3.
The npm license declaration for native packages is SEE LICENSE IN LICENSES.md.
See THIRD_PARTY_NOTICES.md and THIRD_PARTY_LICENSES.txt for other acknowledgments.

## Obtain the source for your binary version

Use `remedia --version`, the native npm package version, or
`getFFmpegRuntime().remediaVersion` (`getFfmpegRuntime()` in v0.3.0). Open
https://github.com/twlite/remedia/releases/tag/vVERSION, replacing VERSION with
that value. For v0.3.0, download the unchanged corresponding-source.tar.gz and
follow its BUILD.txt. All existing v0.3.0 assets remain unchanged.

Releases after v0.3.0 provide source-manifest.json. Download it from that exact
release. Its releaseSource and dependencies entries identify the complete Remedia
source/configuration archive and the retained dependency snapshot, with immutable
URLs, sizes and SHA-256 digests. Download both archives, verify those values, and
follow SOURCE_DISTRIBUTION.md inside the release source archive. That document is
also a release asset and included in native npm packages. Server archives keep the
legacy entry layout for existing updaters; these retrieval directions are installed
in licenses/LICENSES.md alongside their other notices.

The source includes the exact Remedia revision and release changes, locked Rust
vendor sources, FFmpeg/native sources, patches, build scripts and target records.
Unchanged dependency snapshots are reused on this repository's source-only GitHub
releases. Both source parts are public before native npm publication, available
without login or charge, and must remain available while associated binaries are
distributed. A repository URL or upstream revision list alone is insufficient.

GPLv3 Section 6(d) permits equivalent network copying with clear directions next
to the object code; it does not require duplicating source trees per platform or
binary release. Redistributors must preserve notices, corresponding source access
and applicable installation information. Automated audits do not establish legal
compliance.
