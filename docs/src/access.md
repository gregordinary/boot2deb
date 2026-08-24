# The account, sudo, and SSH keys

Every image carries one account — `debian` — and three settings decide who can use it
and what it costs them to become root. All three are config, resolved before the build
and recorded in the image's provenance.

## The knobs

| field | layer | default | what it sets |
|---|---|---|---|
| `sudo` | `base.toml` | `nopasswd` | `/etc/sudoers.d/debian` — whether `sudo` prompts |
| `first_boot_password_length` | `base.toml` | `12` | length of the generated per-image password |
| `ssh_authorized_keys` | `base.toml` | none | `~debian/.ssh/authorized_keys` |

Each is overridable in a recipe. `resolve` and `doctor` additionally take `--sudo` and
`--password-length`, so you can see what a choice resolves to before writing it down:

```sh
boot2deb resolve turing-rk1/forky --sudo password --password-length 16
```

`build` takes none of them: an image's access rules come from the config its lock was
resolved against, so changing them means changing `base.toml` or the recipe. `resolve`
prints the recipe to write, with the keys already filled in.

`ssh_authorized_keys` has no flag at all, deliberately — see below.

## The password

Each built image gets its own randomly generated password for `debian`, printed on the
build's last lines and stored in the `.provenance.toml` beside the image. It is
**expired**, so the first login has to replace it.

```
first-boot pw : 7kQmR3xLpAvB  (user debian, expired — change at first login)
```

Three facts about the base image decide how much that password is guarding:
`openssh-server` is installed and enabled, a DHCP client brings the board onto the
network before anyone has logged in, and `sudo` defaults to `nopasswd`. So the printed
password is root, on whatever network the board is plugged into, from the moment it
powers on.

**Expiry does not change that.** A login against an expired account is permitted and is
then required to *set* the new password. That protects against a credential nobody ever
rotates; it does nothing against someone reaching the board before its owner does —
whoever logs in first chooses the new password and keeps the account. Length is what
covers that window, which is why it is a validated range rather than a preference.

### Choosing a length

The alphabet is 56 symbols — mixed case and digits, with `0`/`O`/`o` and `1`/`l`/`I`
removed so the value transcribes cleanly at a console — so each character is about 5.8
bits. The accepted range is **8 to 64**, and the default is 12.

There are two different attacks, and they have very different reach:

- **Guessing at the login.** Bounded by what an `sshd` on one of these boards will
  service — tens of attempts per second, not thousands. Even 8 characters (~46 bits) is
  far out of reach here. This is why 8 is the floor rather than a recommendation.
- **Attacking the hash offline.** `/etc/shadow` travels *inside the image file*, and the
  password appears in the `.provenance.toml` beside it. Anyone holding a copy of either
  can attack the hash with no rate limit and no board involved. 8 characters is merely
  expensive against that; 12 (~70 bits) is out of reach.

So the length that matters is the one for an image you might copy, publish, or hand to
someone else — and 12 is chosen for that case. Shortening to 8 suits an image one
operator flashes and boots directly, where the hash never leaves the build host.

**Before shortening it:** the friction here is typing a password at a console, and an
authorized key removes that friction entirely without giving anything up. Reach for a key
first.

## Authorizing an SSH key

List the public key — the `.pub` file's contents — in `ssh_authorized_keys`:

```toml
ssh_authorized_keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9... operator@workstation",
]
```

With a key in place you never type the generated password: `ssh debian@<board>` works on
the first boot, and the password stays as the console fallback for the day the key is on
a laptop you do not have.

The entries are **key material, not paths**. A path would resolve differently on another
machine, and the point of writing a key down is that every build of that recipe carries
it. Public keys are not secret, so they belong in config alongside everything else that
describes the image.

Three places to put them, depending on scope:

- **A recipe** — this build point authorizes these keys.
- **A `base.toml` overlay** — your keys on every image you build, without editing the
  shipped tree. See [Overlays](reference/overlays.md).
- **The shipped `base.toml`** — only if the config root is your own fork.

A recipe's list **replaces** the base list rather than adding to it, so a recipe for an
image you intend to hand to someone else can authorize nobody with
`ssh_authorized_keys = []`.

### What is checked, and what is not

Each entry is validated at `resolve`, because `sshd` reports a line it cannot parse only
in its own log — on a board that may have no console. The failure to avoid is an image
whose key silently does not work.

An entry must be one line holding a key type, a base64 blob, and an optional comment. The
blob is decoded far enough to confirm its *own* embedded type name agrees with the
declared one, which catches the common paste accidents: a key wrapped by a mail client, a
truncated copy, or a blob under the wrong type name.

Two rejections are worth knowing about:

- **Private key material is refused by name.** A `-----BEGIN OPENSSH PRIVATE KEY-----`
  block here means `id_ed25519` was reached for instead of `id_ed25519.pub`, and the
  consequence would be a private key baked into every copy of the image. Authorize the
  `.pub`.
- **Options prefixes** (`restrict`, `command="…"`, `from="…"`) are refused. `sshd`
  accepts them, but their syntax is quoted and comma-separated, and a builder that half
  understood it would write a weaker restriction than the author wrote. Write a bare key;
  add options on the running board if you need them.

`ssh-dss` is not an accepted type: OpenSSH removed DSA support, so such a line could
never authenticate anything.

The file is written with mode `0600` inside a `0700` `~/.ssh`, both owned by `debian`,
which is what `sshd`'s default `StrictModes` requires before it will read a key at all.

## Choosing a sudo policy

`sudo = "nopasswd"` gives `debian` root with no prompt; `sudo = "password"` prompts for
the account's own password.

`nopasswd` is the default because these are single-operator boards: the account's
password was just set at first login, and re-typing it to reach root adds nothing that
the login did not already decide. It is also what makes an unattended first-boot setup
script work without embedding a password in it.

Choose `password` for a board that is shared between people, that runs anything reachable
from beyond a trusted network, or whose console someone else can walk up to. The
tradeoff is narrow but real: under `nopasswd`, anything that can log in is root, so the
password and the keys above are the *whole* boundary.

It is also one line to change on a running board, so this is a default rather than a
commitment:

```sh
sudo sh -c 'echo "debian ALL=(ALL) ALL" > /etc/sudoers.d/debian'
```

Note that `passwd root` does **not** change it. The rule belongs to `debian`, not to
root — root ships locked, and Debian's convention is to leave it that way and reach root
through `sudo`.

## What the provenance records

The `.provenance.toml` beside each image carries the full access picture in
`[credentials]` — the generated password, the sudo policy, and every authorized key:

```toml
[credentials]
user = "debian"
password = "7kQmR3xLpAvB"
note = "expired at first login (passwd -e); unique per built image"
sudo = "nopasswd"
authorized_keys = ["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9... operator@workstation"]
```

That file holds a live credential — treat it as sensitive. The image's own
`/etc/boot2deb/image.toml` deliberately carries none of this: it is readable by anyone
holding the disk, and an image that inventoried its own access rules would hand a reader
the list of what to go after.

Because the password is fresh per build, it is also the one thing that puts a built
image's `/etc/shadow` outside the byte-reproducibility claim. The keys and the sudo
policy are ordinary resolved config, so they are part of what a rebuild reproduces — and
part of the rootfs cache key, so adding a key or tightening `sudo` rebuilds the rootfs
rather than reusing the tree that had the old rules.
