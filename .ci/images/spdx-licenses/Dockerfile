# Builder stage: download the SPDX license text archive and pull the text license files out of it.
FROM registry.ddbuild.io/images/mirror/ubuntu:24.04 AS builder

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Etc/UTC

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl

RUN curl -s -L -o /tmp/spdx.tar.gz https://github.com/spdx/license-list-data/archive/refs/tags/v3.28.0.tar.gz && \
    tar -C /tmp -xzf /tmp/spdx.tar.gz && \
    mv /tmp/license-list-data-3.28.0 /tmp/spdx-licenses

FROM registry.ddbuild.io/images/mirror/ubuntu:24.04 AS final
COPY --from=builder /tmp/spdx-licenses/text /tmp/spdx-licenses/text
