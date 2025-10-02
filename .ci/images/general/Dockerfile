ARG DD_AGENT_VERSION=7.70.2-jmx
ARG DD_AGENT_IMAGE=datadog/agent:${DD_AGENT_VERSION}

FROM ${DD_AGENT_IMAGE} AS agent

FROM registry.ddbuild.io/docker:20.10-py3

# Install datadog-ci.
RUN set -x \
    && mkdir -p /etc/apt/keyrings/ \
    && curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key | gpg --dearmor -o /etc/apt/keyrings/nodesource-repo.gpg.key \
    && echo "deb [signed-by=/etc/apt/keyrings/nodesource-repo.gpg.key] https://deb.nodesource.com/node_20.x nodistro main" > /etc/apt/sources.list.d/nodesource.list \
    && apt-get update \
    && apt-get -y install --no-install-recommends binutils nodejs=20.11.1-1nodesource1 \
    && apt-get -y clean \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g @datadog/datadog-ci@3.0.1 \
    && datadog-ci version

# Import python bits from Agent image.
# Do last in the pipeline to avoid polluting the builds of above protobuf/go etc from using these libs
COPY --from=agent /opt/datadog-agent/embedded/ /opt/datadog-agent/embedded/
ENV LD_LIBRARY_PATH="/opt/datadog-agent/embedded/lib:${LD_LIBRARY_PATH}"
ENV PATH="/opt/datadog-agent/embedded/bin:${PATH}"

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
