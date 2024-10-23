FROM registry.ddbuild.io/images/mirror/ubuntu:20.04

RUN apt-get update && \
    apt-get install -y --no-install-recommends curl ca-certificates awscli && \
    apt-get clean

RUN curl -L https://s3.amazonaws.com/dd-package-public/dd-package-$(dpkg --print-architecture).deb -o dd-package.deb && \
    dpkg -i dd-package.deb && \
    rm dd-package.deb && \
    apt-get update && \
    dd-package --bucket binaries.ddbuild.io --package devtools/dd-package-dev --distribution "20.04"

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
