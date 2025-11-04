FROM registry.ddbuild.io/docker:24.0.4-jammy

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

# Install the AWS CLI.
COPY .ci/install-awscli.sh /
RUN chmod +x /install-awscli.sh && /install-awscli.sh

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
