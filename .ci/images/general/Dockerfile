FROM registry.ddbuild.io/docker:24.0.4-jammy

# Install some necessary tools.
RUN set -x \
    && mkdir -p /etc/apt/keyrings/ \
    && curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key | gpg --dearmor -o /etc/apt/keyrings/nodesource-repo.gpg.key \
    && echo "deb [signed-by=/etc/apt/keyrings/nodesource-repo.gpg.key] https://deb.nodesource.com/node_20.x nodistro main" > /etc/apt/sources.list.d/nodesource.list \
    && apt-get update \
    && apt-get -y install --no-install-recommends binutils nodejs=20.11.1-1nodesource1 python3 python3-venv unzip \
    && apt-get -y clean \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g @datadog/datadog-ci@3.0.1 \
    && curl "https://s3.amazonaws.com/aws-cli/awscli-bundle.zip" -o "awscli-bundle.zip" \
    && unzip awscli-bundle.zip \
    && python3 ./awscli-bundle/install -i /usr/local/aws -b /usr/local/bin/aws

RUN datadog-ci version
RUN aws --version

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
