# Ubuntu 24.04; refresh this digest deliberately when updating the test image.
FROM ubuntu:24.04@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
COPY users.sh probe.py /checks/
RUN chmod 0555 /checks/users.sh /checks/probe.py
ENTRYPOINT ["/bin/bash", "/checks/users.sh"]
