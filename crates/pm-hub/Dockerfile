# pm-hub image (AGT-1387). Build context is the workspace root, since the
# hub links sibling crates:
#   docker build -f crates/pm-hub/Dockerfile .
# Railway picks this file up through the root railway.toml.

# Same toolchain as the repo's stamp required_checks.
# Base images are pinned by digest (AGT-1452): a moved tag can't change what
# ships. Bump a pin deliberately:
#   docker buildx imagetools inspect rust:1.94-slim-trixie
FROM rust:1.94-slim-trixie@sha256:cf09adf8c3ebaba10779e5c23ff7fe4df4cccdab8a91f199b0c142c53fef3e1a AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p pm-hub

# Debian 13 (trixie): Debian 12 leaves security support in 2026.
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a
COPY --from=build /src/target/release/pm-hub /usr/local/bin/pm-hub
USER nobody
# DATABASE_URL and PORT come from the service's environment.
CMD ["pm-hub"]
