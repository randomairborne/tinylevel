FROM rust:alpine AS builder

RUN apk add musl-dev

WORKDIR /build

COPY . .

RUN cargo build --release

FROM scratch

COPY --from=builder /build/target/release/tinylevel /usr/bin/tinylevel

ENTRYPOINT ["/usr/bin/tinylevel"]
