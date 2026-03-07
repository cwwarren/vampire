FROM rust:1-bookworm AS builder
WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin vampire

FROM debian:bookworm-slim
RUN useradd --system --create-home --home-dir /var/lib/vampire --shell /usr/sbin/nologin vampire \
    && mkdir -p /var/lib/vampire \
    && chown -R vampire:vampire /var/lib/vampire
COPY --from=builder /work/target/release/vampire /usr/local/bin/vampire
RUN chown vampire:vampire /usr/local/bin/vampire
USER vampire
ENV VAMPIRE_BIND=0.0.0.0:8080
ENV VAMPIRE_CACHE_DIR=/var/lib/vampire
WORKDIR /var/lib/vampire
EXPOSE 8080
VOLUME ["/var/lib/vampire"]
ENTRYPOINT ["/usr/local/bin/vampire"]

