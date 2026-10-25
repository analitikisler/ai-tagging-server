# Static musl binary on an empty image.
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add "$(uname -m)-unknown-linux-musl"
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --target "$(uname -m)-unknown-linux-musl" \
    && cp "target/$(uname -m)-unknown-linux-musl/release/tagging-server" /tagging-server
RUN echo 'app:x:10001:10001::/:/sbin/nologin' > /passwd

FROM scratch
COPY --from=build /tagging-server /tagging-server
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /passwd /etc/passwd
USER 10001
ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/tagging-server"]
