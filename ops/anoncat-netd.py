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

Relay mode (ANONCAT_RELAY_IP, set in /etc/anoncat/relay.env by
anoncat-relay) replaces all of that: a VPS with a fixed address passes
ports 443, 8448 and 53383 down a WireGuard tunnel to this machine, and
anoncat answers there itself, .well-known included. DNS then points both
names at the VPS for good: no port forward, no Pages, nothing that changes.

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
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys
import threading
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
# The relay VPS's public address; empty without a relay.
RELAY_IP = os.environ.get("ANONCAT_RELAY_IP", "").strip()
# What the relay passes down the tunnel: apps and the front page, Matrix's
# server port, and the old forwarded port apps signed in before still use.
RELAY_PORTS = (443, 8448, 53383)
# The second name is the pre-v1.8 one, still the only one some older servers look up.
SRV_NAMES = ("_matrix-fed._tcp", "_matrix._tcp")
RENEW = 45
LEASE = 60
NFT = "/usr/sbin/nft"
LOCAL_PORT = int(os.environ.get("ANONCAT_LOCAL_PORT", "8448"))
# The local listener (TLS, same certificate) for this machine's own clients;
# anoncat.toml has it on the tunnel address as well as loopback.
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
        try:
            while True:
                data, sender = s.recvfrom(64)
                if sender == (GATEWAY, 5351) and len(data) >= size and data[1] == 128 + payload[1]:
                    break
        except TimeoutError:
            raise TimeoutError("Proton's gateway didn't answer the port-forward renewal; "
                               "the VPN tunnel is probably down") from None
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


# The front page and the rules people accept when they sign up
# (registration_terms points at /rules). Plain HTML and CSS, nothing loaded
# from anywhere else. Change "Version" in the rules when they change.
PAGE_CSS = """
:root {
  --bg: #f4f2ee; --card: #ffffff; --text: #1c1c1e; --muted: #6b6b70; --line: #e6e2da;
  --accent: #5b5bd6; --accent-soft: #ecebfd; --accent-text: #ffffff;
  color-scheme: light dark;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #111113; --card: #1b1b1f; --text: #ededf0; --muted: #9a9aa3; --line: #2b2b31;
    --accent: #9b9bff; --accent-soft: #24243a; --accent-text: #111113;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0; min-height: 100vh; color: var(--text);
  font: 16px/1.6 ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  background: radial-gradient(60rem 30rem at 50% -10rem, var(--accent-soft), transparent 70%), var(--bg);
}
main { max-width: 40rem; margin: 0 auto; padding: 3rem 1rem 4rem; }
header { text-align: center; margin-bottom: 2rem; }
.logo {
  width: 3.5rem; height: 3.5rem; margin: 0 auto 1rem; border-radius: 1rem; display: grid; place-items: center;
  font-size: 1.8rem; background: var(--card); border: 1px solid var(--line);
  box-shadow: 0 8px 24px rgb(0 0 0 / 0.08);
}
h1 { margin: 0; font-size: clamp(1.7rem, 5vw, 2.2rem); letter-spacing: -0.02em; line-height: 1.2; }
.sub { margin: 0.4rem 0 0; color: var(--muted); }
.pill {
  display: inline-block; margin-top: 0.8rem; padding: 0.15rem 0.7rem; border-radius: 99px;
  font-size: 0.8rem; color: var(--accent); background: var(--accent-soft);
}
.card {
  background: var(--card); border: 1px solid var(--line); border-radius: 1.25rem;
  padding: 1.5rem 1.5rem 1.25rem; margin: 1rem 0; box-shadow: 0 1px 2px rgb(0 0 0 / 0.04);
}
h2 { margin: 0 0 0.9rem; font-size: 1.1rem; letter-spacing: -0.01em; }
ol, ul { list-style: none; margin: 0; padding: 0; }
li { position: relative; padding: 0.55rem 0 0.55rem 2.4rem; border-top: 1px solid var(--line); }
li:first-child { border-top: 0; }
ol { counter-reset: rule; }
ol li::before {
  counter-increment: rule; content: counter(rule);
  position: absolute; left: 0; top: 0.6rem; width: 1.6rem; height: 1.6rem; border-radius: 50%;
  display: grid; place-items: center; font-size: 0.85rem; font-weight: 700;
  color: var(--accent); background: var(--accent-soft);
}
ul li::before {
  content: ""; position: absolute; left: 0.35rem; top: 1.05rem; width: 0.9rem; height: 0.45rem;
  border-left: 2.5px solid var(--accent); border-bottom: 2.5px solid var(--accent); transform: rotate(-45deg);
}
.note { margin: 0.9rem 0 0; color: var(--muted); font-size: 0.92rem; }
a { color: var(--accent); }
.actions { display: flex; gap: 0.75rem; justify-content: center; flex-wrap: wrap; margin: 1.5rem 0; }
.button {
  display: inline-block; padding: 0.75rem 1.4rem; border-radius: 0.8rem; font-weight: 600;
  text-decoration: none; border: 1px solid var(--line); background: var(--card); color: var(--text);
}
.button.primary { background: var(--accent); border-color: var(--accent); color: var(--accent-text); }
.button:focus-visible, a:focus-visible { outline: 3px solid var(--accent); outline-offset: 2px; }
footer { text-align: center; color: var(--muted); font-size: 0.85rem; margin-top: 2rem; }
"""

RULES_BODY = """
<header>
  <div class="logo" aria-hidden="true">&#x1F431;</div>
  <h1>Rules and privacy</h1>
  <p class="sub">@@DOMAIN@@</p>
  <span class="pill">Version 2</span>
</header>
<section class="card">
  <h2>Rules</h2>
  <ol>
    <li>Nothing illegal. Sexual content involving minors means an instant ban.</li>
    <li>No spam, scams, malware, or bots unless the admins agree.</li>
    <li>No threats, harassment, or posting other people's private details.</li>
  </ol>
  <p class="note">Break them and your posts or account may be removed.</p>
</section>
<section class="card">
  <h2>Privacy</h2>
  <ul>
    <li>No email or phone number is asked for. No tracking, no ads, no analytics.</li>
    <li>Your IP address is not stored or logged.</li>
    <li>Location and camera details are removed from photos you upload (not from videos).</li>
    <li>Admins can technically read rooms that are not encrypted. Use encrypted chats for anything private.</li>
    <li>Other servers in your rooms get copies of your messages and profile, and we can't delete those.</li>
    <li>Messages are deleted after 30 days, or 90 days if they have a picture or file.</li>
    <li>Delete your account any time at <a href="/login">/login</a>. Your uploads go with it, and backups keep it for up to 2 more days.</li>
    <li>There is no password reset. If you lose your password, the account is gone.</li>
  </ul>
</section>
<footer><a href="/">@@DOMAIN@@</a></footer>
"""

INDEX_BODY = """
<header>
  <div class="logo" aria-hidden="true">&#x1F431;</div>
  <h1>@@DOMAIN@@</h1>
  <p class="sub">A private Matrix chat server. No email, no tracking.</p>
</header>
<div class="actions">
  <a class="button primary" href="/register">Create an account</a>
  <a class="button" href="/login">Manage your account</a>
</div>
<section class="card">
  <h2>How to join</h2>
  <ol>
    <li>Create an account above.</li>
    <li>Get a Matrix app, like <a href="https://element.io/download" rel="noopener noreferrer">Element</a>.</li>
    <li>Sign in, choose another homeserver, and enter <strong>@@DOMAIN@@</strong>.</li>
  </ol>
</section>
<footer><a href="/rules">Rules and privacy</a></footer>
"""


def page(title, body):
    return ("<!doctype html>\n<html lang=\"en\">\n<meta charset=\"utf-8\">\n"
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n"
            "<meta name=\"referrer\" content=\"no-referrer\">\n"
            f"<title>{title}</title>\n<style>{PAGE_CSS}</style>\n<main>{body}</main>\n</html>\n"
            ).replace("@@DOMAIN@@", DOMAIN)


def pages_files(port, deployed_at=None):
    client = json.dumps({"m.homeserver": {"base_url": f"https://{HS}:{port}"}}, indent=2) + "\n"
    return {
        "matrix-client.json": client,
        # Served under the path Matrix apps ask for, whether or not the host
        # serves dot-directories itself.
        # /register and /login go to the server's own pages (web_signup),
        # which follow the forwarded port like everything else.
        "_redirects": "/.well-known/matrix/client /matrix-client.json 200\n"
                      f"/register https://{HS}:{port}/register 302\n"
                      f"/login https://{HS}:{port}/login 302\n"
                      f"/account https://{HS}:{port}/account 302\n"
                      "/rules /rules.html 200\n",
        # Element's web app reads it from another origin.
        "_headers": "/matrix-client.json\n    Access-Control-Allow-Origin: *\n"
                    "/.well-known/matrix/client\n    Access-Control-Allow-Origin: *\n",
        "index.html": page(DOMAIN, INDEX_BODY),
        "rules.html": page(f"Rules and privacy - {DOMAIN}", RULES_BODY),
        # Changed only to force a push, which is what makes Codeberg deploy.
        "deployed.txt": f"{deployed_at or 0}\n",
    }


def pages_rev(port):
    """A fingerprint of the pages for this port, leaving out the deploy stamp."""
    files = pages_files(port)
    files.pop("deployed.txt", None)
    return hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest()[:16]


def publish_pages(port, redeploy=False):
    """Create the pages repository and its deploy webhook if needed, then
    bring its files up to date; a push deploys them. redeploy forces a push
    even when nothing else changed: the deploy webhook goes to DOMAIN, so a
    push made before DNS pointed it at Codeberg never deployed."""
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
    current_stamp = codeberg("GET", f"{repo}/contents/deployed.txt?ref={PAGES_BRANCH}", ok_missing=True)
    stamp = int(time.time()) if redeploy or not current_stamp else \
        base64.b64decode(current_stamp.get("content", "")).decode().strip()
    for path, text in pages_files(port, stamp).items():
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


def tunnel_address():
    """This machine's IPv4 address on wg0."""
    out = subprocess.run(["/usr/sbin/ip", "-j", "-4", "addr", "show", "dev", "wg0"],
                         capture_output=True, text=True, check=True)
    return json.loads(out.stdout)[0]["addr_info"][0]["local"]


def redirect(port, ip, public):
    """Proton mode; see redirect_rules."""
    redirect_rules(port, (public,), (port, ip, public))


def redirect_relay():
    """Relay mode: nothing arrives on wg0 any more, but this machine's own
    apps still reach the server through its tunnel address (/etc/hosts), on
    any of the ports the relay serves."""
    redirect_rules(None, RELAY_PORTS, ("relay",))


def redirect_rules(port, public_ports, key):
    """Make `inet anoncat` redirect exactly this local port on wg0, and this
    machine's own connections to the public port on its tunnel address to the
    local listener there. This machine resolves hs.anoncat.gleeze.com to its
    tunnel address (/etc/hosts): the tunnel doesn't carry connections to the
    public address back, and redirecting those instead fails, since their
    replies then claim the public address as their source on loopback and
    wg-quick's anti-spoofing rule drops them. Built afresh once per start,
    then only when something changes or the table is gone."""
    global _applied
    local_ip = tunnel_address()
    out = subprocess.run([NFT, "-j", "list", "set", "inet", "anoncat", "forwarded"],
                         capture_output=True, text=True)
    if out.returncode == 0:
        sets = [x["set"] for x in json.loads(out.stdout)["nftables"] if "set" in x]
        want = [port] if port is not None else None
        if sets and sets[0].get("elem") == want and _applied == (*key, local_ip):
            return
    forwarded = f"elements = {{ {port} }}; " if port is not None else ""
    public = ", ".join(str(p) for p in public_ports)
    # Declared first so the delete can't fail, then rebuilt whole: one
    # transaction, so there's never a moment with a half-made table.
    script = f"""table inet anoncat
delete table inet anoncat
table inet anoncat {{
    set forwarded {{ type inet_service; {forwarded}}}
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
        iifname "wg0" tcp dport @forwarded counter redirect to :{LOCAL_PORT}
    }}
    chain output {{
        type nat hook output priority dstnat; policy accept;
        ip daddr {local_ip} tcp dport {{ {public} }} counter dnat ip to {local_ip}:{LOOPBACK_PORT}
    }}
}}
"""
    subprocess.run([NFT, "-f", "-"], input=script, text=True, check=True)
    _applied = (*key, local_ip)
    inbound = f"wg0 tcp {port} -> {LOCAL_PORT}; " if port is not None else ""
    log(f"redirect: {inbound}local {local_ip}:{public} -> {LOOPBACK_PORT}")


# The DNS and Pages work runs here, apart from the lease renewal: it can take
# minutes (API timeouts, the wait for Codeberg), and the 60 s lease must not
# lapse meanwhile.
_publisher = None


def step(dry_run=False, background=False):
    global _publisher
    if RELAY_IP:
        step_relay(dry_run)
        return
    ip, port, local = forward()
    if REDIRECT and not dry_run:
        redirect(local, ip, port)
    if background:
        if _publisher is not None and _publisher.is_alive():
            return
        _publisher = threading.Thread(target=publish_logged, args=(ip, port), daemon=True)
        _publisher.start()
        return
    publish(ip, port, dry_run)


RELAY_IFACE = "anoncat-relay"


def allow_relay_out():
    """FerroOS's VPN kill switch (`inet ferro_vpn`) lets traffic leave only
    through wg0, so anoncat's answers into the relay tunnel would be refused.
    That tunnel is itself carried inside wg0 (and its own packets may only
    leave there), so it's let through too. The VPN tool rebuilds its table on
    every reconnect, so this is checked on every round."""
    out = subprocess.run([NFT, "-j", "list", "chain", "inet", "ferro_vpn", "filter_out"],
                         capture_output=True, text=True)
    if out.returncode != 0:
        return  # no kill switch right now
    rule = f'oifname "{RELAY_IFACE}" accept'
    for item in json.loads(out.stdout)["nftables"]:
        expr = item.get("rule", {}).get("expr", [])
        if any(e.get("match", {}).get("right") == RELAY_IFACE for e in expr) and \
                any("accept" in e for e in expr):
            return
    subprocess.run([NFT, "insert", "rule", "inet", "ferro_vpn", "filter_out", *rule.split()],
                   check=True)
    log(f"kill switch: let {RELAY_IFACE} through (it runs inside wg0)")


def step_relay(dry_run=False):
    """Relay mode: DNS points at the VPS; only a change of VPS (or of this
    layout) sends anything to Dynu."""
    if not ipaddress.ip_address(RELAY_IP).is_global:
        raise RuntimeError(f"ANONCAT_RELAY_IP {RELAY_IP} isn't a public address")
    if REDIRECT and not dry_run:
        redirect_relay()
        allow_relay_out()
    state = load_state()
    if (state.get("relay"), state.get("layout")) == (RELAY_IP, DNS_LAYOUT):
        return
    if dry_run:
        log(f"would set DNS: A {DOMAIN} and {HS} {RELAY_IP}, SRV {', '.join(SRV_NAMES)} -> {HS}:8448")
        return
    set_dns(RELAY_IP, 8448, pages=False)
    save_state({"relay": RELAY_IP, "ip": RELAY_IP, "port": 8448, "pages": False,
                "layout": DNS_LAYOUT, "since": int(time.time())})
    log(f"DNS now {DOMAIN} and {HS} -> relay {RELAY_IP} (was {state.get('ip')}:{state.get('port')})")


def publish_logged(ip, port):
    try:
        publish(ip, port)
    except Exception as e:  # tried again on the next renewal
        log(f"error publishing DNS or pages: {e}")


def publish(ip, port, dry_run=False):
    pages = pages_mode()
    state = load_state()
    # The pages' content counts too, so a new version of them goes up without
    # waiting for the port to change.
    rev = pages_rev(port) if pages else None
    if (state.get("ip"), state.get("port"), state.get("pages"), state.get("layout"),
            state.get("pages_rev")) == (ip, port, pages, DNS_LAYOUT, rev):
        return
    front = "Codeberg Pages" if pages else ip
    if dry_run:
        log(f"would set DNS: A {HS} {ip}, SRV {', '.join(SRV_NAMES)} -> {HS}:{port}, A {DOMAIN} {front}")
        return
    # Signpost first: by the time DNS sends apps to Codeberg it says the new port.
    if pages:
        publish_pages(port)
    set_dns(ip, port, pages)
    # Pointing DNS at Codeberg (first time, or a new layout): deploy again now
    # that the webhook reaches Codeberg, so it serves the site and issues its
    # certificate.
    if pages and (not state.get("pages") or state.get("layout") != DNS_LAYOUT):
        time.sleep(TTL)
        publish_pages(port, redeploy=True)
    save_state({"ip": ip, "port": port, "pages": pages, "layout": DNS_LAYOUT,
                "pages_rev": rev, "since": int(time.time())})
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
    failing_since = None
    while True:
        try:
            step(background=True)
            if failing_since is not None:
                log(f"working again after {int(time.time() - failing_since)}s of failures")
                failing_since = None
        except Exception as e:  # keep renewing; a missed lease only costs the port
            failing_since = failing_since or time.time()
            what = "keeping DNS on the relay" if RELAY_IP else "renewing the port forward"
            log(f"error {what}: {e}")
        time.sleep(RENEW)


if __name__ == "__main__":
    main()
