#!/usr/bin/env python3
"""anoncat-netd: keep anoncat reachable through Proton VPN port forwarding.

Every RENEW seconds it:
  1. renews the NAT-PMP port mapping on Proton's gateway (leases last 60s),
  2. if the exit IP or forwarded port changed, points Dynu DNS at them:
       A    hs.anoncat.gleeze.com            -> Proton exit IP
       SRV  _matrix-fed._tcp / _matrix._tcp  -> hs.anoncat.gleeze.com:<port>
       A    anoncat.gleeze.com               -> Proton exit IP, or Codeberg Pages
     and, in pages mode, publishes the signpost Matrix apps read,
       https://anoncat.gleeze.com/.well-known/matrix/client
         -> {"m.homeserver": {"base_url": "https://hs.anoncat.gleeze.com:<port>"}}
     so users type only anoncat.gleeze.com and a new port reaches them on its
     own,
  3. writes the current address to STATE_FILE for other tools,
  4. with ANONCAT_REDIRECT=1 (as the service runs it), keeps the nftables
     table `inet anoncat` redirecting the forwarded connections on wg0 to
     anoncat's socket, re-creating it if something flushed the ruleset.
     Proton delivers them to the mapping's internal port (1), not to the
     public port that DNS advertises. It also points this machine's own
     connections to the public address at the local listener, since the
     tunnel doesn't carry them back here.

Pages mode needs a Codeberg account (CODEBERG_FILE: username, then token)
and a certificate that covers hs.anoncat.gleeze.com; until both are there,
anoncat.gleeze.com itself keeps pointing at Proton.

DNS only ever receives the address Proton's gateway reports, never one
looked up from the outside, so the home IP can't end up in DNS even if the
tunnel is down: with no gateway answer, nothing is updated.

Usage:
  anoncat-netd.py            run forever
  anoncat-netd.py --once     one renewal + DNS check, then exit
  anoncat-netd.py --dry-run  like --once, but print the DNS change instead of sending it
  anoncat-netd.py --show     print the domain's current Dynu settings and records
"""

import base64
import ipaddress
import json
import os
import socket
import struct
import subprocess
import sys
import time
import ssl
import urllib.error
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
# The loopback listener (TLS, same certificate) for this machine's own clients.
LOOPBACK_PORT = int(os.environ.get("ANONCAT_LOOPBACK_PORT", "8449"))
REDIRECT = os.environ.get("ANONCAT_REDIRECT") == "1"

HS_NODE = "hs"
HS = f"{HS_NODE}.{DOMAIN}"
CODEBERG_FILE = os.environ.get("ANONCAT_CODEBERG_FILE", "/Drives/server/data/secrets/codeberg")
CERT_FILE = os.environ.get("ANONCAT_CERT_FILE", f"/Drives/server/data/tls/certificates/{DOMAIN}.crt")
CODEBERG_API = "https://codeberg.org/api/v1"
PAGES_REPO = "pages"
PAGES_BRANCH = "pages"
# docs.codeberg.org/codeberg-pages/using-custom-domain: Dynu can't do ALIAS at
# the domain itself, so these, which Codeberg says may change.
PAGES_IPV4 = "217.197.84.141"
# Bumped when what set_dns publishes changes, so a running setup re-applies it.
DNS_LAYOUT = 2


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


def set_dns(ip, port, pages):
    dom = domain()
    # The domain object carries the A/AAAA settings: send it back with ours.
    if pages:
        # IPv4 only: Dynu gives a name's IPv6 to every sub-name without its
        # own, and hs has none (Proton forwards IPv4), so publishing
        # Codeberg's would send IPv6 apps for hs to Codeberg.
        dom.update({"ipv4Address": PAGES_IPV4, "ipv4": True, "ipv6": False, "ttl": TTL})
    else:
        dom.update({"ipv4Address": ip, "ipv4": True, "ipv6": False, "ttl": TTL})
    dynu("POST", f"/dns/{dom['id']}", dom)

    records = dynu("GET", f"/dns/{dom['id']}/record")["dnsRecords"]

    def upsert(node, kind, fields):
        rec = {"nodeName": node, "recordType": kind, "ttl": TTL, "state": True, **fields}
        found = next((r for r in records if r["nodeName"] == node and r["recordType"] == kind), None)
        if found:
            dynu("POST", f"/dns/{dom['id']}/record/{found['id']}", rec)
        else:
            dynu("POST", f"/dns/{dom['id']}/record", rec)

    upsert(HS_NODE, "A", {"ipv4Address": ip})
    for name in SRV_NAMES:
        upsert(name, "SRV", {"host": HS, "priority": 0, "weight": 5, "port": port})
    if pages:
        user, _ = codeberg_login()
        upsert("_git-pages-repository", "TXT",
               {"textData": f"https://codeberg.org/{user}/{PAGES_REPO}.git"})


# --- Codeberg Pages: the signpost on the standard port --------------------------

def codeberg_login():
    with open(CODEBERG_FILE) as f:
        user, token = (line.strip() for line in f.read().splitlines()[:2])
    return user, token


def codeberg(method, path, body=None, ok_missing=False):
    _, token = codeberg_login()
    req = urllib.request.Request(
        CODEBERG_API + path,
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"token {token}", "Content-Type": "application/json",
                 "Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
    except urllib.error.HTTPError as e:
        if ok_missing and e.code == 404:
            return None
        # Codeberg says why (a missing token scope, a wrong name): keep it.
        detail = e.read()[:300].decode(errors="replace")
        raise RuntimeError(f"Codeberg {method} {path}: HTTP {e.code}: {detail}") from None
    return json.loads(raw) if raw else None


def cert_covers_hs():
    """Whether the server's certificate is valid for HS yet."""
    try:
        cert = ssl._ssl._test_decode_cert(CERT_FILE)
    except (OSError, ssl.SSLError):
        return False
    return ("DNS", HS) in cert.get("subjectAltName", ())


def pages_mode():
    return os.path.exists(CODEBERG_FILE) and cert_covers_hs()


def pages_files(port):
    client = json.dumps({"m.homeserver": {"base_url": f"https://{HS}:{port}"}}, indent=2) + "\n"
    return {
        "matrix-client.json": client,
        # Served under the path Matrix apps ask for, whether or not the host
        # serves dot-directories itself.
        "_redirects": "/.well-known/matrix/client /matrix-client.json 200\n",
        # Element's web app reads it from another origin.
        "_headers": "/matrix-client.json\n    Access-Control-Allow-Origin: *\n"
                    "/.well-known/matrix/client\n    Access-Control-Allow-Origin: *\n",
        "index.html": f"<!doctype html><title>{DOMAIN}</title><p>{DOMAIN} is a Matrix server.</p>\n",
    }


def publish_pages(port):
    """Create the pages repository and its deploy webhook if needed, then
    bring its files up to date; a push deploys them."""
    user, _ = codeberg_login()
    repo = f"/repos/{user}/{PAGES_REPO}"
    info = codeberg("GET", repo, ok_missing=True)
    if info is None:
        # Creating a repository takes the token's "user" scope as well; a
        # repository-only token can't, so the admin makes it once by hand.
        try:
            info = codeberg("POST", "/user/repos", {"name": PAGES_REPO, "private": False,
                                                    "auto_init": True, "default_branch": PAGES_BRANCH,
                                                    "description": f"Matrix discovery for {DOMAIN}"})
            log(f"pages: created codeberg.org/{user}/{PAGES_REPO}")
        except RuntimeError as e:
            raise RuntimeError(f"create a public repository named '{PAGES_REPO}' on Codeberg "
                               f"(tick 'Initialize repository'), then this continues by itself: {e}") from None
    if codeberg("GET", f"{repo}/branches/{PAGES_BRANCH}", ok_missing=True) is None:
        codeberg("POST", f"{repo}/branches", {"new_branch_name": PAGES_BRANCH,
                                              "old_branch_name": info["default_branch"]})
        log(f"pages: created the {PAGES_BRANCH} branch")
    hook_url = f"http://{DOMAIN}/"
    hooks = codeberg("GET", f"{repo}/hooks") or []
    if not any(h.get("config", {}).get("url") == hook_url for h in hooks):
        codeberg("POST", f"{repo}/hooks", {"type": "forgejo", "active": True, "events": ["push"],
                                           "branch_filter": PAGES_BRANCH,
                                           "config": {"url": hook_url, "content_type": "json"}})
        log("pages: added the deploy webhook")
    for path, text in pages_files(port).items():
        current = codeberg("GET", f"{repo}/contents/{path}?ref={PAGES_BRANCH}", ok_missing=True)
        content = base64.b64encode(text.encode()).decode()
        if current and base64.b64decode(current.get("content", "")) == text.encode():
            continue
        body = {"content": content, "branch": PAGES_BRANCH, "message": f"Update {path}"}
        if current:
            codeberg("PUT", f"{repo}/contents/{path}", {**body, "sha": current["sha"]})
        else:
            codeberg("POST", f"{repo}/contents/{path}", body)
        log(f"pages: updated {path}")


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


def redirect(port, ip, public):
    """Make `inet anoncat` redirect exactly this local port on wg0, and this
    machine's own connections to ip:public to the loopback listener. Built
    afresh once per start, then only when something changes or the table is
    gone."""
    global _applied
    out = subprocess.run([NFT, "-j", "list", "set", "inet", "anoncat", "forwarded"],
                         capture_output=True, text=True)
    if out.returncode == 0:
        sets = [x["set"] for x in json.loads(out.stdout)["nftables"] if "set" in x]
        if sets and sets[0].get("elem") == [port] and _applied == (port, ip, public):
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
    chain output {{
        type nat hook output priority dstnat; policy accept;
        ip daddr {ip} tcp dport {public} redirect to :{LOOPBACK_PORT}
    }}
}}
"""
    subprocess.run([NFT, "-f", "-"], input=script, text=True, check=True)
    _applied = (port, ip, public)
    log(f"redirect: wg0 tcp {port} -> {LOCAL_PORT}; local {ip}:{public} -> {LOOPBACK_PORT}")


def step(dry_run=False):
    ip, port, local = forward()
    if REDIRECT and not dry_run:
        redirect(local, ip, port)
    pages = pages_mode()
    state = load_state()
    if (state.get("ip"), state.get("port"), state.get("pages"), state.get("layout")) == (
            ip, port, pages, DNS_LAYOUT):
        return
    front = "Codeberg Pages" if pages else ip
    if dry_run:
        log(f"would set DNS: A {HS} {ip}, SRV {', '.join(SRV_NAMES)} -> {HS}:{port}, A {DOMAIN} {front}")
        return
    # Signpost first: by the time DNS sends apps to Codeberg it says the new port.
    if pages:
        publish_pages(port)
    set_dns(ip, port, pages)
    save_state({"ip": ip, "port": port, "pages": pages, "layout": DNS_LAYOUT,
                "since": int(time.time())})
    log(f"DNS now {HS} -> {ip}:{port}, {DOMAIN} -> {front} "
        f"(was {state.get('ip')}:{state.get('port')}, pages {state.get('pages')})")


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
