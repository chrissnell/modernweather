# The binary is cross-compiled before this file is used, so nothing in the build runs
# target code and no QEMU is involved — buildx assembles each architecture's image from a
# statically linked artefact. That is the whole reason the Linux targets are musl-static:
# the Go pipeline needed QEMU under buildx, which is slow and a source of flakes.
#
# Build context: a directory holding one subdirectory per platform label, as produced by
# `cargo xtask dist` and arranged by .github/workflows/release.yml.

FROM alpine:3.22 AS certs
RUN apk add --no-cache ca-certificates tzdata

FROM scratch

ARG TARGETPLATFORM
ARG VERSION=0.0.0
ARG REVISION=unknown

LABEL org.opencontainers.image.title="modernweather" \
      org.opencontainers.image.description="Weather station data collection and distribution" \
      org.opencontainers.image.source="https://github.com/chrissnell/modernweather" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

# Outbound TLS to Aeris, PWS Weather, Weather Underground and APRS-IS needs roots, and
# chrono needs the zoneinfo database for local-time rendering.
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=certs /usr/share/zoneinfo /usr/share/zoneinfo

# A scratch image has no /etc/passwd, so the numeric id is the only thing that means
# anything here. 65532 matches the Go image's choice so that an existing bind-mounted
# data directory keeps working.
COPY rootfs/etc/passwd /etc/passwd
COPY rootfs/etc/group /etc/group

COPY --chown=65532:65532 ${TARGETPLATFORM}/modernweather /usr/local/bin/modernweather

USER 65532:65532
VOLUME ["/var/lib/modernweather"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/modernweather"]
CMD ["--config", "/var/lib/modernweather/config.db"]
