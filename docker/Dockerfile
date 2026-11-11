# syntax=docker/dockerfile:1

# Standalone Remedia server image.
#
# The image installs a published, SHA-256-verified release and a verified FFmpeg 9
# shared-library runtime. It never compiles Remedia and never downloads anything at
# container startup. Build it from the repository root (see docker/compose.yaml):
#
#   docker build -f docker/Dockerfile --build-arg REMEDIA_VERSION=0.1.0-dev.6 -t remedia-server .
#
# Targets linux/amd64 and linux/arm64 (glibc 2.39+). Alpine/musl is intentionally unsupported
# because the release binaries and the conda-forge FFmpeg build target GNU/Linux.

# The release binaries are built on Ubuntu 24.04 and need glibc 2.39 or newer, so bookworm
# (glibc 2.36) is too old. Debian 13 "trixie" provides glibc 2.41.
ARG DEBIAN_IMAGE=debian:trixie-slim

# ---------------------------------------------------------------------------
# Stage 1: run the repository's own installer into /opt/remedia.
#
# scripts/install.sh already resolves the release asset for this machine, verifies the
# archive against GitHub's published SHA-256 digest, checks the archive's contents, and
# provisions FFmpeg 9.0.2 through micromamba/conda-forge while verifying all five
# required shared-library ABI majors. Reusing it keeps one implementation of that logic.
# This stage runs on the *target* platform (natively, or emulated by buildx), so the
# installer's uname-based asset selection always matches the platform being built.
# ---------------------------------------------------------------------------
FROM ${DEBIAN_IMAGE} AS installer

ARG TARGETARCH
# An explicit release, without the leading "v". "latest" is rejected on purpose: an image
# must be reproducible from its build arguments.
ARG REMEDIA_VERSION=0.1.0-dev.6

# bzip2 is needed by the installer to unpack micromamba; curl for GitHub and conda-forge.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends bzip2 ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY scripts/install.sh /usr/local/share/remedia-install.sh

# An optional BuildKit secret named github_token raises GitHub's anonymous API rate limit
# (60 requests per hour per IP) for the installer's release lookup. It is made available
# to curl only for api.github.com through ~/.netrc, so it is never sent to conda-forge or
# micromamba hosts, and both files are deleted before the layer is committed.
RUN --mount=type=secret,id=github_token \
    set -eu; \
    case "$TARGETARCH" in \
      amd64) expected_machine=x86_64 ;; \
      arm64) expected_machine=aarch64 ;; \
      *) echo "Unsupported TARGETARCH '$TARGETARCH': Remedia releases cover linux/amd64 and linux/arm64" >&2; exit 1 ;; \
    esac; \
    [ "$(uname -m)" = "$expected_machine" ] \
      || { echo "Builder machine $(uname -m) does not match TARGETARCH $TARGETARCH" >&2; exit 1; }; \
    case "$REMEDIA_VERSION" in \
      ''|latest) echo "Set REMEDIA_VERSION to an explicit release such as 0.1.0-dev.6" >&2; exit 1 ;; \
    esac; \
    if [ -s /run/secrets/github_token ]; then \
      ( umask 077; \
        printf 'machine api.github.com login x-access-token password %s\n' "$(tr -d '\r\n' < /run/secrets/github_token)" > "$HOME/.netrc"; \
        printf 'netrc\n' > "$HOME/.curlrc" ); \
    fi; \
    REMEDIA_INSTALL_DIR=/opt/remedia REMEDIA_VERSION="$REMEDIA_VERSION" sh /usr/local/share/remedia-install.sh; \
    rm -f "$HOME/.netrc" "$HOME/.curlrc"; \
    # Keep only what the server loads. The conda FFmpeg environment also contains the
    # ffmpeg/ffprobe/ffplay executables, headers and build metadata, none of which Remedia
    # uses (it links the shared libraries and never starts an FFmpeg subprocess).
    cd /opt/remedia/ffmpeg9; \
    rm -f bin/ffmpeg bin/ffprobe bin/ffplay; \
    rm -rf include lib/pkgconfig lib/cmake share/man share/doc share/info share/gtk-doc; \
    find lib -name '*.a' -delete

# ---------------------------------------------------------------------------
# Stage 2: the runtime image.
# ---------------------------------------------------------------------------
FROM ${DEBIAN_IMAGE} AS runtime

ARG REMEDIA_VERSION=0.1.0-dev.6

LABEL org.opencontainers.image.title="Remedia server" \
      org.opencontainers.image.description="Standalone Remedia audio server (WebSocket RPC) with FFmpeg 9 shared libraries" \
      org.opencontainers.image.source="https://github.com/twlite/remedia" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${REMEDIA_VERSION}"

# CA certificates are required for HTTPS media and plugin hosts (the server uses rustls
# with the bundled webpki roots for its own connections, FFmpeg uses the system store).
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 remedia \
    && useradd --system --uid 10001 --gid remedia --home-dir /home/remedia --create-home \
        --shell /usr/sbin/nologin remedia \
    && install -d -o remedia -g remedia -m 0750 /config \
    && install -d -o root -g root -m 0755 /media

# Binaries, FFmpeg and licences are owned by root and read-only to the service account.
COPY --from=installer /opt/remedia/libexec /opt/remedia/libexec
COPY --from=installer /opt/remedia/ffmpeg9 /opt/remedia/ffmpeg9
COPY --from=installer /opt/remedia/licenses /opt/remedia/licenses

# `remedia` is the documented command; `remedia-server` is the same lightweight CLI. The
# CLI finds remedia-server-runtime next to its real (symlink-resolved) path.
RUN ln -s /opt/remedia/libexec/remedia-server /usr/local/bin/remedia \
    && ln -s /opt/remedia/libexec/remedia-server /usr/local/bin/remedia-server \
    && ln -s /opt/remedia/licenses /usr/share/doc/remedia-licenses

# Verify the finished image once at build time: every shared library the runtime needs
# must resolve (including FFmpeg's transitive dependencies), and the loaded FFmpeg
# versions must match the required ABI majors. `doctor` needs a valid configuration, so a
# throwaway one is generated; the placeholder value below is not a credential.
ENV LD_LIBRARY_PATH=/opt/remedia/ffmpeg9/lib
RUN set -eu; \
    remedia --version; \
    missing="$(ldd /opt/remedia/libexec/remedia-server-runtime | grep 'not found' || true)"; \
    [ -z "$missing" ] || { echo "Unresolved libraries:" >&2; echo "$missing" >&2; exit 1; }; \
    for library in /opt/remedia/ffmpeg9/lib/libav*.so.* /opt/remedia/ffmpeg9/lib/libswresample.so.*; do \
      missing="$(ldd "$library" | grep 'not found' || true)"; \
      [ -z "$missing" ] || { echo "Unresolved libraries for $library:" >&2; echo "$missing" >&2; exit 1; }; \
    done; \
    cd "$(mktemp -d)"; \
    remedia --generate-config-file ./check.json >/dev/null; \
    REMEDIA_RPC_PASSWORD=build-time-placeholder remedia doctor --config ./check.json

ENV HOME=/home/remedia

USER 10001:10001
WORKDIR /config
EXPOSE 8787

# Local status check: `remedia --list --json` asks the running server, over its private
# loopback control socket, for live statistics. The server registers that endpoint only
# after its WebSocket listener is bound, and the CLI discards records which do not answer
# the authenticated query. An empty list therefore means "not running and ready". This
# says nothing about Discord, plugin hosts or reachability from outside the container.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD remedia --list --json | grep -q '"pid"' || exit 1

# SIGTERM (docker stop) and SIGINT are handled by the runtime. The exec-form entrypoint
# and the CLI's exec() into the runtime keep the real server as PID 1.
STOPSIGNAL SIGTERM
ENTRYPOINT ["remedia"]
CMD ["serve", "--config", "/config/remedia.config.json"]
