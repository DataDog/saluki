FROM registry.ddbuild.io/docker:24.0.4-jammy

# Install some basic dependencies, including some additional APT repositories.
RUN mkdir -p /etc/apt/keyrings/ \
    && curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key | gpg --dearmor -o /etc/apt/keyrings/nodesource-repo.gpg.key \
    && echo "deb [signed-by=/etc/apt/keyrings/nodesource-repo.gpg.key] https://deb.nodesource.com/node_20.x nodistro main" > /etc/apt/sources.list.d/nodesource.list \
    && curl -L https://s3.amazonaws.com/dd-package-public/dd-package-$(dpkg --print-architecture).deb -o dd-package.deb \
    && dpkg -i dd-package.deb \
    && rm dd-package.deb \
    && apt-get update \
    && apt-get -y install --no-install-recommends ca-certificates git lsb-release binutils nodejs=20.11.1-1nodesource1 \
    && dd-package --bucket binaries-ddbuild-io-prod --package devtools/dd-package-dev --distribution "20.04" \
    && apt-get -y clean \
    && rm -rf /var/lib/apt/lists/*

# Install datadog-ci.
RUN npm install -g @datadog/datadog-ci@3.0.1 && \
    datadog-ci version

# Install aws-cli.
RUN curl -o awscliv2.zip https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip \
    && unzip awscliv2.zip \
    && rm awscliv2.zip \
    && ./aws/install --bin-dir /usr/local/bin --install-dir /usr/local/aws-cli \
    aws --version

# Install pr-commenter.
RUN dd-package --bucket binaries-ddbuild-io-prod --package devtools/pr-commenter

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
