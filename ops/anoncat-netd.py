#!/usr/bin/env python3
"""anoncat-netd: keep anoncat reachable through Proton VPN port forwarding.

Every RENEW seconds it:
  1. renews the NAT-PMP port mapping on Proton's gateway (leases last 60s),
  2. if the exit IP or forwarded port changed, points Dynu DNS at them:
       A    anoncat.gleeze.com               -> Proton exit IP
       AAAA anoncat.gleeze.com               -> off (NAT-PMP forwards IPv4 only)
       SRV  _matrix-fed._tcp / _matrix._tcp  -> anoncat.gleeze.com:<port>
  3. writes the current address to STATE_FILE for other tools.

DNS only ever receives the address Proton's gateway reports, never one
looked up from the outside, so the home IP can't end up in DNS even if the
tunnel is down: with no gateway answer, nothing is updated.

Usage:
  anoncat-netd.py            run forever
  anoncat-netd.py --once     one renewal + DNS check, then exit
  anoncat-netd.py --dry-run  like --once, but print the DNS change instead of sending it
  anoncat-netd.py --show     print the domain's current Dynu settings and records
"""

import json
import os
import socket
import struct
import sys
import time
import urllib.request

DOMAIN = os.environ.get("ANONCAT_DOMAIN", "anoncat.gleeze.com")
GATEWAY = os.environ.get("ANONCAT_GATEWAY", "10.2.0.1")
KEY_FILE = os.environ.get("ANONCAT_DYNU_KEY_FILE", os.path.expanduser("~/.config/anoncat/dynu-key"))
STATE_FILE = os.environ.get("ANONCAT_STATE_FILE", os.path.expanduser("~/.config/anoncat/address.json"))
API = "https://api.dynu.com/v2"
TTL = 60
# The second name is the pre-v1.8 one, still the only one some older servers look up.
SRV_NAMES = ("_matrix-fed._tcp", "_matrix._tcp")
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


def dynu(method, path, body=None):
    with open(KEY_FILE) as f:
        key = f.read().strip()
    req = urllib.request.Request(
        API + path,
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"API-Key": key, "Content-Type": "application/json", "Accept": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=20) as r:
        raw = r.read()
    return json.loads(raw) if raw else None


def domain():
    return next(d for d in dynu("GET", "/dns")["domains"] if d["name"] == DOMAIN)


def set_dns(ip, port):
    dom = domain()
    # The domain object carries the A/AAAA settings: send it back with ours.
    dom.update({"ipv4Address": ip, "ipv4": True, "ipv6": False, "ttl": TTL})
    dynu("POST", f"/dns/{dom['id']}", dom)
    existing = {
        r["nodeName"]: r
        for r in dynu("GET", f"/dns/{dom['id']}/record")["dnsRecords"]
        if r["recordType"] == "SRV" and r["nodeName"] in SRV_NAMES
    }
    for name in SRV_NAMES:
        rec = {"nodeName": name, "recordType": "SRV", "ttl": TTL, "state": True,
               "host": DOMAIN, "priority": 0, "weight": 5, "port": port}
        if name in existing:
            dynu("POST", f"/dns/{dom['id']}/record/{existing[name]['id']}", rec)
        else:
            dynu("POST", f"/dns/{dom['id']}/record", rec)


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
        log(f"would set DNS: A {DOMAIN} {ip}, no AAAA, SRV {', '.join(SRV_NAMES)} -> {DOMAIN}:{port}")
        return
    set_dns(ip, port)
    save_state({"ip": ip, "port": port, "since": int(time.time())})
    log(f"DNS now {DOMAIN} -> {ip}:{port} (was {state.get('ip')}:{state.get('port')})")


def show():
    dom = domain()
    print(json.dumps({k: v for k, v in dom.items() if k != "token"}, indent=2))
    print(json.dumps(dynu("GET", f"/dns/{dom['id']}/record")["dnsRecords"], indent=2))


def main():
    args = sys.argv[1:]
    if args == ["--show"]:
        show()
        return
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
