FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin vampire

FROM alpine:3
RUN adduser -D -h /nonexistent -H -s /sbin/nologin vampire \
    && mkdir -p /var/cache/vampire \
    && chown vampire:vampire /var/cache/vampire
COPY --from=builder /work/target/release/vampire /usr/local/bin/vampire
USER vampire
ENV VAMPIRE_PKG_BIND=0.0.0.0:8080
ENV VAMPIRE_GIT_BIND=0.0.0.0:8081
ENV VAMPIRE_MANAGEMENT_BIND=0.0.0.0:8082
ENV VAMPIRE_CACHE_DIR=/var/cache/vampire
# Set VAMPIRE_PUBLIC_BASE_URL at runtime to the externally reachable package-listener origin.
WORKDIR /var/cache/vampire
EXPOSE 8080
EXPOSE 8081
EXPOSE 8082
VOLUME ["/var/cache/vampire"]
ENTRYPOINT ["/usr/local/bin/vampire"]
