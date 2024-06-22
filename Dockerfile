ARG LLVMTARGETARCH
FROM --platform=${BUILDPLATFORM} ghcr.io/randomairborne/cross-cargo-${LLVMTARGETARCH}:latest AS builder

ARG LLVMTARGETARCH

WORKDIR /build

RUN cargo build --release --target ${LLVMTARGETARCH}-unknown-linux-musl

FROM scratch
ARG LLVMTARGETARCH

COPY --from=builder /build/target/${LLVMTARGETARCH}-unknown-linux-musl/release/tinylevel /usr/bin/tinylevel

WORKDIR /

ENTRYPOINT "/usr/bin/tinylevel"
