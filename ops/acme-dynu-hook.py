#!/usr/bin/env python3
"""acme-dynu-hook: lego "exec" DNS provider for Dynu.

lego's own Dynu provider hands the challenge to Let's Encrypt before every
Dynu nameserver serves it. This one returns from "present" only once all of
them answer with the expected value, then waits a little longer for good
measure.

  lego --dns exec --dns.disable-cp ...   with EXEC_PATH=<this file>

Called by lego as:  acme-dynu-hook.py present|cleanup <fqdn> <value>
"""

import importlib.util
import json
import os
import random
import socket
import struct
import sys
import time
import urllib.request

spec = importlib.util.spec_from_file_location(
    "netd", os.path.join(os.path.dirname(os.path.abspath(__file__)), "anoncat-netd.py"))
netd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(netd)

TIMEOUT = 300
SETTLE = 20


def doh(name, rtype):
    url = f"https://dns.google/resolve?name={name}&type={rtype}"
    with urllib.request.urlopen(url, timeout=10) as r:
        return [a["data"].rstrip(".") for a in json.load(r).get("Answer", [])]


def txt_has(server_ip, fqdn, value):
    msg = (struct.pack("!HHHHHH", random.randint(0, 65535), 0, 1, 0, 0, 0)
           + b"".join(bytes([len(p)]) + p.encode() for p in fqdn.rstrip(".").split("."))
           + b"\0" + struct.pack("!HH", 16, 1))
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.settimeout(4)
        try:
            s.sendto(msg, (server_ip, 53))
            return value.encode() in s.recv(4096)
        except OSError:
            return False


def node_name(fqdn):
    suffix = "." + netd.DOMAIN
    name = fqdn.rstrip(".")
    if not name.endswith(suffix):
        sys.exit(f"{fqdn} is not under {netd.DOMAIN}")
    return name[: -len(suffix)]


def txt_records(dom_id, node):
    return [r for r in netd.dynu("GET", f"/dns/{dom_id}/record")["dnsRecords"]
            if r["recordType"] == "TXT" and r["nodeName"] == node]


def present(fqdn, value):
    dom_id = netd.domain()["id"]
    node = node_name(fqdn)
    for r in txt_records(dom_id, node):  # leftovers from a failed run
        netd.dynu("DELETE", f"/dns/{dom_id}/record/{r['id']}")
    netd.dynu("POST", f"/dns/{dom_id}/record",
              {"nodeName": node, "recordType": "TXT", "textData": value, "ttl": 60, "state": True})
    servers = {ns: doh(ns, "A")[0] for ns in doh(netd.DOMAIN.split(".", 1)[1], "NS")}
    deadline = time.time() + TIMEOUT
    pending = set(servers)
    while pending and time.time() < deadline:
        time.sleep(5)
        pending = {ns for ns in pending if not txt_has(servers[ns], fqdn, value)}
    if pending:
        sys.exit(f"still missing on {sorted(pending)} after {TIMEOUT}s")
    time.sleep(SETTLE)


def cleanup(fqdn, value):
    dom_id = netd.domain()["id"]
    for r in txt_records(dom_id, node_name(fqdn)):
        netd.dynu("DELETE", f"/dns/{dom_id}/record/{r['id']}")


if __name__ == "__main__":
    action, fqdn, value = sys.argv[1:4]
    {"present": present, "cleanup": cleanup}[action](fqdn, value)
