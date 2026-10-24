# Build context: images/
FROM gitlab/gitlab-runner:v18.11.4 AS runner

FROM ghcr.io/osscontainertools/kaniko:v1.28.6-alpine

LABEL org.opencontainers.image.source="https://gitlab.com/<namespace>/ciburst" \
      org.opencontainers.image.title="ciburst kaniko pool image" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apk add --no-cache bash ca-certificates curl git git-lfs

COPY --from=runner /usr/bin/gitlab-runner /usr/local/bin/gitlab-runner
COPY --chmod=0755 base/entrypoint.sh /usr/local/bin/ciburst-entrypoint

ENV CIBURST_BUILDS_DIR=/kaniko/builds
RUN mkdir -p /kaniko/builds /cache \
 && echo "export PATH=\"$PATH\"" > /etc/profile.d/ciburst-path.sh

WORKDIR /root
ENTRYPOINT ["/usr/local/bin/ciburst-entrypoint"]
