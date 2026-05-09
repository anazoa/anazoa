FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libstdc++6 libgcc-s1 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --chmod=0755 anazoa-tun /usr/local/bin/anazoa-tun

ENTRYPOINT ["/usr/local/bin/anazoa-tun"]
