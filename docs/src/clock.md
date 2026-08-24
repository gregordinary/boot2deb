# The clock and time sync

None of the boards here has a battery-backed real-time clock. That is the hardware, not
a gap in the config, and it decides how every image handles time: the clock is wrong for
the first few seconds of every boot, and everything that cares about a date has to wait
for it.

This page is about *when* the clock is right. For the timezone it is rendered in, see
[Locale, timezone, and keyboard](localization.md).

## What a board with no RTC does at boot

The kernel starts at the epoch. `systemd` then advances the clock to the later of its
own compiled-in epoch and the mtime of `/var/lib/systemd/timesync/clock`, a file
`systemd-timesyncd` rewrites periodically while it runs. So a board comes up at
*roughly whenever it was last powered on* — a plausible time, and as stale as the gap
since then. Seconds later, once the network is up, `timesyncd` reaches an NTP server and
steps the clock to the real one.

The window between those two events is short and entirely real. Run `apt update` inside
it and you get:

```
E: Release file for http://deb.debian.org/debian/dists/forky/InRelease is not valid yet
   (invalid for another 2h 51min 12s). Updates for this repository will not be applied.
```

or a TLS certificate that is "not yet valid", or `gpg` reporting a signature made in the
future. Nothing is broken. The clock is simply behind, and every one of those checks is
a date comparison against it. Wait a few seconds and run it again.

## What the image does about it

Debian already orders its maintenance jobs behind `time-sync.target` — `apt-daily`,
`apt-daily-upgrade`, `logrotate`, `man-db`, `fstrim`, `e2scrub_all`, `dpkg-db-backup`,
and `anacron` all declare `After=time-sync.target`. On a stock system that ordering is
inert, because nothing *reaches* that target unless `systemd-time-wait-sync` is enabled.
Every boot2deb image enables it, so the ordering means what it says: those jobs do not
run until the clock is trustworthy.

The image also **bounds the wait at 45 seconds**, which matters more than enabling it.
The stock unit ships `TimeoutStartSec=infinity`, and Debian's `anacron.service` is both
`After=time-sync.target` and `Before=multi-user.target` — so with no reachable time
source, the unmodified unit leaves `multi-user.target`, `graphical.target`, and
`timers.target` permanently inactive, and `systemctl is-system-running` stuck at
`starting` forever. Logins still work, which is what makes it a bad failure: `sshd`,
`getty`, and a display manager are all pulled in by their targets rather than ordered
behind them, so the board looks fine while no timer ever fires again.

With the bound, a board that cannot find a time source gives up and finishes booting.
Measured on the H96 MAX M9: 13.7 s to `graphical.target` with NTP reachable, 50.9 s with
it unreachable, and no failed units either way.

The drop-in that does this ships in the base overlay at
`base/overlay/etc/systemd/system/systemd-time-wait-sync.service.d/bounded.conf`.

## Choosing a time server

| field | layer | default | what it sets |
|---|---|---|---|
| `ntp_servers` | `base.toml` | empty | `NTP=` in `/etc/systemd/timesyncd.conf.d/10-boot2deb.conf` |

Empty is the default and writes no configuration at all. Debian compiles its own pool
into `systemd` as `FallbackNTP`, which is correct on any network with a route out and
assumes nothing about where a board is plugged in. `timedatectl show-timesync` on a
stock image shows exactly that:

```
FallbackNTPServers: 0.debian.pool.ntp.org 1.debian.pool.ntp.org ...
ServerName:         2.debian.pool.ntp.org
```

Naming servers sets `NTP=`, which `timesyncd` prefers, and **leaves the fallback pool
alone**. A configured server is therefore a preference rather than a commitment: an
image built for one network still finds the time on another. That is deliberate, and it
is why this never writes `FallbackNTP=`.

Two reasons to set it:

- **An isolated network**, where the public pool cannot be reached at all. Without a
  server it can reach, a board never syncs — it boots, waits its 45 seconds, and carries
  on with a stale clock.
- **A LAN time source on any network.** It answers in about a millisecond instead of an
  internet round trip, which shortens the window this page opens with.

In `base.toml`, for every image:

```toml
ntp_servers = ["ntp.lan", "192.168.1.1"]
```

or in a recipe, for one build point only. A recipe's list *replaces* the base list
rather than adding to it, so `ntp_servers = []` in a recipe is how one point opts back
out to the Debian pool.

`resolve` and `doctor` take `--ntp-server` (repeatable) so you can see what a choice
resolves to before writing it down:

```sh
boot2deb resolve h96-max-m9/forky --ntp-server ntp.lan
```

```
timezone     : UTC
ntp servers  : ntp.lan
```

An unconfigured image prints `(Debian fallback pool)` there rather than an empty line —
"no servers configured" and "no time source" are different things, and only the first is
true.

Like the localization flags, `build` takes none of these: an image's time config comes
from the config its lock was resolved against, not from a flag at build time.

## What resolution checks

Each entry must be a **bare host** — a hostname or an IP address, with no scheme, no
port, and no whitespace. `timesyncd` parses `NTP=` by splitting on spaces, so an entry
with a space in it silently becomes two servers, and one with a scheme or a port becomes
a server the resolver can never answer. All of these are rejected at resolve time rather
than at boot:

```
ntp://ntp.example.org      a scheme
ntp.example.org:123        a port (timesyncd always uses 123)
[fd00::1]                  the bracketed form is for URLs
ntp.a ntp.b                whitespace: two servers in one entry
```

IPv6 goes in unbracketed: `fd00::1`.

## Changing it on a running image

The ordinary systemd path, no rebuild needed:

```sh
sudo timedatectl show-timesync        # what it is asking now
sudoedit /etc/systemd/timesyncd.conf.d/10-boot2deb.conf
sudo systemctl restart systemd-timesyncd
```

To force a resync immediately:

```sh
sudo systemctl restart systemd-timesyncd
timedatectl                            # "System clock synchronized: yes"
```

To set the clock by hand on a board that has no time source at all, turn NTP off first —
`timesyncd` will otherwise overwrite you the moment it succeeds:

```sh
sudo timedatectl set-ntp false
sudo timedatectl set-time '2026-08-06 12:00:00'
```

That does not persist across a power cycle. Nothing does, without an RTC.

## Notes for the curious

- **A drop-in, not an edit.** `/etc/systemd/timesyncd.conf` is a `systemd` conffile.
  Rewriting it would make every future `systemd` upgrade prompt about a modified
  conffile on a running board, so the config goes in `timesyncd.conf.d/` instead.
- **`-` before `ExecStart`.** The bounded wait runs `timeout 45 …` with a leading `-`,
  which tells `systemd` to ignore the exit status. Without it, a timed-out oneshot
  counts as a failed unit and `systemctl is-system-running` reports `degraded` on every
  offline boot. The boot is released either way — ordering is satisfied when a job
  *finishes*, not when it succeeds, and `time-sync.target` only `Wants` the service.
- **DHCP does not help here.** Option 42 is the standard way for a network to advertise
  a time server, but `NetworkManager` — which manages the interface on these images —
  has no path to feed it to `timesyncd`, and most home routers do not advertise it
  anyway. Hence a config key rather than "let the network say".
- **Enabled after the packages, not before.** The `.wants` symlink is written by the
  build's customize step rather than staged in the base overlay, because the unit ships
  inside the `systemd` package: a symlink laid down before that package installs is one
  `deb-systemd-helper` may still have an opinion about when it applies the unit's
  preset.
