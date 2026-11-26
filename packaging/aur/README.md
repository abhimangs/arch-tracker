# AUR packages

Two packages, ready for the [AUR](https://aur.archlinux.org):

- `arch-tracker` builds the release's source with cargo.
- `arch-tracker-bin` installs the release's prebuilt binary.

Both install the same files, through `packaging/install-files.sh`, like the local
`packaging/PKGBUILD`. CI builds both from every commit (`scripts/aur-check.sh`) and fails on a
stale `.SRCINFO`, a namcap error or a difference in their files.

## After a release

```sh
scripts/aur-bump.sh 0.2.0     # version, checksums of the release's files, .SRCINFO
scripts/aur-check.sh          # optional: build both here
git commit -am "aur: 0.2.0"
```

## Publishing (once you have an AUR account)

For each package, clone its AUR repository, copy the three files in, and push:

```sh
git clone ssh://aur@aur.archlinux.org/arch-tracker.git aur-arch-tracker
cp packaging/aur/arch-tracker/{PKGBUILD,.SRCINFO,arch-tracker.install} aur-arch-tracker/
cd aur-arch-tracker && git add -A && git commit -m "arch-tracker 0.2.0" && git push
```

The same with `arch-tracker-bin`.
