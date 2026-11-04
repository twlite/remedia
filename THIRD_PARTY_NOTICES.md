# Third-party notices

Remedia is licensed under MIT; see `LICENSE`. Third-party dependencies retain their own terms.
This page highlights the major components. The accompanying `THIRD_PARTY_LICENSES.txt` reproduces
additional upstream copyright notices, conditions, and disclaimers required for redistribution,
including native Rust dependencies, Unicode data, and the Rust standard library. Keep both notice
files with redistributed packages and binaries.

## FFmpeg 7 — independently installed runtime libraries

Remedia dynamically links FFmpeg 7. Consumers install the shared libraries independently; they are
not bundled in Remedia's npm packages or standalone server binaries. FFmpeg's licensing depends on
the selected build, enabled components, and external libraries. GPL-enabled builds are not rejected.
Users must comply with applicable licensing requirements for their chosen build and deployment.
See [official licensing information](https://ffmpeg.org/legal.html) and
[FFmpeg source code](https://git.ffmpeg.org/ffmpeg.git).

The Rust `ffmpeg-next` and `ffmpeg-sys-next` bindings have separate WTFPL terms, reproduced in the
license appendix; those terms do not apply to FFmpeg itself.

## libopus — statically included

`opus2` 0.4.0 uses `libopus_sys` 0.3.3 with the bundled libopus backend. Its Opus source is compiled
and statically linked into the native addon and standalone server. A system libopus is not needed.
The bundled source's `opus/COPYING` is reproduced verbatim below. See
[Opus source](https://gitlab.xiph.org/xiph/opus) and [official licensing](https://opus-codec.org/license/).

```text
Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Opus is subject to the royalty-free patent licenses which are
specified at:

Xiph.Org Foundation:
https://datatracker.ietf.org/ipr/1524/

Microsoft Corporation:
https://datatracker.ietf.org/ipr/1914/

Broadcom Corporation:
https://datatracker.ietf.org/ipr/1526/
```

The bundled source also supplies this `opus/LICENSE_PLEASE_READ.txt`:

```text
Contributions to the collaboration shall not be considered confidential.

Each contributor represents and warrants that it has the right and
authority to license copyright in its contributions to the collaboration.

Each contributor agrees to license the copyright in the contributions
under the Modified (2-clause or 3-clause) BSD License or the Clear BSD License.

Please see the IPR statements submitted to the IETF for the complete
patent licensing details:

Xiph.Org Foundation:
https://datatracker.ietf.org/ipr/1524/

Microsoft Corporation:
https://datatracker.ietf.org/ipr/1914/

Skype Limited:
https://datatracker.ietf.org/ipr/1602/

Broadcom Corporation:
https://datatracker.ietf.org/ipr/1526/
```

## Discord voice and cryptography

[DAVE (`davey` 0.2.0)](https://crates.io/crates/davey/0.2.0) and
[OpenMLS 0.8.3](https://crates.io/crates/openmls/0.8.3) provide native voice encryption components
under their upstream MIT terms. Native TLS/crypto also incorporates
[ring 0.17.14](https://crates.io/crates/ring/0.17.14), whose Apache-2.0 and ISC notices include
BoringSSL-derived code. Their actual license and copyright notices are in `THIRD_PARTY_LICENSES.txt`.

HPKE components are MPL-2.0 covered and used unmodified. Corresponding source is available from
these exact published archives: [hpke-rs 0.6.1](https://crates.io/api/v1/crates/hpke-rs/0.6.1/download),
[hpke-rs-crypto 0.6.1](https://crates.io/api/v1/crates/hpke-rs-crypto/0.6.1/download),
[hpke-rs-libcrux 0.6.1](https://crates.io/api/v1/crates/hpke-rs-libcrux/0.6.1/download), and
[hpke-rs-rust-crypto 0.6.1](https://crates.io/api/v1/crates/hpke-rs-rust-crypto/0.6.1/download).
The MPL-2.0 text is in the license appendix. Redistributors must preserve source availability and
applicable notices, including for any modifications they make.

## JavaScript runtime dependency

The client uses [ws 8.22.1](https://registry.npmjs.org/ws/-/ws-8.22.1.tgz), installed separately by
npm under MIT terms. Its full copyright and license text is in `THIRD_PARTY_LICENSES.txt`.
The packages compile with `tsc`; development tools and documentation-site dependencies are not
bundled into the npm libraries or standalone server.

Review these files when dependencies, native build options, or distributed artifacts change.
Their presence does not guarantee legal compliance.
