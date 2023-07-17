FROM alpine

COPY /executables/tinylevel /usr/bin/

ENTRYPOINT "/usr/bin/tinylevel"
