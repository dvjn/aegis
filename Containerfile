# scratch has no shell, so the binary and data directory are installed in a
# stage that has one. The stage is pinned to the build platform, because its
# own architecture never reaches the runtime image.
FROM --platform=$BUILDPLATFORM docker.io/library/busybox:1.38.0@sha256:fd7dc98638c8e305f4dc34e979f1c0fdfdcaeb0fbf8fcff77ae834b6da3d7e6e AS layout
ARG TARGETARCH
COPY dist/${TARGETARCH}/aegis /tmp/aegis
RUN install -Dm0555 /tmp/aegis /out/usr/local/bin/aegis \
    && install -d -o 65532 -g 65532 -m 0700 /out/data

FROM scratch

COPY --from=layout /out/ /

USER 65532:65532
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8765

ENV HTTP_ADDR=0.0.0.0:8765 \
    DATABASE_URL="sqlite:///data/aegis.db?mode=rwc"

ENTRYPOINT ["/usr/local/bin/aegis"]
