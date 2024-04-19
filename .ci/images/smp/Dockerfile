FROM registry.ddbuild.io/images/mirror/ubuntu:22.04

# Install awscli by hand. The version in-image is too old.
COPY .ci/install-awscli.sh /
RUN chmod +x /install-awscli.sh && /install-awscli.sh
RUN /install-awscli.sh

COPY .ci/images/smp/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
