# The vault and unlocking it from the panel

All of anoncat's data (database, media, keys, logs) lives in a passphrase-only LUKS2 container, `/Drives/server/vault/anoncat.luks`, mounted at `/Drives/server/data`. Nothing on the machine can open it: after every boot it is locked and anoncat can't start until someone types the passphrase. `anoncat-vault` (root) creates, unlocks, locks and reports on it.

## Boot flow

```
boot
 ├─ anoncat.service          skipped: ConditionPathIsMountPoint=/Drives/server/data fails
 ├─ anoncat-vault-helper.socket   root helper listening on /run/anoncat-vault.sock
 └─ anoncat-unlock.service   unlock page on 127.0.0.1:8450 (the panel's address),
                             only while the vault is NOT mounted

admin opens http://127.0.0.1:8450/_anoncat/panel/  → passphrase box
 └─ page server (user anoncat) ──unix socket──► anoncat-vault-helper (root, one per connection)
      checks the peer is the anoncat user (SO_PEERCRED)
      runs: anoncat-vault unlock --stdin   (passphrase on stdin, straight into cryptsetup)
        ├─ cryptsetup open, fsck, mount
        └─ systemctl start --no-block anoncat-start.service
             ├─ stops anoncat-unlock.service (Conflicts=; it finishes its answer first)
             └─ starts anoncat.socket + anoncat.service
 page shows "starting…", polls, and reloads into the real panel once it answers
```

From a terminal it's still `sudo anoncat-vault unlock`; it stops the unlock page first. `sudo anoncat-vault lock` stops anoncat, locks the vault and brings the unlock page back.

The panel is loopback-only. From another machine, reach it through SSH: `ssh -L 8450:127.0.0.1:8450 this-pc`, then open http://127.0.0.1:8450/_anoncat/panel/ locally.

## Why it's built this way

- **cryptsetup and mount need root; anoncat must never have it.** The root part is the smallest thing that works: a socket-activated helper (`Accept=yes`, a fresh process per connection, `MaxConnections=1`) that does two things only, *status* and *unlock*, and only for the anoncat user. The socket file is `root:anoncat 0660` and the helper checks `SO_PEERCRED` too. The actual unlock is `anoncat-vault`, the same code used from a terminal.
- **The server can't serve the unlock page: its database is in the vault.** So a tiny separate page server (Python standard library, user anoncat, loopback only, hardened unit) takes the panel's address only while the vault is locked, and hands it back.
- **Python, not Rust,** for the helper and page: about 150 lines each, readable in full before running as root, nothing to build on this machine. What Rust would add is guaranteed zeroing of memory; here the passphrase is read into one preallocated buffer, written straight into cryptsetup's stdin (`--key-file=-`, read to EOF, no newline added), and overwritten. It is never decoded into a string, logged, or put on a command line. The page server's HTTP reader keeps a small internal buffer that isn't zeroed; the process is short-lived and has no core dumps (`LimitCORE=0`), and there's no disk swap.
- **Guessing:** each try costs argon2 (~4 s) plus a 3 s delay on failure, one at a time; the socket refuses more than 5 connections in 10 s. The page needs a custom header and a non-form content type, so a web page elsewhere can't post to it (no CORS preflight is ever answered), and a `Host` check refuses DNS rebinding. Anyone who can already run code as a local user can reach the page, as they could reach `sudo`'s prompt; the passphrase is the protection.

## Units

| Unit | What |
|---|---|
| `anoncat-vault-helper.socket` / `anoncat-vault-helper@.service` | the root helper |
| `anoncat-unlock.service` | the unlock page while locked (`WantedBy=multi-user.target`) |
| `anoncat-start.service` | after a page unlock: stop the page, start anoncat |

The installer (`ops/install/anoncat-install`) installs and enables them; the helper and page go to `/usr/local/lib/anoncat/`, the page's files to `/usr/local/lib/anoncat/unlock/`.
