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
    && curl "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o "awscliv2.zip" \
    && unzip awscliv2.zip \
    && ./aws/install --bin-dir /usr/local/bin --install-dir /usr/local/aws-cli --update

RUN datadog-ci version
RUN aws --version

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
