#!/usr/bin/env python3
"""anoncat-netd: keep anoncat reachable through Proton VPN port forwarding.

Every RENEW seconds it:
  1. renews the NAT-PMP port mapping on Proton's gateway (leases last 60s),
  2. if the exit IP or forwarded port changed, points Dynu DNS at them:
       A    anoncat.gleeze.com               -> Proton exit IP
       AAAA anoncat.gleeze.com               -> off (NAT-PMP forwards IPv4 only)
       SRV  _matrix-fed._tcp / _matrix._tcp  -> anoncat.gleeze.com:<port>
  3. writes the current address to STATE_FILE for other tools,
  4. with ANONCAT_REDIRECT=1 (as the service runs it), keeps the nftables
     table `inet anoncat` redirecting the forwarded connections on wg0 to
     anoncat's socket, re-creating it if something flushed the ruleset.
     Proton delivers them to the mapping's internal port (1), not to the
     public port that DNS advertises.

DNS only ever receives the address Proton's gateway reports, never one
looked up from the outside, so the home IP can't end up in DNS even if the
tunnel is down: with no gateway answer, nothing is updated.

Usage:
  anoncat-netd.py            run forever
  anoncat-netd.py --once     one renewal + DNS check, then exit
  anoncat-netd.py --dry-run  like --once, but print the DNS change instead of sending it
  anoncat-netd.py --show     print the domain's current Dynu settings and records
"""

import ipaddress
import json
import os
import socket
import struct
import subprocess
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
NFT = "/usr/sbin/nft"
LOCAL_PORT = int(os.environ.get("ANONCAT_LOCAL_PORT", "8448"))
REDIRECT = os.environ.get("ANONCAT_REDIRECT") == "1"


def log(msg):
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def natpmp(payload, size):
    """One NAT-PMP request to Proton's gateway over wg0. Only a well-formed
    answer to this request from the gateway counts: a spoofed reply from the
    LAN must never get an address published."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, b"wg0")
        s.settimeout(3)
        s.sendto(payload, (GATEWAY, 5351))
        while True:
            data, sender = s.recvfrom(64)
            if sender == (GATEWAY, 5351) and len(data) >= size and data[1] == 128 + payload[1]:
                break
    rc = struct.unpack("!H", data[2:4])[0]
    if rc != 0:
        raise RuntimeError(f"NAT-PMP error code {rc}")
    return data


def forward():
    """Return (exit_ip, public_port, local_port) from Proton, renewing the TCP
    and UDP mappings. Connections to the public port arrive through wg0 on
    the local port (the mapping's internal port, 1 by Proton's convention),
    not on the public port."""
    ip = socket.inet_ntoa(natpmp(b"\x00\x00", 12)[8:12])
    if not ipaddress.ip_address(ip).is_global:
        raise RuntimeError(f"gateway reported a non-public exit address {ip}")
    ports, local = set(), set()
    # Proton's convention: internal port 1, requested public port 0. Renewing
    # with the same request keeps the same port for as long as the tunnel lives.
    for op in (2, 1):  # TCP, UDP
        data = natpmp(struct.pack("!BBHHHI", 0, op, 0, 1, 0, LEASE), 16)
        ports.add(struct.unpack("!H", data[10:12])[0])
        local.add(struct.unpack("!H", data[8:10])[0])
    if len(ports) != 1:
        raise RuntimeError(f"TCP and UDP got different ports: {sorted(ports)}")
    if len(local) != 1:
        raise RuntimeError(f"TCP and UDP got different local ports: {sorted(local)}")
    return ip, ports.pop(), local.pop()


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


_applied = None


def redirect(port):
    """Make `inet anoncat` redirect exactly this local port on wg0. Built
    afresh once per start, then only when the port changes or the table is
    gone."""
    global _applied
    out = subprocess.run([NFT, "-j", "list", "set", "inet", "anoncat", "forwarded"],
                         capture_output=True, text=True)
    if out.returncode == 0:
        sets = [x["set"] for x in json.loads(out.stdout)["nftables"] if "set" in x]
        if sets and sets[0].get("elem") == [port] and _applied == port:
            return
    # Declared first so the delete can't fail, then rebuilt whole: one
    # transaction, so there's never a moment with a half-made table.
    script = f"""table inet anoncat
delete table inet anoncat
table inet anoncat {{
    set forwarded {{ type inet_service; elements = {{ {port} }} }}
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
        iifname "wg0" tcp dport @forwarded counter redirect to :{LOCAL_PORT}
    }}
}}
"""
    subprocess.run([NFT, "-f", "-"], input=script, text=True, check=True)
    _applied = port
    log(f"redirect: wg0 tcp {port} -> {LOCAL_PORT}")


def step(dry_run=False):
    ip, port, local = forward()
    if REDIRECT and not dry_run:
        redirect(local)
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
