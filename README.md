# anoncat

<!-- ANCHOR: catchphrase -->

## A small, privacy-minded Matrix homeserver

<!-- ANCHOR_END: catchphrase -->

<!-- ANCHOR: body -->

anoncat is the [Matrix](https://matrix.org/) homeserver behind `anoncat.gleeze.com`. It works with
any Matrix app (Element, FluffyChat, Whisker and others) and talks to the rest of the Matrix
network like any other server.

### Where it comes from

anoncat started from a copy of the source code of [Tuwunel](https://github.com/matrix-construct/tuwunel),
an open-source Matrix homeserver written in Rust, released under the Apache 2.0 licence. We took a
copy of their code as our starting point rather than forking their repository, because anoncat is
heading its own way: it is run for one community, with different priorities, and we didn't want to
present it as part of their project or send them changes they never asked for.

All the credit for the homeserver underneath (federation, the database, the Matrix protocol
work) belongs to the Tuwunel developers and to conduwuit and Conduit before them. Everything listed
under "What anoncat adds" is our own work, and any problem in it is ours, not theirs. **Please don't
take questions or bug reports about anoncat to the Tuwunel developers.**

The licence and copyright notices of the original code are kept in [LICENSE](LICENSE). Files we
changed are recorded in this repository's history.

### What anoncat adds

**Privacy**

- Client IP addresses are never stored, and are kept out of logs. Logs live in memory only.
- The server's software version and user count are not published.
- Location and camera details are removed from uploaded JPEG, PNG and WebP pictures. Other picture
  formats are not cleaned yet.
- Messages are deleted after 30 days, and messages with a picture, video or file after 90.
  Encrypted messages can't be told apart, so they go after 30.
- Presence (who is online) is off.
- All data (database, media, keys, logs) lives in an encrypted container that is locked after
  every restart and needs a passphrase to open.

**Running the server**

- An admin panel, reachable only from the server machine itself: users, rooms, reports, media,
  cleanup, backups, plugins and logs.
- Sign-up from a web page with a captcha drawn by the server itself (no third-party captcha
  service), limited to a number of accounts an hour. Matrix apps signing up directly still need a
  registration token.
- Daily online database backups inside the encrypted container.
- Plugins that run in a WebAssembly sandbox, with each capability approved by an admin.
- Live updates that keep the listening socket open.
- Scripts for running the server from a home machine behind a VPN with a forwarded port
  (`ops/`).

**Moderation**

- Admins can block users, servers and rooms for the whole server. Blocked media is replaced with a
  stand-in picture.
- Optional AI moderation, off unless an admin turns it on. When it is on, content is sent to a
  third-party, no-retention AI service to be checked. Matrix user IDs, room IDs,
  aliases and event IDs are swapped for placeholders first, but the content itself has to be sent
  as it is, so the service can read it. What can be sent:
  - the text of messages, and pictures and video frames, in rooms that aren't end-to-end
    encrypted;
  - pictures and videos uploaded here, and ones fetched from other servers when someone here opens
    them, wherever they were posted (end-to-end encrypted files can't be read, so they aren't);
  - a room's name, topic, address and join rules when someone here joins it or they change,
    encrypted rooms included, since those aren't encrypted; and its latest messages when the room
    isn't encrypted;
  - what people report with their app's Report button, and the reason they give;
  - the admin's notes on judging, and the reported case a note was written about;
  - for the admin panel's assistant, the admin's questions, a summary of the server's state, and
    the output of the commands it runs.

  End-to-end encrypted messages can't be read by the server, so they are never sent.

**Speed**

- The database cache is warmed at start-up, compaction output can be cached, and joined rooms load
  in parallel during sync.
- `ops/bench` holds a repeatable benchmark used to choose the settings in `ops/config/anoncat.toml`.

### Using it

Sign up at <https://anoncat.gleeze.com/register>, or in any Matrix app by entering
`anoncat.gleeze.com` as the server. Read the rules and privacy page first:
<https://anoncat.gleeze.com/rules>.

End-to-end encryption protects what you say from the server, its admins and anyone who gets hold
of the machine. Turn it on for every room and direct message where it matters.

### Running your own

anoncat is built and run the way Tuwunel is, so most of the [Tuwunel
documentation](https://matrix-construct.github.io/tuwunel/) still applies to the server itself.
Start from `tuwunel-example.toml` for the full list of settings, and from `ops/config/anoncat.toml`
for the settings anoncat uses. `ops/install/anoncat-install` sets up the encrypted container, the
systemd units and the configuration.

The packaging folders (`debian/`, `rpm/`, `arch/`, `nix/`, `docker/`, `quadlet/`) are still
Tuwunel's and have not been adapted for anoncat.

### Reporting a problem

For security problems, use **Report a vulnerability** on this repository's Security tab, so the
report stays private. For anything else, open an issue here. See [SECURITY.md](SECURITY.md).

<!-- ANCHOR_END: body -->

<!-- ANCHOR: footer -->

<!-- ANCHOR_END: footer -->
