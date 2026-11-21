# Video worker

Videos come from anyone: people here, and every server whose media someone
here opens. ffmpeg reads hundreds of formats, and a bug in any of them would
be code running with whatever ffmpeg can reach. So ffmpeg never runs inside
the server.

- `anoncat-video` is what the server runs (`media_video_thumbnail_command`
  and `media_video_worker` in the config). It sends the staged video and a job
  name over `/run/anoncat-video.sock` and prints what comes back.
- `anoncat-video-helper` runs on the other end, started by systemd for each
  connection (`anoncat-video.socket`, `anoncat-video@.service`). It runs
  ffmpeg with fixed arguments for one of these jobs:
  - `frame`: one still, for clients' video thumbnails;
  - `sheet`: six stills from across the video, for the panel;
  - `playable`: a WebM copy (VP8 and Opus, open codecs every browser plays
    without extra codec packages), for the panel.
  - `clean`: the same video in its own container, streams copied untouched,
    without metadata (location, camera, dates, names), chapters or data tracks
    (GoPro and dashcam GPS). Every video uploaded here goes through it before
    it is stored; one it can't clean is held back with a warning, and stored
    as it is if the person sends it again within 10 minutes. Videos over the size limit
    (`media_video_thumbnail_max_size`) are stored without cleaning.

  The request names a job, never ffmpeg's arguments, so even a compromised
  server can't make it run anything else.

The worker runs as a throwaway user with no network, no view of the vault or
home directories, no devices and no capabilities. Its `/tmp` is in RAM, so a
staged video never touches the unencrypted disk. It has a 3 GiB memory cap,
the lowest CPU and disk priority, cores 0-3 only, and four minutes at most.
ffmpeg opens only the staged file, read as a common video container: no
playlist formats that would open other files or URLs.

Only root and the `anoncat` group can connect, and only two jobs run at a
time. `anoncat-install` installs and starts all of it.
