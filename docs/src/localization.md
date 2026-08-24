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
| `locales_generate` | `base.toml` | 17 widely-spoken locales | extra locales compiled into the image |
| `timezone` | `base.toml` | `UTC` | the `/etc/localtime` symlink |
| `keymap` | `devices/<board>.toml` | none | `/etc/default/keyboard` (the XKB variables) |

Each is overridable in a recipe. `resolve` and `doctor` additionally take `--locale`,
`--locale-gen` (repeatable), `--timezone`, and `--keymap`, so you can see what a
different choice resolves to before committing it to config:

```sh
cargo run -p boot2deb-cli -- resolve asus-c201/forky \
    --locale de_DE.UTF-8 --timezone Europe/Berlin --keymap de
```

`build` takes none of them, and that is the design: an image's localization comes from
the config its lock was resolved against, so changing what an image ships means changing
`base.toml` or the recipe — not a flag at build time.

`resolve` shows what a build will bake in:

```
locale       : C.UTF-8 (generated: C.UTF-8, en_US.UTF-8, en_GB.UTF-8, de_DE.UTF-8, ...)
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

### Which languages ship

Every image carries these compiled, in addition to the system locale:

`en_US` `en_GB` `de_DE` `fr_FR` `es_ES` `it_IT` `nl_NL` `pt_BR` `pl_PL` `uk_UA` `ru_RU`
`vi_VN` `ja_JP` `ko_KR` `zh_CN` `zh_HK` `zh_TW` — all `.UTF-8`.

It is a set of widely-spoken languages, not a complete one, and it is deliberately not
just English. Two reasons it can afford to be this wide:

- **glibc's locale archive shares data aggressively.** Measured on forky/arm64,
  `/usr/lib/locale/locale-archive` is 2.9 MiB with `C.UTF-8` + `en_US.UTF-8` alone, and
  19.2 MiB with the full set — about 1 MiB per added language, not the several MiB a
  standalone locale suggests.
- **A locale can only be compiled at build time.** `locale-gen` runs during the image
  build; no package a user installs later will generate one for them. So a first-run
  desktop wizard offers exactly the languages the *image* chose, and a graphical
  installer that lists one language is showing the truth about the image, not a bug in
  the desktop.

Anything outside the set is still one `dpkg-reconfigure locales` away with no network —
see [Adding a language](#adding-a-language).

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

That is one of the reasons `en_US.UTF-8` leads `locales_generate`: it makes the common
client's forwarded locale resolve.

The shipped set covers most clients, but it is not a general fix — a client forwarding a
locale outside it still warns, and chasing every locale by pre-generating it is
whack-a-mole. The actual fix is that the locale is **changeable**, which is what the rest
of this page is about. To silence it for one session without changing anything:

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

## Adding a language

Two places to do it, depending on whether you want it on *this* board or on every image
you build.

**On a running board**, no network needed:

```sh
sudo dpkg-reconfigure locales        # tick the extra languages, keep or change the default
```

Tick as many as you like and leave the default alone if you only want the language
*available* — a desktop's language picker reads the generated set, not the default. The
new locales are compiled on the spot.

**In an image**, so every board built from it ships the language: edit
`locales_generate` in `base.toml` (all images) or in the recipe (that build point only).
There is no build-time flag for it, deliberately — see [The knobs](#the-knobs). Each
entry is a full locale name **with its codeset** —
`sv_SE.UTF-8`, not `sv_SE`; resolution rejects the bare form rather than let
`locale-gen` fail mid-build. The system `locale` is always generated whether or not it
appears in the list, so it never needs repeating.

Valid names come from `/usr/share/i18n/SUPPORTED` on any Debian system — it lists
`language_TERRITORY.codeset` pairs, not language names, so search by the two-letter
language code and take the `.UTF-8` line:

```sh
grep '^sv_.*UTF-8' /usr/share/i18n/SUPPORTED    # sv_FI.UTF-8, sv_SE.UTF-8
```

Budget roughly 1 MiB of image per added language, and check the result with
`resolve` before building:

```sh
cargo run -p boot2deb-cli -- resolve h96-max-m9/forky | grep locale
```

There is no `locales-all` option here, and that is on purpose: it carries every locale
Debian has for 231 MiB installed.

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
- **Not `locales-all`.** It carries every locale Debian has, at 231 MiB installed. The
  three packages boot2deb ships cost about 44 MiB installed (measured on forky/arm64),
  plus 19.2 MiB for the generated locale archive — call it ~63 MiB on the image. On a
  2 GiB rootfs that is about 3%, and it compresses well into the shipped `.img.xz`.
