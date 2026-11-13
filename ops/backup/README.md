# anoncat backups

Two different things, kept in two different places:

| What | Where | Protects against |
|---|---|---|
| Database backups (RocksDB backup engine, online) | `/Drives/server/data/backups`, **inside the vault** | mistakes, a bad update, a corrupted database |
| LUKS header backup | **off this machine** | a damaged vault header, which would otherwise make all data unreadable even with the passphrase |

Media files (`db/media`) and plugins (`db/plugins`) are files next to the database, not in it, so database backups don't include them. Remote media expires anyway (cleanup); local uploads exist only there.

Neither protects against a dead drive by itself: the database backups sit on the same drive as the database. For that, also copy the backup directory to an encrypted drive elsewhere now and then (see the end).

## Database backups

The server makes them itself while it keeps running: RocksDB flushes, then copies the table files into `database_backup_path`. Files unchanged since the last backup are shared rather than copied again, so a daily backup costs only what changed. The server keeps the newest `database_backups_to_keep` and deletes older ones.

Config (in the vault's `anoncat.toml`; the installer adds it):

```toml
database_backup_path = "/Drives/server/data/backups"
database_backups_to_keep = 2
admin_signal_execute = ["server backup-database"]
```

Ways to make one:

- **Daily timer:** `anoncat-backup.timer` (04:30 ± 30 min) runs `anoncat-backup run`, which sends the server SIGUSR2 (running `admin_signal_execute`) and waits until a new backup shows up. If none does within an hour, the unit fails: `systemctl status anoncat-backup`.
- **Panel:** Backups page → *Back up now*. The page also lists the backups, their size and the vault's free space, lets you verify one (re-reads it against its checksums), and warns when the newest backup is over two days old.
- **Console:** `server backup-database`, `server list-backups`, `server verify-backup`.
- **Shell (root):** `anoncat-backup run`, `anoncat-backup list`.

The first backup is about the size of the database. Keep that much free in the vault.

## Restoring a database backup

Like restoring a SQLite file from a copy: stop the server, put the backup in place, start it. Here the server does the copying itself, from inside its own sandbox (so the VPN-only killswitch still applies), with `--restore-backup`:

```sh
sudo anoncat-backup list            # pick a backup number; 0 = the newest
sudo anoncat-backup restore 0       # asks you to type RESTORE
```

What `restore` does, so you can also do it by hand:

1. `systemctl stop anoncat.socket anoncat.service`
2. moves the database aside: `db` → `db.before-restore-<date>` (nothing is deleted)
3. creates an empty `db` owned by anoncat and moves `media/` and `plugins/` back into it (they aren't in the backup)
4. adds a runtime drop-in (`/run/systemd/system/anoncat.service.d/anoncat-restore.conf`) whose `ExecStart` adds `--restore-backup <N>`, starts the service once — it restores, opens the database and starts serving — then removes the drop-in, so no later restart restores again
5. once you're happy, delete `db.before-restore-<date>` yourself

Anything that happened after the backup was made is gone from this server: messages, new accounts, the records of newer uploads (their files stay in `media/` but nothing points at them). Other servers keep their copies of federated rooms and send recent events again as the rooms continue.

If the restored server misbehaves, stop it, move `db/media` and `db/plugins` back into `db.before-restore-<date>`, move `db` aside and rename `db.before-restore-<date>` to `db`.

## LUKS header backup (do this once, and after every passphrase change)

The vault's first 16 MiB hold the LUKS header: the encrypted copies of the key. If those sectors get damaged, the passphrase no longer opens anything and every byte inside is lost. A header backup lets you put it back.

```sh
# as root, writing somewhere OUTSIDE the vault (it refuses paths inside)
sudo anoncat-backup header /root/anoncat-luks-header.img
# then copy it off this machine, e.g. to a USB stick you keep elsewhere
sudo cp /root/anoncat-luks-header.img /media/usb/
sudo shred -u /root/anoncat-luks-header.img
```

Keep in mind:

- The header backup **plus the passphrase** opens the vault. Store it like you store the passphrase, and never next to it.
- After changing the passphrase (`cryptsetup luksChangeKey`), an old header backup still opens with the **old** passphrase. Make a new header backup and destroy the old copies.
- To restore a damaged header (it overwrites the current one; only do this when the vault no longer opens):

  ```sh
  sudo anoncat-vault lock
  sudo cryptsetup luksHeaderRestore /Drives/server/vault/anoncat.luks \
       --header-backup-file /media/usb/anoncat-luks-header.img
  sudo anoncat-vault unlock
  ```

## Copies off this machine (optional, recommended)

The backup directory is plain files once the vault is unlocked. To survive a dead drive, copy it, and the media, to an **encrypted** external drive (another LUKS volume) between backups, i.e. not around 04:30:

```sh
sudo rsync -a --delete /Drives/server/data/backups/ /media/encrypted-usb/anoncat-backups/
sudo rsync -a /Drives/server/data/db/media/ /media/encrypted-usb/anoncat-media/
```

Copy the whole backup directory: a backup is only complete with its `meta/`, `private/` and shared-file subdirectories together. To restore from such a copy, put it back at `database_backup_path` first, then follow the restore steps above.
