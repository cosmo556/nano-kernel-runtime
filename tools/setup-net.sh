#!/bin/bash
set -e
OIF=$(ip route | grep default | awk '{print $5}')
BRIDGE="nkr0"
SUBNET="10.0.0"

if ! ip link show $BRIDGE >/dev/null 2>&1; then
    ip link add name $BRIDGE type bridge
    ip addr add $SUBNET.1/24 dev $BRIDGE
    ip link set $BRIDGE up
    sysctl -w net.ipv4.ip_forward=1 >/dev/null
    iptables -t nat -A POSTROUTING -s $SUBNET.0/24 -o $OIF -j MASQUERADE
    iptables -A FORWARD -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
    iptables -A FORWARD -i $BRIDGE -o $OIF -j ACCEPT
fi

create_tap() {
    local TAP=$1
    if ip link show $TAP >/dev/null 2>&1; then
        ip link set $TAP down
        ip link delete $TAP
    fi
    # Simple TAP device owned by the user
    ip tuntap add dev $TAP mode tap user $SUDO_USER
    ip link set $TAP master $BRIDGE
    ip link set $TAP up
}

COMMAND=$1
case $COMMAND in
    tap) create_tap $2 ;;
esac
