# ---- build stage: full Rust toolchain (discarded) ----
FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /build

# Pre-build dependencies as a cacheable layer: compile a dummy main against the
# real manifests, then drop the dummy so only our code recompiles after `COPY src`.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release \
 && rm -f target/release/anthroxy target/release/deps/anthroxy-*

COPY src ./src
RUN cargo build --release

# ---- runtime stage: distroless (glibc; rustls bundles its own CA roots) ----
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/anthroxy /usr/local/bin/anthroxy
EXPOSE 8787
# Bind is NOT defaulted to a wildcard here on purpose — the deployer (Nomad job /
# `podman run`) sets ANTHROXY_BIND explicitly so the exposure is always a reviewed choice.
ENTRYPOINT ["/usr/local/bin/anthroxy"]
