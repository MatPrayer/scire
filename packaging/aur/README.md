# AUR package

`PKGBUILD` and `.SRCINFO` here are the canonical copies. The AUR serves each
package as its **own** git repository, so publishing is a copy into that
repository rather than a push of this one.

It builds from source at the tag, not from the release tarball: that tarball is
built on Ubuntu and carries its glibc floor, which is the one thing an Arch
user has no use for, and building from source covers `aarch64` at the same
time.

## Releasing a new version

After the `vX.Y.Z` tag exists on GitHub:

```bash
# 1. Bump the version here and regenerate the metadata.
sed -i 's/^pkgver=.*/pkgver=X.Y.Z/; s/^pkgrel=.*/pkgrel=1/' packaging/aur/PKGBUILD
( cd packaging/aur && makepkg --printsrcinfo > .SRCINFO )

# 2. Build it once in a clean chroot before publishing anything. This is the
#    only check that the dependency list is complete — a missing depends()
#    entry is invisible on a machine that already has the library.
( cd packaging/aur && makepkg -si )

# 3. Copy both files into the AUR repository and push.
git clone ssh://aur@aur.archlinux.org/scire.git /tmp/aur-scire
cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO /tmp/aur-scire/
cd /tmp/aur-scire && git commit -am "scire X.Y.Z" && git push
```

`pkgrel` goes up instead of `pkgver` when the packaging changes but the
software does not — a corrected dependency, say.

First-time setup needs an AUR account with an SSH key registered, and the
package name reserved by pushing to `ssh://aur@aur.archlinux.org/scire.git`
(the repository is created by the first push).
