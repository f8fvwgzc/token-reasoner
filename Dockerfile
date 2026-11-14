# ---- build -----------------------------------------------------------------
FROM rust:1-bookworm AS build
# Pin the LadybugDB library the `lbug` crate downloads; without a version it
# asks for the latest release, which may not match the crate.
ENV LBUG_VERSION=0.22.3
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p lge-cli && cp target/release/lge /usr/local/bin/lge
# Install the full-text extension into the image, so a container needs no
# network for it. A failure here is not fatal: search falls back to substring.
RUN lge --db /tmp/setup/graph.lbug setup || echo "full-text extension not installed at build time"
RUN mkdir -p /root/.lbdb

# ---- run -------------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/lge /usr/local/bin/lge
COPY --from=build /root/.lbdb /root/.lbdb
ENV LGE_HOME=/data \
    LGE_HOST=0.0.0.0 \
    LGE_PORT=8787
VOLUME /data
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s CMD ["lge", "--version"]
ENTRYPOINT ["lge", "serve"]
