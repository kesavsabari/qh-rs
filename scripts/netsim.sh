#!/usr/bin/env bash
# netsim: test qh-link across simulated home networks on one machine, without root.
#
#   internet 203.0.113.0/24, 2001:db8::/64 (bridge, STUN server at .1)
#     |-- router A (NAT) -- alice 192.168.1.2, 2001:db8:a::2
#     |-- router B (NAT) -- bob   192.168.1.2, 2001:db8:b::2   (same LAN range on purpose)
#     `-- carol 203.0.113.30, 2001:db8::30 (public, can relay)
#
# Usage: scripts/netsim.sh [scenario...]   (default: all)
#   cone       both routers keep ports (most home routers): expect a hole-punched public line
#   symmetric  both routers pick a new port per destination: expect a line via carol
#   ipv6       symmetric v4 NAT but IPv6 behind stateful firewalls: expect an ipv6 line
#   mixed      A keeps ports, B is symmetric: unpunchable without port guessing, expect via carol
#   norelay    symmetric, no carol, no IPv6: expect no line, messages FAIL (git takes over)
#   upnp       symmetric, but router A answers UPnP and opens the port: expect public via upnp
#   openinput  like cone, but routers without an input firewall (some ISP boxes): the first
#              hello leaves a stale entry that remaps the other side's port; expect via carol
#   move       alice's laptop moves to another network mid-line: expect recovery in seconds
#   server     strict routers, no teammate relay, a private `qh relay` on a public host; only alice
#              has it configured (bob learns it from her addresses): expect a relay line
#   lan        same home network, keys only, no relay: the LAN beacon finds them
#   intro      alice and bob know only each other's key (no addresses): they meet through carol,
#              swap addresses on the new session and upgrade to a hole-punched line
# Env: QH_BIN (default target/debug/qh)
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
QH_BIN=${QH_BIN:-$here/target/debug/qh}

if [ "${NETSIM_INSIDE:-}" != 1 ]; then
  # Each scenario gets a fresh world. A pid namespace means everything dies with the script.
  [ $# -eq 0 ] || [ "$1" = all ] && set -- cone symmetric ipv6 mixed norelay upnp openinput move intro lan restart return away restartsym server
  rc=0
  for s in "$@"; do
    env NETSIM_INSIDE=1 QH_BIN="$QH_BIN" unshare -rn -p -f --mount-proc "$0" "$s" || rc=1
  done
  exit $rc
fi

work=$(mktemp -d)
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$work"; }
trap cleanup EXIT

ns() { unshare -n sleep infinity >/dev/null 2>&1 & pids+=($!); echo $!; }
nsx() { local p=$1; shift; nsenter -t "$p" -n -- "$@"; }
veth() { # veth <nsA|-> <ifA> <nsB> <ifB>
  ip link add name "$2" type veth peer name "$4"
  [ "$1" != - ] && ip link set "$2" netns "$1"
  ip link set "$4" netns "$3"
}

# The internet is this namespace.
ip link set lo up
ip link add br0 type bridge && ip link set br0 up
ip addr add 203.0.113.1/24 dev br0
ip -6 addr add 2001:db8::1/64 dev br0 nodad
echo 1 > /proc/sys/net/ipv6/conf/all/forwarding
python3 -I "$here/scripts/stun.py" 203.0.113.1 3478 3479 & pids+=($!)

router() { # router <name> <wan4 last octet> <v6 lan prefix> <nat: cone|symmetric>
  local r h
  r=$(ns); h=$(ns)
  ip link add name "w$1" type veth peer name "u$1"
  ip link set "w$1" netns "$r"; ip link set "u$1" master br0 up
  ip link add name "l$1" type veth peer name "h$1"
  ip link set "l$1" netns "$r"; ip link set "h$1" netns "$h"
  nsx "$r" sh -c "
    ip link set lo up; ip link set w$1 name wan; ip link set wan up; ip link set l$1 name lan; ip link set lan up
    ip addr add 203.0.113.$2/24 dev wan; ip -6 addr add 2001:db8::$2/64 dev wan nodad
    ip addr add 192.168.1.1/24 dev lan; ip -6 addr add $3::1/64 dev lan nodad
    ip -6 route add default via 2001:db8::1
    echo 1 > /proc/sys/net/ipv4/ip_forward; echo 1 > /proc/sys/net/ipv6/conf/all/forwarding"
  local masq=masquerade
  [ "$4" = symmetric ] && masq='masquerade random'
  # Home routers drop unsolicited packets to themselves; NETSIM_OPEN_INPUT=1 models one that doesn't.
  local inpol=drop
  [ -n "${NETSIM_OPEN_INPUT:-}" ] && inpol=accept
  nsx "$r" nft -f - <<EOF
table ip nat {
  chain post { type nat hook postrouting priority 100; oifname "wan" $masq; }
}
table ip dbg {
  set out_sport { type inet_service; flags dynamic; }
  set in_dport { type inet_service; flags dynamic; }
  chain post { type filter hook postrouting priority 200; oifname "wan" meta l4proto udp add @out_sport { udp sport }; }
  chain pre { type filter hook prerouting priority -300; iifname "wan" meta l4proto udp add @in_dport { udp dport }; }
}
table inet fw {
  chain forward {
    type filter hook forward priority 0; policy drop;
    ct state established,related counter accept
    iifname "lan" counter accept
    counter comment "dropped"
  }
  chain input {
    type filter hook input priority 0; policy $inpol;
    ct state established,related accept
    iifname { "lan", "lo" } accept
    meta l4proto { icmp, ipv6-icmp } accept
  }
}
EOF
  nsx "$h" sh -c "
    ip link set lo up; ip link set h$1 name eth0; ip link set eth0 up
    ip addr add 192.168.1.2/24 dev eth0; ip route add default via 192.168.1.1
    ip -6 addr add $3::2/64 dev eth0 nodad; ip -6 route add default via $3::1"
  ip -6 route add "$3::/64" via "2001:db8::$2"
  echo "$r $h"
}

public_host() {
  local h; h=$(ns)
  ip link add name uc type veth peer name hc
  ip link set hc netns "$h"; ip link set uc master br0 up
  nsx "$h" sh -c "ip link set lo up; ip link set hc name eth0; ip link set eth0 up
    ip addr add 203.0.113.30/24 dev eth0; ip route add default via 203.0.113.1
    ip -6 addr add 2001:db8::30/64 dev eth0 nodad; ip -6 route add default via 2001:db8::1"
  echo "$h"
}

key() { head -c32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '='; }

# start_link <ns pid> <name> [env...]: runs `qh link` with a fifo stdin; card goes to $work/<name>.card
# KEYOF=<name>: come back as that member (same key, same home, so same memory).
start_link() {
  local p=$1 n=$2; shift 2
  local who=${KEYOF:-$n}
  mkfifo "$work/$n.in"
  mkdir -p "$work/home-$who"
  [ -f "$work/$who.key" ] || key > "$work/$who.key"
  local mem=(); [ -z "${NETSIM_NO_MEMORY:-}" ] && mem=(--remember "$work/home-$who/link-memory.json")
  nsx "$p" env HOME="$work/home-$who" QH_STUN=203.0.113.1:3478,203.0.113.1:3479 "$@" \
    "$QH_BIN" link --key "$(cat "$work/$who.key")" "${mem[@]}" <"$work/$n.in" >"$work/$n.log" 2>&1 & pids+=($!)
  exec {fd}>"$work/$n.in"; eval "fd_$n=$fd"
}
say() { local fd; eval "fd=\$fd_$1"; echo "$2" >&$fd; }
card() { for _ in $(seq 50); do grep -q '^card: ' "$work/$1.log" 2>/dev/null && { grep -m1 '^card: ' "$work/$1.log" | sed 's/^card: //'; return 0; }; sleep 0.1; done; return 1; }
waitfor() { for _ in $(seq $(( $3 * 10 ))); do grep -q "$2" "$work/$1.log" && return 0; sleep 0.1; done; return 1; }

no_ipv6() { nsx "$1" ip -6 route del default 2>/dev/null; nsx "$1" ip -6 addr flush dev eth0 2>/dev/null; true; }

# A fake UPnP router on A's LAN: answers SSDP, serves a description, maps the port with nft.
fake_igd() {
  local r=$1
  nsx "$r" nft add table ip nat 2>/dev/null
  nsx "$r" nft 'add chain ip nat pre { type nat hook prerouting priority -100; }'
  nsx "$r" python3 -I - "$work/igd.log" <<'EOF' & pids+=($!)
import socket, struct, threading, subprocess, re, sys
log = lambda m: open(sys.argv[1], "a").write(m + "\n")
from http.server import BaseHTTPRequestHandler, HTTPServer
DESC = """<?xml version="1.0"?><root><device><serviceList><service>
<serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
<controlURL>/ctl</controlURL></service></serviceList></device></root>"""
def ssdp():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("", 1900))
    s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton("239.255.255.250") + socket.inet_aton("192.168.1.1"))
    while True:
        d, a = s.recvfrom(2048)
        if b"M-SEARCH" in d:
            s.sendto(b"HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLOCATION: http://192.168.1.1:5000/desc.xml\r\n\r\n", a)
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, body):
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body.encode())
    def do_GET(self): self.reply(DESC)
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"])).decode()
        if "AddPortMapping" in body:
            g = lambda t: re.search(f"<{t}>(.*?)</{t}>", body).group(1)
            subprocess.run(["nft", "add", "rule", "ip", "nat", "pre", "iifname", "wan", "udp", "dport", g("NewExternalPort"),
                            "dnat", "to", f"{g('NewInternalClient')}:{g('NewInternalPort')}"], check=True)
            subprocess.run(["nft", "insert", "rule", "inet", "fw", "forward", "ip", "daddr", g("NewInternalClient"), "udp", "dport", g("NewInternalPort"), "accept"], check=True)
            log(f"mapped udp {g('NewExternalPort')} -> {g('NewInternalClient')}")
            self.reply("<ok/>")
        elif "DeletePortMapping" in body:
            log("unmapped udp " + re.search("<NewExternalPort>(.*?)</NewExternalPort>", body).group(1))
            self.reply("<ok/>")
        else:
            self.reply("<NewExternalIPAddress>203.0.113.10</NewExternalIPAddress>")
threading.Thread(target=ssdp, daemon=True).start()
HTTPServer(("192.168.1.1", 5000), H).serve_forever()
EOF
  sleep 0.5
}

# move: line up between two cone homes, then alice's laptop moves to a third network (new LAN
# range, new public IP). Bob gets her new card the way git would bring it. Measures recovery.
move_scenario() {
  local A B
  read -r RA A < <(router a 10 2001:db8:a cone)
  read -r RB B < <(router b 20 2001:db8:b cone)
  no_ipv6 "$A"; no_ipv6 "$B"
  start_link "$A" alice; start_link "$B" bob
  card alice >/dev/null && card bob >/dev/null || { echo "FAIL move: no card"; return 1; }
  local ca cb; ca=$(card alice); cb=$(card bob)
  local bid=${cb#qhlink:}; bid=${bid:0:6}
  say alice "$cb"; say bob "$ca"
  waitfor alice "^up $bid" 5 || { echo "FAIL move: no first line"; tail "$work"/*.log; return 1; }
  echo "  first line: $(grep "^up $bid" "$work/alice.log" | tail -1)"
  # the new home: router c, LAN 10.0.0.0/24
  local RC; RC=$(ns)
  ip link add name wc type veth peer name uc2; ip link set wc netns "$RC"; ip link set uc2 master br0 up
  ip link add name lc type veth peer name hc2; ip link set lc netns "$RC"; ip link set hc2 netns "$A"
  nsx "$RC" sh -c "ip link set lo up; ip link set wc name wan; ip link set wan up; ip link set lc name lan; ip link set lan up
    ip addr add 203.0.113.40/24 dev wan; ip addr add 10.0.0.1/24 dev lan; echo 1 > /proc/sys/net/ipv4/ip_forward"
  printf '%s\n' 'table ip nat {' \
    '  chain post { type nat hook postrouting priority 100; oifname "wan" masquerade; }' '}' 'table inet fw {' \
    '  chain forward { type filter hook forward priority 0; policy drop; ct state established,related accept; iifname "lan" accept; }' \
    '  chain input { type filter hook input priority 0; policy drop; ct state established,related accept; iifname { "lan", "lo" } accept; }' \
    '}' | nsx "$RC" nft -f -
  local t0; t0=$(date +%s.%N)
  nsx "$A" sh -c "ip link del eth0; ip link set hc2 name eth0; ip link set eth0 up; ip addr add 10.0.0.7/24 dev eth0; ip route add default via 10.0.0.1"
  # her side notices within ~2 s and asks STUN again
  local new=""
  for _ in $(seq 100); do
    say alice "/card"; sleep 0.1
    new=$(grep '^card: ' "$work/alice.log" | tail -1 | sed 's/^card: //')
    grep -q 203.0.113.40 <<<"$new" && break
  done
  local tnew; tnew=$(awk -v a="$(date +%s.%N)" -v b="$t0" 'BEGIN{printf "%.1f", a-b}')
  echo "  alice's new card after ${tnew}s: ${new#*@}"
  say bob "$new"   # what git would deliver
  local tb="" n=0
  while [ $n -lt 300 ]; do
    say alice "ping-a-$n"; say bob "ping-b-$n"; sleep 0.1; n=$((n+1))
    if grep -q "\] ping-a-" "$work/bob.log" && grep -q "\] ping-b-" "$work/alice.log"; then
      tb=$(awk -v a="$(date +%s.%N)" -v b="$t0" 'BEGIN{printf "%.1f", a-b}'); break
    fi
  done
  local ok=FAIL; [ -n "$tb" ] && ok=PASS
  printf '%s %-10s alice moved networks; both directions flowing again after %ss (new card at %ss)\n' "$ok" move "${tb:-never}" "$tnew"
  if [ "$ok" = FAIL ] || [ -n "${NETSIM_VERBOSE:-}" ]; then
    for f in alice bob; do echo "  --- $f"; grep -vE 'ping|^card|^FAILED' "$work/$f.log" | sed 's/^/    /'; echo "    ($(grep -c '^FAILED' "$work/$f.log") failed sends)"; done
  fi
  [ "$ok" = PASS ]
}
# lan: alice and bob on the same home network, knowing only each other's key (no addresses,
# no relay, no git). The LAN beacon must find them: expect a lan line.
lan_scenario() {
  local R A B
  R=$(ns); A=$(ns); B=$(ns)
  ip link add name wl type veth peer name ul; ip link set wl netns "$R"; ip link set ul master br0 up
  nsx "$R" sh -c "ip link set lo up; ip link set wl name wan; ip link set wan up
    ip addr add 203.0.113.10/24 dev wan; ip link add br1 type bridge; ip link set br1 up
    ip addr add 192.168.1.1/24 dev br1; echo 1 > /proc/sys/net/ipv4/ip_forward"
  local h n=2
  for h in "$A" "$B"; do
    ip link add name "la$n" type veth peer name "ha$n"; ip link set "la$n" netns "$R"; ip link set "ha$n" netns "$h"
    nsx "$R" sh -c "ip link set la$n master br1; ip link set la$n up"
    nsx "$h" sh -c "ip link set lo up; ip link set ha$n name eth0; ip link set eth0 up
      ip addr add 192.168.1.$n/24 dev eth0; ip route add default via 192.168.1.1"
    n=$((n+1))
  done
  printf '%s\n' 'table ip nat {' '  chain post { type nat hook postrouting priority 100; oifname "wan" masquerade; }' '}' | nsx "$R" nft -f -
  start_link "$A" alice; start_link "$B" bob
  card alice >/dev/null && card bob >/dev/null || { echo "FAIL lan: no card"; return 1; }
  local ca cb; ca=$(card alice); cb=$(card bob)
  local bid=${cb#qhlink:}; bid=${bid:0:6}
  local t0; t0=$(date +%s.%N)
  say alice "${cb%@*}@"; say bob "${ca%@*}@"   # keys only
  local result=none
  for _ in $(seq 100); do
    grep -q "^up $bid (lan" "$work/alice.log" && { result=lan; break; }
    sleep 0.1
  done
  local secs; secs=$(awk -v a="$(date +%s.%N)" -v b="$t0" 'BEGIN{printf "%.1f", a-b}')
  say alice "hello from alice"; sleep 1
  local got=no; grep -q "\] hello from alice" "$work/bob.log" && got=yes
  local ok=FAIL; [ "$result" = lan ] && [ "$got" = yes ] && ok=PASS
  printf '%s %-10s same LAN, keys only, no relay -> line: %-7s (%ss) delivered=%s\n' "$ok" lan "$result" "$secs" "$got"
  if [ "$ok" = FAIL ] || [ -n "${NETSIM_VERBOSE:-}" ]; then
    for f in alice bob; do echo "  --- $f"; sed 's/^/    /' "$work/$f.log"; done
  fi
  [ "$ok" = PASS ]
}

scenario() {
  local name=$1 natA=$2 natB=$3 carol=$4 v6=$5 expect=$6 upnp=${7:-0} intro=${8:-0} server=${9:-0}
  local A B C
  read -r RA A < <(router a 10 2001:db8:a "$natA")
  read -r RB B < <(router b 20 2001:db8:b "$natB")
  [ "$v6" = 0 ] && { no_ipv6 "$A"; no_ipv6 "$B"; }
  [ "$upnp" = 1 ] && fake_igd "$RA"
  if [ "$server" = 1 ]; then
    # a private `qh relay` on a public host; only alice is set up to use it, bob learns it from her
    local S rk; S=$(public_host); rk=$(key)
    mkdir -p "$work/home-relay"
    nsx "$S" env HOME="$work/home-relay" "$QH_BIN" relay --secret "$rk" >"$work/relay.log" 2>&1 & pids+=($!)
    start_link "$A" alice QH_RELAY="203.0.113.30:47232#$rk"
    start_link "$B" bob QH_RELAY=none
  else
    start_link "$A" alice
    start_link "$B" bob
  fi
  [ "$carol" = 1 ] && { C=$(public_host); start_link "$C" carol; }
  card alice >/dev/null && card bob >/dev/null || { echo "FAIL $name: no card"; cat "$work"/*.log; return 1; }
  local ca cb cc
  ca=$(card alice); cb=$(card bob)
  echo "  alice card addrs: ${ca#*@}"
  echo "  bob   card addrs: ${cb#*@}"
  if [ "$carol" = 1 ]; then
    cc=$(card carol)
    say carol "$ca"; say carol "$cb"; say alice "$cc"; say bob "$cc"
    waitfor alice "^up .*203.0.113.30" 10 && waitfor bob "^up .*203.0.113.30" 10 || echo "  (carol not reached)"
  fi
  local t0; t0=$(date +%s.%N)
  if [ "$intro" = 1 ]; then say alice "${cb%@*}@"; say bob "${ca%@*}@"   # keys only: they meet through carol
  else say alice "$cb"; say bob "$ca"; fi
  local bobid=${cb#qhlink:}; bobid=${bobid:0:6}
  local result="none"
  local tries=120; [ "$intro" = 1 ] && tries=400
  for _ in $(seq $tries); do
    local l; l=$(grep "^up $bobid" "$work/alice.log" | tail -1)
    [ -n "$l" ] && result=$(sed -E 's/^up [^ ]+ \(([^ )]+).*/\1/' <<<"$l")
    [ "$result" = "$expect" ] && break
    sleep 0.1
  done
  local secs; secs=$(awk -v a="$(date +%s.%N)" -v b="$t0" 'BEGIN{print a-b}')
  say alice "hello from alice"
  sleep 1.5
  local got=no; grep -q "\] hello from alice" "$work/bob.log" && got=yes
  [ "$expect" = none ] && sleep 11
  local failed=no; grep -q "^FAILED .* to $bobid" "$work/alice.log" && failed=yes
  if [ "$upnp" = 1 ]; then # alice quits: the router port must be given back
    say alice /quit
    for _ in $(seq 50); do grep -q unmapped "$work/igd.log" 2>/dev/null && break; sleep 0.1; done
    echo "  igd: $(tr '\n' ';' < "$work/igd.log")"
    grep -q unmapped "$work/igd.log" || got="no (port not given back)"
  fi
  local ok=FAIL
  if [ "$expect" = none ]; then [ "$result" = none ] && [ "$failed" = yes ] && ok=PASS
  else [ "$result" = "$expect" ] && [ "$got" = yes ] && ok=PASS; fi
  printf '%s %-10s nat A=%-9s B=%-9s relay=%s v6=%s -> line: %-7s (%.1fs) delivered=%s failed=%s\n' \
    "$ok" "$name" "$natA" "$natB" "$carol" "$v6" "$result" "$secs" "$got" "$failed"
  if [ "$ok" = FAIL ] || [ -n "${NETSIM_VERBOSE:-}" ]; then
    for r in "$RA" "$RB"; do echo "  --- router"; nsx "$r" nft list table inet fw | grep counter | sed 's/^/   /'; nsx "$r" nft list table ip dbg | grep elements | sed 's/^/   /'; done
    for f in alice bob carol; do [ -f "$work/$f.log" ] && { echo "  --- $f"; sed 's/^/    /' "$work/$f.log"; }; done
  fi
  [ "$ok" = PASS ]
}

# comeback <natA> <natB> <who returns: bob|both> <seconds away> <carol 0|1>
# Meet once by cards (what git does today), then quit and come back with NO cards: only what each
# side remembers. Measures time until the line is up again and a message gets across.
comeback() {
  local name=$1 natA=$2 natB=$3 who=$4 away=$5 carol=$6
  local A B C
  read -r RA A < <(router a 10 2001:db8:a "$natA")
  read -r RB B < <(router b 20 2001:db8:b "$natB")
  no_ipv6 "$A"; no_ipv6 "$B"
  start_link "$A" alice; start_link "$B" bob
  [ "$carol" = 1 ] && { C=$(public_host); start_link "$C" carol; }
  card alice >/dev/null && card bob >/dev/null || { echo "FAIL $name: no card"; return 1; }
  local ca cb cc; ca=$(card alice); cb=$(card bob)
  local aid=${ca#qhlink:}; aid=${aid:0:6}
  if [ "$carol" = 1 ]; then cc=$(card carol); say carol "$ca"; say carol "$cb"; say alice "$cc"; say bob "$cc"; fi
  say alice "$cb"; say bob "$ca"
  waitfor bob "^up $aid" 10 || { echo "FAIL $name: never met"; tail -n 20 "$work"/*.log; return 1; }
  sleep 1.5 # the memory file is written
  local first; first=$(grep "^up $aid" "$work/bob.log" | tail -1)
  say bob /quit; [ "$who" = both ] && say alice /quit
  sleep "$away"
  local t0; t0=$(date +%s.%N)
  [ "$who" = both ] && KEYOF=alice start_link "$A" alice2
  KEYOF=bob start_link "$B" bob2
  local res=none secs=never got=no
  for _ in $(seq 400); do
    if grep -q "^up $aid" "$work/bob2.log" 2>/dev/null; then
      secs=$(awk -v a="$(date +%s.%N)" -v b="$t0" 'BEGIN{printf "%.1f", a-b}')
      res=$(grep "^up $aid" "$work/bob2.log" | tail -1 | sed -E 's/^up [^ ]+ \(([^ )]+).*/\1/'); break
    fi
    sleep 0.1
  done
  if [ "$res" != none ]; then
    say bob2 "back from bob" # read once its 2.5 s card wait is over
    local alog=alice; [ "$who" = both ] && alog=alice2
    waitfor "$alog" "\] back from bob" 6 && got=yes
  fi
  local ok=FAIL; [ "$res" != none ] && [ "$got" = yes ] && ok=PASS
  [ -n "${NETSIM_NO_MEMORY:-}" ] && ok="BASE"
  printf '%s %-10s nat A=%-9s B=%-9s back=%s away=%ss carol=%s -> line: %-7s after %ss, delivered=%s\n' \
    "$ok" "$name" "$natA" "$natB" "$who" "$away" "$carol" "$res" "$secs" "$got"
  if [ "$ok" = FAIL ] || [ -n "${NETSIM_VERBOSE:-}" ]; then
    echo "  first meeting: $first"
    for f in alice bob alice2 bob2 carol; do [ -f "$work/$f.log" ] && { echo "  --- $f"; sed 's/^/    /' "$work/$f.log" | grep -v '^    card' | tail -n 15; }; done
    for h in "$work"/home-*/link-memory.json; do [ -f "$h" ] && echo "  mem $h: $(cat "$h")"; done
  fi
  [ "$ok" != FAIL ]
}

case "$1" in
  restart)   comeback restart   cone      cone      both 3  0 ;;
  return)    comeback return    cone      cone      bob  45 0 ;;
  away)      comeback away      cone      cone      bob  150 0 ;;
  restartsym) comeback restartsym symmetric symmetric both 3 1 ;;
  cone)      scenario cone      cone      cone      0 0 public ;;
  symmetric) scenario symmetric symmetric symmetric 1 0 via ;;
  ipv6)      scenario ipv6      symmetric symmetric 0 1 ipv6 ;;
  mixed)     scenario mixed     cone      symmetric 1 0 via ;;
  norelay)   scenario norelay   symmetric symmetric 0 0 none ;;
  openinput) NETSIM_OPEN_INPUT=1 scenario openinput cone cone 1 0 via ;;
  move)      move_scenario ;;
  lan)       lan_scenario ;;
  server)    scenario server    symmetric symmetric 0 0 relay 0 0 1 ;;
  intro)     scenario intro     cone      cone      1 0 public 0 1 ;;
  upnp)      scenario upnp      symmetric symmetric 0 0 public 1 ;;
  *) echo "unknown scenario $1"; exit 2 ;;
esac
