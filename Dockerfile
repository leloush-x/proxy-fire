# Build the single all-in-one binary, then copy just it + configs into a
# distroless static image. Works from a clean checkout:
#   docker build -t proxy-fire .
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev git
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin proxy-fire

FROM gcr.io/distroless/static:nonroot
WORKDIR /app
# The binary and configs/ share a directory: find_data_file looks in
# <exe dir>/configs for the bundled config.example.yaml and catalog.json.
COPY --from=build /src/target/release/proxy-fire /app/
COPY configs /app/configs

# Usage/reliability state and scanned catalogs go to $HOME/.proxy-fire
# (HOME=/home/nonroot, writable). Mount a volume there to keep them.
USER nonroot:nonroot
EXPOSE 8080
# The binary defaults to 127.0.0.1; inside a container that would be unreachable
# through -p, so the image listens on all interfaces. Publish the port only on
# localhost (-p 127.0.0.1:8080:8080) or set proxy.auth_token.
ENTRYPOINT ["/app/proxy-fire", "--host", "0.0.0.0"]
