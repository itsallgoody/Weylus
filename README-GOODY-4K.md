# Goody's 4K path (Windows, Intel Quick Sync)

Branch `goody-4k-qsv`, on top of `goody-windows`.

## What it is

On Windows the stock capture (captrs) copies every desktop frame into system memory and
converts and encodes it there. At 4K that tops out around 11 fps on the Worker PC.

This path keeps the frame on the GPU from capture to bitstream:

```
ddagrab (Desktop Duplication, adapter 0, output N)
  -> hwmap=derive_device=qsv -> format=qsv
  -> scale_qsv  (BGRA -> NV12, fitted to the client's max size, never above 4K)
  -> h264_qsv   (low_power=1, async_depth=1, look_ahead=0, no B-frames, gop 60, ICQ 23, Main)
  -> fragmented MP4, the moov written from the first frame (delay_moov)
```

The same graph from ffmpeg's command line measured 56-59 fps at 2304x1296 for 0.34 cores on
the Worker PC (i9-13900H, Iris Xe). The stream the tablet gets is the same kind as before.

It is used automatically when the client picks a whole monitor ("Desktop ... (captrs)" in the
list) and this build has Quick Sync (the CI's mingw build does). Otherwise nothing changes.

The fallback encoder (h264_mf, Media Foundation) was fixed on the same branch: 12 Mbit/s
average, 20 Mbit/s peak, Main profile, gop 60 (it was running at FFmpeg's default of
200 kbit/s, Baseline, which was the grain), and its moov now carries a real avcC.

## Switching it off

Set `WEYLUS_DDA=0` in Weylus's environment and restart Weylus. No rebuild. For example, in
PowerShell: `setx WEYLUS_DDA 0`, then close and start Weylus again. `false`, `off` and `no`
also work. Remove the variable (or set it to `1`) to turn it back on.

## How to test

Run Weylus with its log visible, connect the tablet, pick the 4K monitor.

1. When the client starts, the log says which path it took:
   `Video path: dda_qsv (ddagrab + h264_qsv)` (or `captrs`, with the reason in the fields).
2. When the encoder opens:
   `Video: dda+qsv output=0 3840x2160->2304x1296@h264_qsv fps=60 ddagrab_rate=120 ... rc=icq global_quality=23 ...`
3. Every 5 s while frames flow:
   `Video stats: fps=... capture_ms=... encode_ms=... timeouts=... errors=... size=3840x2160->2304x1296 path=dda_qsv backlog_max=... skipped_ticks=... max_frame_kb=...`
   - `fps` should sit near the client's frame rate (the goal: 55+ at 60).
   - `backlog_max` is the most video messages that were waiting for the tablet at a tick, and
     `skipped_ticks` the ticks not captured because two were already waiting (see
     Backpressure). On a good link both stay at 0-1 and 0.
   - `max_frame_kb` is the largest frame sent in those 5 s (keyframes and 4K scrolls).
   - `capture_ms` is the time to get a frame out of the graph (duplication, conversion,
     scaling). On a still screen it includes up to a quarter frame of waiting for a new one.
   - `encode_ms` is h264_qsv plus muxing.
4. Cores: in PowerShell, `$a=(Get-Process weylus).CPU; Start-Sleep 10; ((Get-Process weylus).CPU-$a)/10`
   prints the cores Weylus used over those 10 s. The goal is well under one core (0.34 was
   measured for ffmpeg alone). Compare with a run under `WEYLUS_DDA=0`.

## Backpressure

The server makes a frame only when the tablet has room for it. While two or more video
messages are still waiting to be written to the tablet, a tick is skipped rather than
encoded: the stream stays valid (the next frame references the last one sent), and the
tablet gets the newest picture as soon as it catches up instead of a growing queue of old
ones. The video thread never waits on a full queue.

If the queue stays full for more than 3 s the tablet is taken as gone (a dead link can keep
a session open for minutes): the log says `Client took no video for ... s; ending its
session`, the encoder and its desktop duplication are dropped, and the connection is
closed, so the tablet's reconnect gets the monitor.

## Known limits

- The lock screen, a UAC prompt, Ctrl+Alt+Del, or a resolution, scaling or rotation change
  ends the desktop duplication. The path is then rebuilt, retrying every 250 ms for up to
  4 s. If it still cannot start (the lock screen usually lasts longer), it falls back to
  captrs for the rest of that connection; reconnect the tablet or pick the monitor again to
  get back on the GPU path. The log names each step (`rebuilding it`, `falling back to captrs`).
- Intel graphics only (Quick Sync), and only the monitors on adapter 0. On a PC without it
  the path fails to start, retries for 4 s at each connect, then uses captrs; set
  `WEYLUS_DDA=0` there to skip the wait.
- Windows allows one desktop duplication per monitor per process, so two tablets on the
  same monitor at once do not work (captrs had the same limit).
- The mouse pointer is drawn when the client asks for "capture cursor" (captrs never drew it).
- Rate control is ICQ (constant quality) at 23, Harley's pick in a blind clip test (about
  5 Mbit/s on the test clip). It has no peak-rate cap: FFmpeg 8.0's h264_qsv picks ICQ only
  when no max rate is set, so a busy screen can briefly go above 5 Mbit/s.
- The build uses FFmpeg 8.0; the 56-59 fps measurement was with gyan's FFmpeg 9.0.1. The
  numbers with this build are the Windows PC's to measure. A rotated monitor is untested.
