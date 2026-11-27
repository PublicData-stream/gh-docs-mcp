FROM docker.io/library/rust:1.100.0-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS rust-base
ENV CARGO_TARGET_DIR=/tmp/rust/target

FROM scratch AS cargo-registry-seed

FROM rust-base AS git-build
WORKDIR /git
RUN apk add --no-cache make=4.4.1-r4 zlib-dev=1.4-r0 zlib-static=1.4-r0 xz=5.8.4-r0
RUN wget -q -O /tmp/git.tar.xz https://www.kernel.org/pub/software/scm/git/git-2.54.1.tar.xz \
    && echo 'ca988eb9af7d8770339df7cdc38106fec8244664ff886840767cb9be3ef0e16c  /tmp/git.tar.xz' | sha256sum -c - \
    && tar -xJf /tmp/git.tar.xz --strip-components=1 \
    && rm /tmp/git.tar.xz
RUN --network=none make -j4 git NO_CURL=YesPlease NO_OPENSSL=YesPlease NO_EXPAT=YesPlease \
      NO_GETTEXT=YesPlease NO_ICONV=YesPlease NO_TCLTK=YesPlease NO_PERL=YesPlease \
      NO_PYTHON=YesPlease NO_PCRE2=YesPlease NO_REGEX=YesPlease LDFLAGS=-static \
    && mkdir /out && cp git COPYING /out/ \
    && readelf -l /out/git > /tmp/git-elf && ! grep -q INTERP /tmp/git-elf \
    && /out/git --version

FROM rust-base AS ripgrep-build
WORKDIR /ripgrep
RUN apk add --no-cache make=4.4.1-r4
RUN wget -q -O /tmp/rg.tar.gz https://codeload.github.com/BurntSushi/ripgrep/tar.gz/669cb4c755db20b26c3af3d923c042ae224c7496 \
    && echo '21002cba547651acf2ffb2f1e90010ee9533df3ce8819b171378d55a4f5953fd  /tmp/rg.tar.gz' | sha256sum -c - \
    && tar -xzf /tmp/rg.tar.gz --strip-components=1 \
    && rm /tmp/rg.tar.gz
RUN --mount=type=bind,from=cargo-registry-seed,target=/registry-seed \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    if [ -d /registry-seed/index ]; then \
        cp -r /registry-seed/index /registry-seed/cache /usr/local/cargo/registry/; \
    fi \
    && (cargo fetch --locked --offline || cargo fetch --locked)
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/tmp/rust/target,sharing=locked \
    target="$(rustc -vV | sed -n 's/^host: //p')" \
    && RUSTFLAGS='-C target-feature=+crt-static' cargo build --frozen --release --no-default-features \
         --bin rg --target "$target" --target-dir /tmp/rust/target/rg \
    && mkdir /out && cp "/tmp/rust/target/rg/$target/release/rg" /out/ \
    && cp LICENSE-MIT UNLICENSE /out/ \
    && readelf -l /out/rg > /tmp/rg-elf && ! grep -q INTERP /tmp/rg-elf \
    && /out/rg --version

FROM rust-base AS app-build
WORKDIR /src
COPY Cargo.lock Cargo.toml rust-toolchain.toml ./
COPY apps apps
COPY crates crates
COPY migrations migrations
# Cached artifacts may be newer than the copied workspace sources.
RUN --mount=type=bind,from=cargo-registry-seed,target=/registry-seed \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    if [ -d /registry-seed/index ]; then \
        cp -r /registry-seed/index /registry-seed/cache /usr/local/cargo/registry/; \
    fi \
    && (cargo fetch --locked --offline || cargo fetch --locked)
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/tmp/rust/target,sharing=locked \
    find apps crates migrations -type f -exec touch {} + \
    && cargo build --workspace --bins --release --frozen \
    && mkdir /out \
    && cp /tmp/rust/target/release/docs-mcp-serve /tmp/rust/target/release/docs-mcp-sync \
          /tmp/rust/target/release/docs-mcp-feed /tmp/rust/target/release/docs-mcp-git-reader \
          /tmp/rust/target/release/docs-mcp-git-sync \
          /tmp/rust/target/release/docs-mcp-search-worker /out/

FROM docker.io/library/alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf AS runtime
COPY LICENSE docs/RFC-MCP-DOCKER-LICENSE /usr/share/licenses/docs-mcp/
RUN --network=none grep -A 1 -x 'P:ca-certificates-bundle' /lib/apk/db/installed \
    | grep -qx 'V:20261020-r0' \
    && test -s /etc/ssl/certs/ca-certificates.crt

FROM runtime AS serve
COPY --from=app-build /out/docs-mcp-serve /out/docs-mcp-git-reader /out/docs-mcp-search-worker /usr/local/bin/
COPY --from=git-build /out/git /usr/local/bin/
COPY --from=git-build /out/COPYING /usr/share/licenses/git/
COPY --from=ripgrep-build /out/rg /usr/local/bin/
COPY --from=ripgrep-build /out/LICENSE-MIT /out/UNLICENSE /usr/share/licenses/ripgrep/
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/docs-mcp-serve"]

FROM runtime AS sync
COPY --from=app-build /out/docs-mcp-sync /out/docs-mcp-git-sync /out/docs-mcp-git-reader /usr/local/bin/
COPY --from=app-build /out/docs-mcp-git-sync /usr/local/libexec/docs-mcp-git/git-remote-docsmcp
COPY --from=git-build /out/git /usr/local/bin/
COPY --from=git-build /out/COPYING /usr/share/licenses/git/
USER 10002:10002
ENTRYPOINT ["/usr/local/bin/docs-mcp-sync"]

FROM runtime AS feed
COPY --from=app-build /out/docs-mcp-feed /usr/local/bin/
USER 10003:10003
ENTRYPOINT ["/usr/local/bin/docs-mcp-feed"]

FROM docker.io/library/postgres:19.0-alpine3.24@sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f AS postgres
ARG TARGETARCH
# APK's transaction log embeds wall-clock time in the image layer.
RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) arch=x86_64; digest=8b37229c4761cf0eef5089045283526ef43bf335073050515010d34a313186ed ;; \
        arm64) arch=aarch64; digest=3e12146a2f15e27a22d70d21f8f52be29966105ce304c55bab2bfe75604981a9 ;; \
        *) exit 1 ;; \
    esac; \
    package=/tmp/libxml2-2.13.9-r3.apk; \
    wget -q -O "$package" "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/$arch/${package##*/}"; \
    printf '%s  %s\n' "$digest" "$package" | sha256sum -c -; \
    apk --no-logfile add --no-network --no-cache "$package"; \
    rm "$package" /usr/local/bin/gosu
COPY --chmod=755 deploy/postgres-entrypoint.sh /usr/local/bin/docs-mcp-postgres-entrypoint
USER 70:70
ENTRYPOINT ["/usr/local/bin/docs-mcp-postgres-entrypoint"]
CMD ["postgres"]

FROM app-build AS phase1-fixture-build
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/tmp/rust/target,sharing=locked \
    cargo build --frozen --release -p docs-mcp --features phase1-fixtures --bin docs-mcp-phase1-fixture \
    && cp /tmp/rust/target/release/docs-mcp-phase1-fixture /out/

FROM serve AS phase1-serve
COPY --from=phase1-fixture-build /out/docs-mcp-phase1-fixture /usr/local/bin/

FROM sync AS phase1-sync
COPY --from=phase1-fixture-build /out/docs-mcp-phase1-fixture /out/docs-mcp-git-reader /usr/local/bin/

FROM app-build AS phase2-fixture-build
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/tmp/rust/target,sharing=locked \
    cargo build --frozen --release -p docs-mcp --features phase2-fixtures --bin docs-mcp-phase2-fixture \
    && cp /tmp/rust/target/release/docs-mcp-phase2-fixture /out/

FROM sync AS phase2-sync
COPY --from=phase2-fixture-build /out/docs-mcp-phase2-fixture /usr/local/bin/

FROM runtime AS phase2-upstream
RUN apk add --no-cache python3=3.14.10-r2 git=2.54.1-r0 git-daemon=2.54.1-r0
COPY scripts/phase2/upstream.py /opt/upstream.py
USER 10002:10002
ENTRYPOINT ["python3", "-B", "/opt/upstream.py"]

FROM app-build AS phase3-fixture-build
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/tmp/rust/target,sharing=locked \
    cargo build --frozen --release -p docs-mcp --features phase2-fixtures --bin docs-mcp-phase3-fixture \
    && cp /tmp/rust/target/release/docs-mcp-phase3-fixture /out/

FROM serve AS phase3-serve
COPY --from=phase3-fixture-build /out/docs-mcp-phase3-fixture /usr/local/bin/

FROM phase2-sync AS phase3-sync
COPY --from=phase3-fixture-build /out/docs-mcp-phase3-fixture /usr/local/bin/

FROM app-build AS phase4-fixture-build
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/tmp/rust/target,sharing=locked \
    cargo build --frozen --release -p docs-mcp --features phase4-fixtures --bin docs-mcp-phase4-fixture \
    && cp /tmp/rust/target/release/docs-mcp-phase4-fixture /out/

FROM phase3-serve AS phase4-serve
COPY --from=phase4-fixture-build /out/docs-mcp-phase4-fixture /usr/local/bin/

FROM app-build AS phase6-fixture-build
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/tmp/rust/target,sharing=locked \
    cargo build --frozen --release -p docs-mcp --features phase6-fixtures --bin docs-mcp-sync --bin docs-mcp-feed --bin docs-mcp-serve \
    && cp /tmp/rust/target/release/docs-mcp-sync /tmp/rust/target/release/docs-mcp-feed \
          /tmp/rust/target/release/docs-mcp-serve /out/

FROM phase2-sync AS phase6-sync
COPY --from=phase6-fixture-build /out/docs-mcp-sync /usr/local/bin/

FROM feed AS phase6-feed
COPY --from=phase6-fixture-build /out/docs-mcp-feed /usr/local/bin/

FROM serve AS phase6-serve
COPY --from=phase6-fixture-build /out/docs-mcp-serve /usr/local/bin/
