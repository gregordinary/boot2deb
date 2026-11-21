#!/bin/sh
set -eu
# Exercise the seed first-boot hook against stubbed system tools and a scratch root.
#
# The property under test is that a seed.txt nobody validated cannot leave the board
# worse off than no seed at all. Every value the hook applies passes the checks
# `boot2deb press` makes. A value that fails is skipped without taking the other keys
# with it. A static address the network stack refuses leaves the image's DHCP
# configuration in effect, and the log does not claim the address. The cases assert
# on the files the hook leaves in the scratch root, and on the log where the log is
# the claim.
#
# Runs anywhere with /bin/sh, iconv, base64 and od; touches nothing outside a temp
# directory. Run from the boot2deb root:  sh base/test-seed.sh

hook=base/overlay/etc/boot2deb/first-boot.d/10-seed
[ -r "$hook" ] || { echo "run me from the boot2deb root" >&2; exit 2; }

work="$(mktemp -d)"
trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT
stubs="$work/stubs"
root="$work/root"
calls="$work/calls"
mkdir -p "$stubs"

# A copy of the hook whose root is the scratch tree.
sed "s|^root=''$|root='$root'|" "$hook" > "$work/hook"
grep -q "^root='$root'$" "$work/hook" \
    || { echo "the hook's root= line moved; update the rewrite above" >&2; exit 2; }

# Stubs for the commands that would touch the real system. Each records its call in
# $calls and reads its scenario from STUB_* variables. The seed partition is found on
# root's disk and "mounted" by copying the case's seed.txt into the hook's mount point.
cat > "$stubs/findmnt" <<'S'
#!/bin/sh
echo /dev/mmcblk0p2
S
cat > "$stubs/lsblk" <<'S'
#!/bin/sh
case "$1" in
    -lnspo) printf '/dev/mmcblk0p2 part\n/dev/mmcblk0 disk\n' ;;
    -lnpo)  printf '/dev/mmcblk0 \n/dev/mmcblk0p1 b2d-seed\n/dev/mmcblk0p2 rootfs\n' ;;
    *)      exit 99 ;;
esac
S
cat > "$stubs/mount" <<'S'
#!/bin/sh
for last; do :; done
cp "$STUB_SEED" "$last/seed.txt"
S
cat > "$stubs/umount" <<'S'
#!/bin/sh
rm -f "$1/seed.txt"
S
cat > "$stubs/hostnamectl" <<'S'
#!/bin/sh
echo "hostnamectl $*" >> "$STUB_CALLS"
S
cat > "$stubs/getent" <<'S'
#!/bin/sh
echo "user:x:1000:1000::$STUB_ROOT/home/user:/bin/sh"
S
printf '#!/bin/sh\necho user\n' > "$stubs/id"
cat > "$stubs/install" <<'S'
#!/bin/sh
for last; do :; done
mkdir -p "$last"
S
printf '#!/bin/sh\nexit 0\n' > "$stubs/chown"
cat > "$stubs/nmcli" <<'S'
#!/bin/sh
echo "nmcli $*" >> "$STUB_CALLS"
S
# `generate` keeps a copy of what it was asked to check, and fails on request;
# `apply` fails on request only while the seeded file is installed, which is a stack
# refusing the profile rather than one that is broken outright.
cat > "$stubs/netplan" <<'S'
#!/bin/sh
echo "netplan $*" >> "$STUB_CALLS"
case "$1" in
    generate)
        if [ "${2:-}" = --root-dir ]; then
            cp "$3/etc/netplan/60-boot2deb-seed.yaml" "$STUB_STAGED" 2>/dev/null
            ls "$3/etc/netplan" > "$STUB_STAGED.list"
        fi
        if [ -n "$STUB_NETPLAN_REFUSES" ]; then echo "Error in network definition" >&2; exit 1; fi ;;
    apply)
        if [ -n "$STUB_APPLY_REFUSES" ] && [ -e "$STUB_ROOT/etc/netplan/60-boot2deb-seed.yaml" ]; then
            echo "systemd-networkd failed to start" >&2
            exit 1
        fi ;;
esac
S
cat > "$stubs/systemctl" <<'S'
#!/bin/sh
echo "systemctl $*" >> "$STUB_CALLS"
if [ -n "$STUB_DHCPCD_REFUSES" ] && grep -q '^# boot2deb seed static ip$' "$STUB_ROOT/etc/dhcpcd.conf"; then
    exit 1
fi
S
chmod +x "$stubs"/*

# A scratch root for one network stack: netplan (Ubuntu), nm (NetworkManager), or
# dhcpcd, with an /etc/hosts and the default account's home.
fresh() {
    chmod -R u+w "$root" 2>/dev/null || true
    rm -rf "$root"
    mkdir -p "$root/etc" "$root/home/user"
    printf '127.0.0.1\tlocalhost\n127.0.1.1\timage-host\n' > "$root/etc/hosts"
    case "$1" in
        netplan)
            mkdir -p "$root/etc/netplan"
            printf 'network:\n  version: 2\n' > "$root/etc/netplan/50-boot2deb.yaml" ;;
        nm)
            mkdir -p "$root/etc/NetworkManager/system-connections" ;;
        dhcpcd)
            printf 'hostname\n' > "$root/etc/dhcpcd.conf"
            mkdir -p "$root/sys/class/net/lo" "$root/sys/class/net/end0" ;;
    esac
}

fails=0
# run <stack> <seed.txt contents> [STUB_X=y ...]
#
# READONLY=<dir> is the harness's own: that directory of the scratch root is made
# unwritable before the hook runs, so a write into it fails.
run() {
    stack="$1"; text="$2"; shift 2
    fresh "$stack"
    case "${1:-}" in READONLY=*) chmod a-w "$root/${1#READONLY=}"; shift ;; esac
    printf '%s\n' "$text" > "$work/seed.txt"
    : > "$calls"
    rm -f "$work/staged.yaml" "$work/staged.yaml.list"
    rc=0
    (
        export STUB_SEED="$work/seed.txt" STUB_ROOT="$root" STUB_CALLS="$calls" \
            STUB_STAGED="$work/staged.yaml" STUB_NETPLAN_REFUSES='' STUB_APPLY_REFUSES='' \
            STUB_DHCPCD_REFUSES=''
        for kv; do export "$kv"; done
        PATH="$stubs:$PATH" sh "$work/hook" > "$work/out" 2>&1
    ) || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "FAIL  the hook exited $rc on: $text"
        sed 's/^/        /' "$work/out"
        fails=$((fails + 1))
    fi
}

# check <description> <command...>: the command is the assertion.
check() {
    what="$1"; shift
    if "$@"; then
        echo "ok    $what"
    else
        echo "FAIL  $what"
        sed 's/^/        log: /' "$work/out"
        fails=$((fails + 1))
    fi
}
has()        { [ -f "$root/$1" ]; }
absent()     { [ ! -e "$root/$1" ]; }
line_in()    { grep -qxF -e "$2" "$root/$1"; }
lines_in()   { [ "$(grep -c . "$root/$1")" -eq "$2" ]; }
logged()     { grep -qF -e "$1" "$work/out"; }
not_logged() { ! grep -qF -e "$1" "$work/out"; }
called()     { grep -qF -e "$1" "$calls"; }
not_called() { ! grep -qF -e "$1" "$calls"; }
mode_600()   { [ "$(ls -l "$root/$1" | cut -c1-10)" = "-rw-------" ]; }

tab="$(printf '\t')"
cr="$(printf '\r')"
seeded=etc/netplan/60-boot2deb-seed.yaml
w=etc/NetworkManager/system-connections/b2d-seed.nmconnection
p=etc/NetworkManager/system-connections/b2d-seed-wired.nmconnection
ed25519="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe operator@workstation"
ecdsa="ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBI/Ik3VDAC6UR9cDWp9/6U4c0XaG3BGtEOykmO5zPZ2gWhf/ScyxCsiXVfmnmLTq//7Oeq6tT5aLaWbYwqDlHes= ops@bench"
rsa="ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDCn/jNTTR3aZH6Ur0fDUQ/P+S270GeyIbiWo+ymUr7avzfq0yquYxvuQdmeY38eQCR2/0A4yH00nM+ieBhzGYsm9pM6cnF5zFyN9+zVYlPxUJloFOATI3DrcMJ72Qr/yJTVq2P1lGXb4VXTbV+CRjFOmry+/RqrlDPpyAbCm2P0Q== backup@bench"

echo "-- static_ip is held to the rules press checks"
# A refused value never reaches netplan, and the wired interfaces keep DHCP.
refuses_static() {
    run netplan "static_ip=$2"
    check "refuses $1" absent "$seeded"
    check "  ...without asking netplan" not_called "netplan"
    check "  ...and says so" logged "skipping it"
}
refuses_static "an octet above 255" "192.168.1.256/24"
refuses_static "an octet with a leading zero" "192.168.01.50/24"
refuses_static "prefix 0" "192.168.1.50/0"
refuses_static "prefix 33" "192.168.1.50/33"
refuses_static "a signed prefix" "192.168.1.50/+24"
refuses_static "an address with no prefix" "192.168.1.50"
refuses_static "a bad gateway" "192.168.1.50/24,192.168.1.300"
refuses_static "a bad DNS server" "192.168.1.50/24,192.168.1.1,1.1.1"
refuses_static "a trailing comma" "192.168.1.50/24,"
refuses_static "an empty gateway" "192.168.1.50/24,,1.1.1.1"
refuses_static "whitespace inside the value" "192.168.1.50 /24"
refuses_static "an IPv6 address" "fe80::1/64"
refuses_static "a three-octet address" "192.168.1/24"

run netplan "static_ip=192.168.1.50/24,192.168.1.1,1.1.1.1,9.9.9.9"
check "writes a valid value as a netplan file" has "$seeded"
check "  ...with the address" line_in "$seeded" '        - "192.168.1.50/24"'
check "  ...the gateway" line_in "$seeded" '          via: "192.168.1.1"'
check "  ...and both DNS servers" line_in "$seeded" '          - "9.9.9.9"'
check "  ...mode 600" mode_600 "$seeded"
check "  ...after netplan generated it in a staging root" called "netplan generate --root-dir"
check "  ...that also held the image's own definition" \
    grep -qx 50-boot2deb.yaml "$work/staged.yaml.list"
check "  ...and the staged file is the installed one" cmp -s "$work/staged.yaml" "$root/$seeded"
check "  ...then applies it" called "netplan apply"
check "  ...and says so" logged "pinned the wired interfaces to 192.168.1.50/24 via netplan"

run netplan "static_ip=10.0.0.9/024"
check "accepts a zero-padded prefix, as press does, and writes it plain" \
    line_in "$seeded" '        - "10.0.0.9/24"'

run netplan "static_ip= 10.0.0.9/24 $cr"
check "trims a hand-edited value's spaces and CRLF" line_in "$seeded" '        - "10.0.0.9/24"'

echo "-- a stack that refuses the profile leaves DHCP in effect"
run netplan "static_ip=192.168.1.50/24,192.168.1.1" STUB_NETPLAN_REFUSES=yes
check "a profile netplan generate refuses is never installed" absent "$seeded"
check "  ...nor applied" not_called "netplan apply"
check "  ...nor claimed" not_logged "pinned"
check "  ...and the log says DHCP stays" logged "stay on DHCP"
check "  ...with netplan's own words" logged "netplan: Error in network definition"

run netplan "static_ip=192.168.1.50/24,192.168.1.1" STUB_APPLY_REFUSES=yes
check "a profile netplan apply fails on is removed again" absent "$seeded"
check "  ...the image's definitions are applied again" \
    [ "$(grep -c '^netplan apply' "$calls")" -eq 2 ]
check "  ...the address is not claimed" not_logged "pinned"
check "  ...and the log says DHCP stays" logged "removed it, the wired interfaces stay on DHCP"
check "  ...and the image's definition is untouched" has etc/netplan/50-boot2deb.yaml

run dhcpcd "static_ip=192.168.1.50/24,192.168.1.1,1.1.1.1"
check "dhcpcd: writes the stanza for the wired interface" line_in etc/dhcpcd.conf "interface end0"
check "  ...with the address" line_in etc/dhcpcd.conf "static ip_address=192.168.1.50/24"
check "  ...restarts dhcpcd only if it runs" called "systemctl try-restart dhcpcd"
check "  ...leaves no working copies" absent etc/dhcpcd.conf.b2d-orig
check "  ...and says so" logged "pinned end0 to 192.168.1.50/24 via dhcpcd"

run dhcpcd "static_ip=192.168.1.50/24" STUB_DHCPCD_REFUSES=yes
check "dhcpcd: a stanza dhcpcd will not restart with is taken out" \
    not_logged "pinned"
check "  ...restoring the original file" line_in etc/dhcpcd.conf "hostname"
check "  ...exactly" lines_in etc/dhcpcd.conf 1

run nm "static_ip=192.168.1.50/24,192.168.1.1,1.1.1.1"
check "NetworkManager: pins the wired profile" line_in "$p" "address1=192.168.1.50/24,192.168.1.1"
check "  ...with DNS" line_in "$p" "dns=1.1.1.1;"
check "  ...mode 600" mode_600 "$p"
check "  ...leaving no temporary file" absent etc/NetworkManager/.b2d-seed-wired.nmconnection.tmp
check "  ...and nothing else among the connections" \
    [ "$(ls -A "$root/etc/NetworkManager/system-connections")" = b2d-seed-wired.nmconnection ]

# A write that fails is not reported as done. Permissions do not stop root, so these
# need an unprivileged run.
if [ "$(id -u)" -ne 0 ]; then
    run nm "wifi_ssid=lab
wifi_psk=hunter22" READONLY=etc/NetworkManager
    check "a Wi-Fi profile that cannot be written is not claimed" \
        not_logged "wrote a Wi-Fi profile"
    check "  ...and says so" logged "could not write the Wi-Fi profile"
    check "  ...leaving nothing behind" absent "$w"
    run dhcpcd "static_ip=192.168.1.50/24" READONLY=etc
    check "a dhcpcd stanza that cannot be written is not claimed" not_logged "pinned"
    check "  ...and says so" logged "could not write"
    check "  ...leaving dhcpcd.conf as it was" lines_in etc/dhcpcd.conf 1
else
    echo "skip  unwritable configuration directories (running as root)"
fi

echo "-- one bad key costs only itself"
run netplan "hostname=rk1-07
authorized_key=$ed25519
static_ip=192.168.1.256/24"
check "a bad static_ip does not stop the hostname" called "hostnamectl set-hostname rk1-07"
check "  ...or /etc/hosts" line_in etc/hosts "127.0.1.1${tab}rk1-07"
check "  ...or the SSH key" line_in home/user/.ssh/authorized_keys "$ed25519"
check "  ...and is itself not applied" absent "$seeded"

run netplan "hostname=rk1_07
static_ip=192.168.1.50/24"
check "a bad hostname is not set" not_called "hostnamectl"
check "  ...and /etc/hosts keeps the image's" line_in etc/hosts "127.0.1.1${tab}image-host"
check "  ...while the static_ip applies" has "$seeded"

# The lengths are core::hostname's own test vectors: 64 bytes in all, 63 per label.
a() { printf 'a%.0s' $(seq "$1"); }
for bad in "-rk1" "rk1-" "rk1..lan" ".rk1" "rk1." "rk1 lan" "rk1#x" "$(a 65)" "$(a 64)" \
    "$(a 64).b"; do
    run netplan "hostname=$bad"
    check "refuses hostname '$bad'" not_called "hostnamectl"
done
run netplan "hostname=$(a 63)"
check "accepts a 63-character label" called "hostnamectl set-hostname $(a 63)"
run netplan "hostname=$(a 31).$(printf 'b%.0s' $(seq 32))"
check "accepts 64 characters in two labels" called "hostnamectl set-hostname $(a 31)."
run netplan "hostname=b0ard-1.example.com"
check "accepts a dotted name" called "hostnamectl set-hostname b0ard-1.example.com"

run netplan "authorized_key=$ed25519
authorized_key=-----BEGIN OPENSSH PRIVATE KEY----- b3BlbnNzaA==
authorized_key=restrict $ed25519
authorized_key=ssh-ed25519 AAAAB3NzaC1yc2EA
authorized_key=ssh-ed25519 AAAAC3NzaC1lZDI1
authorized_key=ssh-ed25519 not-base64!!
authorized_key=ssh-ed25519 AAAA=AAA
authorized_key=ssh-dss AAAAB3NzaC1kc3MAAACBAaaa
authorized_key=command=\"/bin/true\"
authorized_key=ssh-ed25519  AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe
authorized_key=$(printf 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe a\033[2Jb')
authorized_key=$ecdsa
authorized_key=$rsa"
k=home/user/.ssh/authorized_keys
check "authorizes the valid ed25519 key" line_in "$k" "$ed25519"
check "  ...the valid ecdsa key" line_in "$k" "$ecdsa"
check "  ...the valid rsa key" line_in "$k" "$rsa"
check "  ...and none of the ten bad entries" lines_in "$k" 3
check "  ...naming each refusal" [ "$(grep -c 'authorized_key entry' "$work/out")" -eq 10 ]
check "  ...a pasted private key is named as one" logged "is private key material"
check "  ...and never echoed" not_logged "b3BlbnNzaA"
check "  ...a mismatched blob is named as one" logged "does not match its type name"
check "  ...an options prefix is named as one" logged "looks like an options prefix"

run netplan "authorized_key=${ed25519%% operator@workstation}"
check "accepts a key with no comment" line_in "$k" "${ed25519%% operator@workstation}"

echo "-- Wi-Fi is held to the rules press checks"
run nm "wifi_ssid=lab net
wifi_psk=hunter22
static_ip=192.168.1.50/24,192.168.1.1"
check "writes a Wi-Fi profile" line_in "$w" "ssid=lab net"
check "  ...with the passphrase" line_in "$w" "psk=hunter22"
check "  ...made static" line_in "$w" "address1=192.168.1.50/24,192.168.1.1"
check "  ...mode 600" mode_600 "$w"
check "  ...and no wired profile besides" absent "$p"

run nm 'wifi_ssid=a\b;c
wifi_psk=pa\ss;wo#rd1'
check "escapes a backslash in the connection id for GKeyFile" line_in "$w" 'id=a\\b;c'
check "  ...and a backslash and semicolon in the SSID, as NetworkManager does" \
    line_in "$w" 'ssid=a\\b\\;c'
check "  ...and a backslash in the passphrase" line_in "$w" 'psk=pa\\ss;wo#rd1'

run nm "wifi_ssid=lab
wifi_psk=$(printf 'a1%.0s' $(seq 32))"
check "accepts a 64-hex-digit raw key" line_in "$w" "psk=$(printf 'a1%.0s' $(seq 32))"

run nm "wifi_ssid=$(printf '\303\234berfunk')
wifi_psk=hunter22"
check "accepts a UTF-8 SSID" has "$w"

refuses_wifi() {
    run nm "$2
static_ip=192.168.1.50/24"
    check "refuses $1" absent "$w"
    check "  ...and keeps the static_ip off the wired interface" absent "$p"
}
refuses_wifi "a passphrase under 8 characters" "wifi_ssid=lab
wifi_psk=hunter2"
check "  ...without echoing it" not_logged "hunter2"
refuses_wifi "a passphrase over 63 characters" "wifi_ssid=lab
wifi_psk=$(printf 'x%.0s' $(seq 65))"
refuses_wifi "64 characters that are not hex" "wifi_ssid=lab
wifi_psk=$(printf 'x%.0s' $(seq 64))"
refuses_wifi "a non-ASCII passphrase" "wifi_ssid=lab
wifi_psk=$(printf 'pass\303\251word')"
refuses_wifi "an SSID over 32 bytes" "wifi_ssid=$(printf 'x%.0s' $(seq 33))"
refuses_wifi "an SSID with a control character" "wifi_ssid=$(printf 'lab\033net')"
refuses_wifi "an SSID with a C1 control character" "wifi_ssid=$(printf 'lab\302\205net')"
refuses_wifi "an SSID ending in a no-break space" "wifi_ssid=$(printf 'lab\302\240')"
refuses_wifi "an SSID that is not UTF-8" "wifi_ssid=$(printf 'lab\377')"

if [ "$fails" -eq 0 ]; then
    echo "all cases passed"
else
    echo "$fails check(s) failed" >&2
    exit 1
fi
