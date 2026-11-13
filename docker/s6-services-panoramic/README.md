# s6 services for panoramic

Panoramic's `agent-adp` participant type places these over the image's own services (`../s6-services`) when it
starts a container. They do what the image's services do, and also write the exit records and log files the harness
reads. No Dockerfile copies them, so every image built from `docker/` is unchanged.
