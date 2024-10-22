ARG DD_AGENT_VERSION=7.58.0-jmx
ARG DD_AGENT_IMAGE=datadog/agent:${DD_AGENT_VERSION}

FROM ${DD_AGENT_IMAGE} as agent
FROM registry.ddbuild.io/images/mirror/ubuntu:20.04

RUN apt-get update && \
    apt-get install -y --no-install-recommends curl ca-certificates awscli && \
    rm -rf /var/lib/apt/lists/*

RUN curl -L https://s3.amazonaws.com/dd-package-public/dd-package-$(dpkg --print-architecture).deb -o dd-package.deb && \
    dpkg -i dd-package.deb && \
    rm dd-package.deb && \
    apt-get update && \
    dd-package --bucket binaries.ddbuild.io --package devtools/dd-package-dev --distribution "20.04"

# Import python bits from Agent image.
COPY --from=agent /opt/datadog-agent/embedded/ /opt/datadog-agent/embedded/
ENV LD_LIBRARY_PATH="/opt/datadog-agent/embedded/lib:${LD_LIBRARY_PATH}"
ENV PATH="/opt/datadog-agent/embedded/bin:${PATH}"

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
