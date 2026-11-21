# syntax=docker/dockerfile:1.7
# The tripletrail server: `docker run -p 7878:7878 -v tripletrail-data:/data ghcr.io/matdata-eu/tripletrail`
# then open http://localhost:7878/ for the web UI. Configure with TRIPLETRAIL_* environment
# variables (see `tripletrail serve --help`).
#
# `--build-arg EDITION=enterprise` builds TripleTrail Enterprise (#734), which needs the
# proprietary crates/tripletrail-enterprise and is only pushed to the private registry. The
# default, `open`, is the Apache-2.0 build and never contains that code.

# The build stage runs on the build machine's own platform and cross-compiles for the
# image's (linux/amd64 or linux/arm64), so an arm64 image needs no emulation.
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
ARG EDITION=open
WORKDIR /src
# No debug info in the image: it would triple the binary.
ENV CARGO_PROFILE_RELEASE_DEBUG=false CARGO_PROFILE_RELEASE_STRIP=symbols
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu > /target ;; \
      arm64) echo aarch64-unknown-linux-gnu > /target ;; \
      *) echo "unsupported architecture $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && if [ "$(uname -m)-$TARGETARCH" != x86_64-amd64 ] && [ "$(uname -m)-$TARGETARCH" != aarch64-arm64 ]; then \
      apt-get update \
      && apt-get install -y --no-install-recommends "gcc-$(cut -d- -f1 /target | sed s/_/-/)-linux-gnu" \
        "g++-$(cut -d- -f1 /target | sed s/_/-/)-linux-gnu" \
      && rm -rf /var/lib/apt/lists/*; \
    fi \
    && rustup target add "$(cat /target)"
COPY . .
# A crate cache per architecture: the two builds of a multi-platform image run at the same
# time, and cargo's lock is not in this folder, so on a shared one they unpack the same crate
# at once and one fails ("failed to unpack package ... File exists").
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=tripletrail-registry-$TARGETARCH \
    --mount=type=cache,target=/src/target,id=tripletrail-target-$TARGETARCH \
    target="$(cat /target)" \
    && cross="$(cut -d- -f1 /target | sed s/_/-/)-linux-gnu" \
    && if command -v "$cross-gcc" >/dev/null; then \
      upper="$(echo "$target" | tr a-z- A-Z_)"; lower="$(echo "$target" | tr - _)"; \
      export "CARGO_TARGET_${upper}_LINKER=$cross-gcc" "CC_${lower}=$cross-gcc" \
        "CXX_${lower}=$cross-g++" "AR_${lower}=$cross-ar"; \
    fi \
    && case "$EDITION" in \
      open) features="" ;; \
      enterprise) features="--features enterprise" ;; \
      *) echo "EDITION is open or enterprise, not $EDITION" >&2; exit 1 ;; \
    esac \
    && cargo build --release --locked -p tripletrail-server $features --target "$target" \
    && cp "target/$target/release/tripletrail" /tripletrail \
    && mkdir -p /data /licenses \
    && cp LICENSE NOTICE /licenses/ \
    && if [ "$EDITION" = enterprise ]; then cp crates/tripletrail-enterprise/LICENSE /licenses/LICENSE-ENTERPRISE; fi \
    && if [ -f THIRD_PARTY_LICENSES.md ]; then cp THIRD_PARTY_LICENSES.md /licenses/; fi

# Only the binary, for release archives: `docker buildx build --target binary --output dist .`
FROM scratch AS binary
COPY --from=build /tripletrail /tripletrail

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tripletrail /usr/local/bin/tripletrail
# LICENSE, NOTICE and, in published images, the third-party license list.
COPY --from=build /licenses /usr/share/doc/tripletrail
COPY --from=build --chown=65532:65532 /data /data
ENV TRIPLETRAIL_BIND=0.0.0.0:7878 \
    TRIPLETRAIL_LOCATION=/data \
    TRIPLETRAIL_LOG_REQUESTS=true \
    TRIPLETRAIL_HEALTH_URL=http://127.0.0.1:7878
VOLUME /data
EXPOSE 7878
USER 65532:65532
HEALTHCHECK --interval=15s --timeout=6s --start-period=30s CMD ["/usr/local/bin/tripletrail", "health"]
ENTRYPOINT ["/usr/local/bin/tripletrail"]
CMD ["serve"]
