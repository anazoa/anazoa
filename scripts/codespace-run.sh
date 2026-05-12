#!/bin/sh
set -eu

LOGFILE="anazoa.log"
PIDFILE="anazoa.pid"

start() {
    target/release/anazoa-tun >"$LOGFILE" 2>&1 &
    echo $! >"$PIDFILE"
    sleep 5

    iptables -I FORWARD 1 -i tun0 -o eth0 -j ACCEPT
    iptables -I FORWARD 2 -i eth0 -o tun0 -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT

    nft -f - <<EOF
table ip anazoa_nat {
        chain postrouting {
                type nat hook postrouting priority srcnat; policy accept;
                oifname "eth0" masquerade
        }
}
EOF

    tc qdisc replace dev tun0 root cake bandwidth 500kbit

    ip link set dev tun0 mtu 1280
    ip addr replace 10.77.0.1/30 peer 10.77.0.2 dev tun0
    ip link set dev tun0 up
}

stop() {
    if [ ! -f "$PIDFILE" ]; then
        echo "Not running (no pidfile)"
        return 1
    fi
    PID="$(cat "$PIDFILE")"
    if ! kill -0 "$PID" 2>/dev/null; then
        echo "Not running (stale pidfile)"
        rm -f "$PIDFILE"
        return 1
    fi
    echo "Stopping (pid $PID)..."
    kill "$PID"
    rm -f "$PIDFILE"

    nft -f - <<EOF
flush table ip anazoa_nat
EOF

    iptables -D FORWARD 1
    iptables -D FORWARD 2
}

status() {
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        echo "Running (pid $(cat "$PIDFILE"))"
    else
        echo "Not running"
    fi
}

case "$1" in
    start)   start  ;;
    stop)    stop   ;;
    status)  status ;;
    *) echo "Usage: $0 {start|stop|restart|status}"; exit 1 ;;
esac
