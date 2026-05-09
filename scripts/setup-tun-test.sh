#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  setup-tun-test.sh IFACE IPV4_CIDR IPV4_PEER IPV6_CIDR IPV6_PEER [MTU]

Examples:
  setup-tun-test.sh tun0 10.77.0.1/30 10.77.0.2 fd00:77::1/127 fd00:77::2
  setup-tun-test.sh tun1 10.77.0.2/30 10.77.0.1 fd00:77::2/127 fd00:77::1 1280

Pass '-' for an address family you want to skip:
  setup-tun-test.sh tun0 10.77.0.1/30 10.77.0.2 - - 1280
EOF
}

if [[ $# -lt 5 || $# -gt 6 ]]; then
  usage >&2
  exit 1
fi

iface=$1
ipv4_cidr=$2
ipv4_peer=$3
ipv6_cidr=$4
ipv6_peer=$5
mtu=${6:-1280}

ip link set dev "$iface" mtu "$mtu"

if [[ "$ipv4_cidr" != "-" || "$ipv4_peer" != "-" ]]; then
  if [[ "$ipv4_cidr" == "-" || "$ipv4_peer" == "-" ]]; then
    echo "IPv4 config requires both IPV4_CIDR and IPV4_PEER" >&2
    exit 1
  fi
  ip addr replace "$ipv4_cidr" peer "$ipv4_peer" dev "$iface"
fi

if [[ "$ipv6_cidr" != "-" || "$ipv6_peer" != "-" ]]; then
  if [[ "$ipv6_cidr" == "-" || "$ipv6_peer" == "-" ]]; then
    echo "IPv6 config requires both IPV6_CIDR and IPV6_PEER" >&2
    exit 1
  fi
  ip -6 addr replace "$ipv6_cidr" dev "$iface"
  ip -6 route replace "$ipv6_peer" dev "$iface"
fi

ip link set dev "$iface" up

echo "Configured $iface (mtu=$mtu)"
