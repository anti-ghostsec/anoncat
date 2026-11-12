# The relay VPS

anoncat runs at home, behind Proton VPN. A small VPS with a fixed address
sits in front of it, so the server's address never changes and people reach
it on the standard ports, with no port forward or Codeberg Pages involved.

```
apps, other servers ──443/8448/53383──▶ VPS ──WireGuard──▶ home (10.77.0.2:8449)
                                                 ▲
                                    carried inside Proton (wg0)
```

- **TLS ends at home.** The VPS forwards packets untouched (nftables DNAT),
  so it only ever sees encrypted traffic. It holds no keys, data or logs.
- **The VPS never learns the home address.** Home connects to it only through
  Proton; a firewall rule drops the tunnel's packets if they'd leave any other
  way. SSH on the VPS only answers through the tunnel, from home:
  `ssh -b 10.77.0.2 root@10.77.0.1` (the provider's web console if the
  tunnel is down).
- **Replaceable.** A new VPS anywhere becomes the relay in minutes; DNS
  follows `ANONCAT_RELAY_IP` in `/etc/anoncat/relay.env`.

## Setting one up

1. Order a VPS with a plain Debian image and nothing else, with your SSH key.
2. On the VPS, through the VPN: `ssh root@<vps> bash -s < ops/relay/relay-setup`.
   It prints the VPS's tunnel key (`PUB=`).
3. At home: `sudo anoncat-relay <vps-address> 39214 <that key>`. It prints
   this machine's tunnel key.
4. On the VPS: `relay-peer <home key>`, then run `relay-setup` once more
   through the tunnel (`ssh -b 10.77.0.2 root@10.77.0.1 bash -s < ops/relay/relay-setup`
   after the first ssh); that closes SSH to the public.

`anoncat-netd` then points `anoncat.gleeze.com` and `hs.anoncat.gleeze.com`
at the VPS, and anoncat serves `.well-known`, the front page and `/rules`
itself.
