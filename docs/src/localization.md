# Locale, timezone, and keyboard

There are two ways to set these, and both are supported on purpose:

- **Before a build** — declare them in the layered config. They are resolved, recorded
  in the image's provenance, and baked in. Nothing asks a question at boot.
- **On a running image** — `dpkg-reconfigure` the relevant package, exactly as on any
  Debian system, **with no network**. This works because the image already ships
  `locales`, `keyboard-configuration`, and `console-setup`, and because the locales are
  already compiled onto the disk.

The second is the reason the first is not enough. A pre-built image is something you
hand to someone else; they should not have to rebuild it — or get it onto a network —
to type on a German keyboard.

## The knobs

| field | layer | default | what it sets |
|---|---|---|---|
| `locale` | `base.toml` | `C.UTF-8` | `LANG` in `/etc/locale.conf` |
| `locales_generate` | `base.toml` | `["en_US.UTF-8"]` | extra locales compiled into the image |
| `timezone` | `base.toml` | `UTC` | the `/etc/localtime` symlink |
| `keymap` | `devices/<board>.toml` | none | `/etc/default/keyboard` (the XKB variables) |

Each is overridable in a recipe, and on the command line with `--locale`,
`--locale-gen` (repeatable), `--timezone`, and `--keymap`:

```sh
cargo run -p boot2deb-cli -- build asus-c201-forky \
    --locale de_DE.UTF-8 --timezone Europe/Berlin --keymap de
```

`resolve` shows what a build will bake in:

```
locale       : C.UTF-8 (generated: C.UTF-8, en_US.UTF-8)
timezone     : UTC
keymap       : us [pc105]
```

### Why the locale and the keymap live on different layers

The locale and the timezone are **distro policy**: no board has an opinion about them,
so they sit in `base.toml`.

A keymap is different — whether a console keymap configures anything at all is a
property of the hardware. The C201 and the C100P are laptops with keyboards under the
user's hands and a US layout; the Turing RK1 and the H96 are headless, and a layout
declared for a console nobody types at is a claim the config cannot back. So `keymap`
sits on the **device**, and a headless board simply omits it: boot2deb then writes no
`/etc/default/keyboard` and Debian's own default (`pc105` / `us`) stands.

The Chromebit CS10 shows what the field is really asking. It has no keyboard at all, and
it declares `keymap = "us"` anyway — because it is not headless: it drives an HDMI
console, and a USB keyboard is the only way to type at it. The question is "does a
console layout configure anything here?", not "does the board ship keys". It does, so it
answers.

You can still pass `--keymap` to a headless board. `console-setup` ships on every
image, so a keymap is always *actionable* — plugging a USB keyboard into the RK1's HDMI
console is a real thing to do. A headless board just has no reason to *default* one.

### Why the default locale is `C.UTF-8` and not `en_US.UTF-8`

`C.UTF-8` is a complete UTF-8 locale built into glibc. It is also neutral: this project
targets no one country, and a US locale is not a better default than any other.

`en_US.UTF-8` is nevertheless **generated** into every image, and that is not a
contradiction — see the next section.

## The `Setting locale failed` warning

SSH into a fresh board and you may see:

```
perl: warning: Setting locale failed.
perl: warning: Please check that your locale settings:
	LANGUAGE = (unset),
	LC_ALL = (unset),
	LANG = "en_US.UTF-8"
    are supported and installed on your system.
```

**Nothing on the image is broken.** Debian's stock `openssh-server` ships
`AcceptEnv LANG LC_*`, so **your client forwards its own `LANG`** into the session. If
that locale was never generated on the target, `setlocale()` fails and every Perl-based
tool says so.

That is why `en_US.UTF-8` is in `locales_generate`: it makes the common client's
forwarded locale resolve. It costs about 3 MiB.

It is *not* a general fix — a `de_DE` client forwarding `de_DE.UTF-8` still warns, and
chasing that by pre-generating more locales is whack-a-mole. The actual fix is that the
locale is **changeable**, which is what the rest of this page is about. To silence it
for one session without changing anything:

```sh
LANG=C.UTF-8 ssh board
```

Do **not** "fix" this by removing `AcceptEnv LANG LC_*` from `sshd_config`. It is
standard Debian behaviour, and silently dropping it surprises anyone who relies on it.

## Changing them on a running image, offline

All three are the ordinary Debian commands. None of them needs the network, because the
packages and the locale data are already on the disk.

**Locale.** `dpkg-reconfigure` is the authoritative path — it generates the locale *and*
sets the default:

```sh
sudo dpkg-reconfigure locales     # tick the locales to generate, then pick the default
```

`localectl` also works on a boot2deb image, and it is worth knowing why: Debian builds
`systemd-localed` with `locale-gen` support, so `localectl set-locale` will add the
locale to `/etc/locale.gen` and run `locale-gen` itself — **but only if
`/usr/sbin/locale-gen` exists**, i.e. only if the `locales` package is installed. On an
image without it, `localectl` would set a `LANG` naming a locale that was never
generated. boot2deb ships `locales`, so:

```sh
sudo localectl set-locale LANG=de_DE.UTF-8
```

is safe here. Reconnect for it to take effect on your session.

**Timezone.** Either command works; both write the `/etc/localtime` symlink, which is
the only thing that reads as the system timezone (forky's `tzdata` no longer keeps an
`/etc/timezone` file at all):

```sh
sudo timedatectl set-timezone America/New_York
sudo dpkg-reconfigure tzdata      # the menu-driven equivalent
```

**Console keymap.**

```sh
sudo dpkg-reconfigure keyboard-configuration   # then: sudo setupcon
```

`setupcon` applies the new layout to the current console without a reboot.

### Why `dpkg-reconfigure` opens on the right values

boot2deb writes `/etc/locale.gen`, `/etc/locale.conf`, `/etc/default/keyboard`, and the
`/etc/localtime` symlink **before** the packages that own them are configured, not
after. Debian's `locales`, `keyboard-configuration`, and `tzdata` each seed their
debconf answers from those exact files when they install, so the shipped files, the
debconf database, and the `console-setup` cached keymap all agree.

The practical consequence: `dpkg-reconfigure locales` on the running board opens with
*your* locales already ticked and *your* default already selected — not Debian's. Had
the files been written after the packages, they would still be correct on disk, and
debconf would still be holding Debian's defaults underneath them.

## Notes for the curious

- **`/etc/locale.conf`, not `/etc/default/locale`.** Debian makes the latter a symlink
  to the former, and `systemd-tmpfiles` re-asserts that link with a *forcing* rule
  (`L+`) — so a regular file written at `/etc/default/locale` is deleted and replaced by
  the symlink on the next boot. Writing the symlink's target satisfies every reader:
  `pam_env` through the link, `systemd`/`localectl` directly, and the `locales` package,
  whose config script reads that path to learn the current default.
- **The system locale is always generated**, even `C.UTF-8`, which glibc would provide
  ungenerated. The `locales` package builds the choice list that `dpkg-reconfigure
  locales` offers for the *default locale* out of `/etc/locale.gen` — so a system locale
  missing from that file is one the user cannot see or re-select on the board.
- **Not `locales-all`.** It carries every locale Debian has, at hundreds of MiB. The
  three packages boot2deb ships cost about 44 MiB installed (measured on forky/arm64),
  plus ~3 MiB for the generated locale archive — call it ~47 MiB on the image. On a 2 GiB
  rootfs that is a little over 2%, and it compresses well into the shipped `.img.xz`.
