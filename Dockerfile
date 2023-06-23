FROM alpine

COPY /executables/tinylevel /usr/bin/

EXPOSE 8080

ENTRYPOINT "/usr/bin/tinylevel"
