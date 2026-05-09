#!/bin/sh

# TMPDIR=/root/tmp podman load -i /root/anazoa.tar.gz

podman run \
  --detach \
  --network=host \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -v /root/anazoa:/app:ro \
  anazoa:latest
