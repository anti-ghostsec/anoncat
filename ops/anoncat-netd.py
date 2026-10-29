#!/usr/bin/env python3
"""anoncat-netd: keep anoncat reachable through Proton VPN port forwarding.

Every RENEW seconds it:
  1. renews the NAT-PMP port mapping on Proton's gateway (leases last 60s),
  2. if the exit IP or forwarded port changed, points deSEC DNS at them:
       A    anoncat.dedyn.io                -> Proton exit IP
       AAAA anoncat.dedyn.io                -> removed (NAT-PMP forwards IPv4 only)
       SRV  _matrix-fed._tcp / _matrix._tcp  -> anoncat.dedyn.io:<port>
  3. writes the current address to STATE_FILE for other tools.

DNS only ever receives the address Proton's gateway reports, never one
looked up from the outside, so the home IP can't end up in DNS even if the
tunnel is down: with no gateway answer, nothing is updated.

Usage:
  anoncat-netd.py            run forever
  anoncat-netd.py --once     one renewal + DNS check, then exit
  anoncat-netd.py --dry-run  like --once, but print the DNS change instead of sending it
"""

import json
import os
import socket
import struct
import sys
import time
import urllib.request

DOMAIN = os.environ.get("ANONCAT_DOMAIN", "anoncat.dedyn.io")
GATEWAY = os.environ.get("ANONCAT_GATEWAY", "10.2.0.1")
TOKEN_FILE = os.environ.get("ANONCAT_DESEC_TOKEN_FILE", os.path.expanduser("~/.config/anoncat/desec-token"))
STATE_FILE = os.environ.get("ANONCAT_STATE_FILE", os.path.expanduser("~/.config/anoncat/address.json"))
API = "https://desec.io/api/v1"
RENEW = 45
LEASE = 60


def log(msg):
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def natpmp(payload):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.settimeout(3)
        s.sendto(payload, (GATEWAY, 5351))
        data = s.recv(64)
    rc = struct.unpack("!H", data[2:4])[0]
    if rc != 0:
        raise RuntimeError(f"NAT-PMP error code {rc}")
    return data


def forward():
    """Return (exit_ip, port) from Proton, renewing the TCP and UDP mappings."""
    ip = socket.inet_ntoa(natpmp(b"\x00\x00")[8:12])
    ports = set()
    # Proton's convention: internal port 1, requested public port 0. Renewing
    # with the same request keeps the same port for as long as the tunnel lives.
    for op in (2, 1):  # TCP, UDP
        data = natpmp(struct.pack("!BBHHHI", 0, op, 0, 1, 0, LEASE))
        ports.add(struct.unpack("!H", data[10:12])[0])
    if len(ports) != 1:
        raise RuntimeError(f"TCP and UDP got different ports: {sorted(ports)}")
    return ip, ports.pop()


def desec(method, path, body=None):
    with open(TOKEN_FILE) as f:
        token = f.read().strip()
    req = urllib.request.Request(
        API + path,
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"Token {token}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=20) as r:
        raw = r.read()
    return json.loads(raw) if raw else None


def rrsets(ip, port, ttl):
    return [
        {"subname": "", "type": "A", "ttl": ttl, "records": [ip]},
        {"subname": "", "type": "AAAA", "ttl": ttl, "records": []},
        {"subname": "_matrix-fed._tcp", "type": "SRV", "ttl": ttl, "records": [f"0 5 {port} {DOMAIN}."]},
        # The pre-v1.8 name, still the only one some older servers look up.
        {"subname": "_matrix._tcp", "type": "SRV", "ttl": ttl, "records": [f"0 5 {port} {DOMAIN}."]},
    ]


def load_state():
    try:
        with open(STATE_FILE) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save_state(state):
    os.makedirs(os.path.dirname(STATE_FILE), mode=0o700, exist_ok=True)
    tmp = STATE_FILE + ".tmp"
    with open(tmp, "w") as f:
        json.dump(state, f)
    os.replace(tmp, STATE_FILE)


def step(dry_run=False):
    ip, port = forward()
    state = load_state()
    if state.get("ip") == ip and state.get("port") == port:
        return
    if dry_run:
        log(f"would set DNS: {json.dumps(rrsets(ip, port, 60))}")
        return
    ttl = desec("GET", f"/domains/{DOMAIN}/")["minimum_ttl"]
    desec("PATCH", f"/domains/{DOMAIN}/rrsets/", rrsets(ip, port, ttl))
    save_state({"ip": ip, "port": port, "since": int(time.time())})
    log(f"DNS now {DOMAIN} -> {ip}:{port} (was {state.get('ip')}:{state.get('port')})")


def main():
    args = sys.argv[1:]
    if args and args[0] in ("--once", "--dry-run"):
        step(dry_run=args[0] == "--dry-run")
        return
    while True:
        try:
            step()
        except Exception as e:  # keep renewing; a missed lease only costs the port
            log(f"error: {e}")
        time.sleep(RENEW)


if __name__ == "__main__":
    main()
