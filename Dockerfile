# Drop-in for CLIProxyAPI's image: same working directory, config path, auth
# directory and ports, so an existing docker-compose.yml keeps working.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY ui ui
RUN cargo build --release --locked
# CLIProxyAPI's image runs ./CLIProxyAPI from /CLIProxyAPI and keeps sign-ins in
# /root/.cli-proxy-api; keep both paths working.
RUN mkdir -p /out/CLIProxyAPI /out/root/.cli-proxy-api \
 && cp target/release/fusebox /out/fusebox \
 && ln -s /usr/local/bin/fusebox /out/CLIProxyAPI/CLIProxyAPI

FROM gcr.io/distroless/cc-debian12
LABEL org.opencontainers.image.title="Fusebox" \
      org.opencontainers.image.source="https://github.com/DigitalPals/Fusebox" \
      org.opencontainers.image.description="All your AI subscriptions as one fast API. Drop-in for CLIProxyAPI." \
      org.opencontainers.image.licenses="Unlicense"
COPY --from=build /out/fusebox /usr/local/bin/fusebox
COPY --from=build /out/CLIProxyAPI /CLIProxyAPI
COPY --from=build /out/root /root
ENV HOME=/root \
    FUSEBOX_DEFAULT_HOST=0.0.0.0
WORKDIR /CLIProxyAPI
# API + dashboard, then the OAuth callback ports (Claude, Codex, Antigravity).
EXPOSE 8317 54545 1455 51121
CMD ["/usr/local/bin/fusebox"]
