# pm-hub image (AGT-1387). Build context is the workspace root, since the
# hub links sibling crates:
#   docker build -f crates/pm-hub/Dockerfile .
# Railway picks this file up through the root railway.toml.

# Same toolchain as the repo's stamp required_checks.
FROM rust:1.94-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p pm-hub

FROM debian:bookworm-slim
COPY --from=build /src/target/release/pm-hub /usr/local/bin/pm-hub
USER nobody
# DATABASE_URL and PORT come from the service's environment.
CMD ["pm-hub"]
