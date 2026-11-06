# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/strut-stack-sql /usr/local/bin/strut-stack-sql
# Smithery container runtime: Smithery builds this image, reads the HTTP port
# from EXPOSE, and proxies MCP requests to http://localhost:8000/mcp (the
# server's streamable-HTTP route; see smithery.yaml). The binary starts HTTP
# mode only via the --http CLI flag (no env var), so the default CMD starts
# HTTP mode. Passing any args to `docker run` replaces this CMD (ENTRYPOINT is
# unchanged), e.g. stdio mode:
#   docker run --rm -i -e DATABASE_URL=... ghcr.io/rzlco666/strut-stack-sql:latest --url-env DATABASE_URL
# SECURITY: --allow-unauthenticated-http is required because Smithery's proxy
# cannot present a bearer token and the bind is non-loopback (run_http refuses
# non-loopback binds without auth otherwise). The port is only reachable via
# Smithery's front-end in hosted mode; do NOT publish port 8000 publicly
# (`docker run -p 8000:8000`) unless you accept unauthenticated MCP access to
# DATABASE_URL. --allow-ddl is deliberately OFF here (DDL stays opt-in via
# self-hosting); --full-tools is on (read-only introspection), --max-rows 100,
# --deny-columns password,salt.
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/strut-stack-sql"]
CMD ["--http", "0.0.0.0:8000", "--url-env", "DATABASE_URL", "--full-tools", "--max-rows", "100", "--deny-columns", "password,salt", "--allow-unauthenticated-http"]
