#!/bin/sh
# Date the kernel's generated Debian changelog from SOURCE_DATE_EPOCH.
#
# dpkg-buildpackage runs this as its init hook, in the kernel tree. The kernel's
# mkdebian has written debian/changelog by then, and debian/rules has not yet installed
# it into every package. mkdebian dates the entry from the wall clock and reads no
# variable, so without this each build of one lock ships its own changelog.Debian.gz.
set -eu
date=$(date -u -R -d "@$SOURCE_DATE_EPOCH")
sed -i "s/^\( -- .*\)  .*\$/\1  $date/" debian/changelog
# A trailer this did not rewrite is a changelog shape it does not know. Stop rather
# than ship the wall clock without a word.
grep -qx " -- .*  $date" debian/changelog
