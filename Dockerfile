ARG LLVMTARGETARCH
FROM ghcr.io/cross-rs/${LLVMTARGETARCH}-unknown-linux-musl:latest AS builder

ENV LLVMTARGETARCH=$LLVMTARGETARCH

WORKDIR /build

RUN cargo build --target ${LLVMTARGETARCH}-unknown-linux-musl

FROM scratch

ENV LLVMTARGETARCH=$LLVMTARGETARCH

COPY --from=builder /build/target/${LLVMTARGETARCH}-unknown-linux-musl/release/tinylevel /usr/bin/tinylevel

WORKDIR /

ENTRYPOINT "/usr/bin/tinylevel"
