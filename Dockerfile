# Multi-stage build: compile with the stable toolchain, run on a slim image.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p notelet-mcp

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/notelet-mcp /usr/local/bin/notelet-mcp
ENV NOTELET_BIND=0.0.0.0:8490
EXPOSE 8490
CMD ["notelet-mcp", "serve"]
