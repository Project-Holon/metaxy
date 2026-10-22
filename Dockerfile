FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:stable-slim
RUN useradd --system --no-create-home metaxy
COPY --from=build /src/target/release/metaxy /usr/local/bin/metaxy
USER metaxy
EXPOSE 4141 443
ENTRYPOINT ["/usr/local/bin/metaxy"]
