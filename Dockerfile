FROM rust:1.88.0-slim-bookworm AS builder

RUN update-ca-certificates
ENV USER=worker
ENV UID=1001

RUN adduser \
    --disabled-password \
    --gecos "" \
    --home "/nonexistent" \
    --shell "/sbin/nologin" \
    --no-create-home \
    --uid "${UID}" \
    "${USER}" \
    && groupadd i2c \
    && usermod -aG i2c "${USER}"

RUN apt-get update \
    && apt-get -y upgrade \
    && apt-get -y --no-install-recommends install pkg-config make g++ libssl-dev \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Create an empty src/main.rs, so compilation of dependencies works without errors
RUN cargo init --vcs none
COPY ./Cargo.toml ./Cargo.lock /app/
# This step compiles only our dependencies and saves them in a layer. This is the most impactful time savings
# Note the use of --mount=type=cache. On subsequent runs, we'll have the crates already downloaded
RUN --mount=type=cache,target=/usr/local/cargo/registry cargo build --release

COPY ./src /app/src/
# A bit of magic here!
# * We're mounting that cache again to use during the build, otherwise it's not present and we'll have to download those again - bad!
# * Rust here is a bit fiddly, so we'll touch the files (even though we copied over them) to force a new build
RUN --mount=type=cache,target=/usr/local/cargo/registry touch /app/src/main.rs && cargo build --release

FROM gcr.io/distroless/cc-debian12

COPY --from=builder /etc/passwd /etc/passwd
COPY --from=builder /etc/group /etc/group

WORKDIR /app

COPY --from=builder /app/target/release/sgp30-exporter ./
USER worker:worker

EXPOSE $PORT

CMD ["/app/sgp30-exporter"]
