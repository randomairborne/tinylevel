FROM alpine
ARG TARGETARCH

COPY /${TARGETARCH}-executables/tinylevel /usr/bin/

ENTRYPOINT "/usr/bin/tinylevel"
