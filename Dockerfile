ARG LLVMTARGETARCH
FROM ghcr.io/cross-rs/${LLVMTARGETARCH}-unknown-linux-musl:latest AS builder

WORKDIR /build

RUN cross build --target ${LLVMTARGETARCH}-unknown-linux-musl

FROM scratch

COPY --from=builder /build/target/${LLVMTARGETARCH}-unknown-linux-musl/release/tinylevel /usr/bin/tinylevel

WORKDIR /

ENTRYPOINT "/usr/bin/tinylevel"
