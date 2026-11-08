# Releasing

Two distribution channels, with different effort: GitHub releases are
automatic, COPR needs a one-time setup and then a click (or a webhook) per
version.

## Cutting a release

1. Bump `version` in `Cargo.toml` and the `Version:` and `%changelog` in
   `packaging/quickshare-daemon.spec`. They must match: the spec's `Source0`
   is `.../archive/v%{version}/...`, so a mismatch fetches a tag that does not
   exist.
2. Commit, then tag and push:

   ```bash
   git tag v0.1.0 && git push origin v0.1.0
   ```

`.github/workflows/release.yml` then builds on ubuntu-22.04, prints the binary
size and the glibc floor into the log, and publishes three assets:

| Asset | Why |
| ----- | --- |
| `quickshare-daemon-<tag>-x86_64-linux.tar.gz` | binary, systemd unit, Nautilus extension, README, LICENSE |
| `...tar.gz.sha256` | `sha256sum -c` format, verified by the installer before unpacking |
| `install.sh` | what the README tells people to download |

The build runs on ubuntu-22.04 deliberately. A newer runner would raise the
glibc floor of the published binary for no benefit.

### Check it

```bash
curl -fsSL -o /tmp/install.sh \
  https://github.com/LXK-98/quickshare-daemon/releases/latest/download/install.sh
XDG_BIN_HOME=/tmp/qsd/bin XDG_DATA_HOME=/tmp/qsd/share sh /tmp/install.sh
```

Pointing the two XDG variables at a scratch directory exercises the real
script against the real release without touching an existing install. Re-run
it to check the update path reports `Already on <tag>.`, then
`sh /tmp/install.sh --uninstall`.

## COPR

### The problem to decide first: network access

COPR builds run in mock **without network access** — the default flipped to
off in June 2022. This package cannot build that way: `cargo build` fetches
276 crates from crates.io and three git dependencies (`rqs_lib` from our fork,
and `mdns-sd` and `sys_metrics` from Martichou).

So one of two things has to be true.

**Either enable network for the project** — one flag, nothing in the
repository changes:

```bash
copr-cli modify lxk-98/quickshare-daemon --enable-net on
```

**Or vendor the dependencies** so the build is genuinely offline. `cargo
vendor` handles the git sources correctly, so this works, but it is not free:

```bash
cargo vendor --locked vendor/
tar -czf vendor.tar.gz vendor/
```

That tarball is **48 MB compressed, 456 MB unpacked** — most of it code for
platforms this never builds for. It would ship as a `Source1`, with a
`.cargo/config.toml` written in `%prep` and `--offline` added to the build.

Try the flag first. Vendoring is the fallback if COPR declines, and it is also
what Fedora proper would require if this were ever submitted there.

The COPR owner is the **FAS** username (`lxk-98`), which is not the same
string as the GitHub owner (`LXK-98`). Everything above and in `install.sh`
uses the GitHub one; only `dnf copr enable` takes the FAS one.

### One-time setup

1. A [Fedora account](https://accounts.fedoraproject.org/) — the same FAS
   account used for Bugzilla and the wiki.

2. **Sign in at [copr.fedorainfracloud.org](https://copr.fedorainfracloud.org/)
   once, in a browser.** This is not optional and cannot be skipped with the
   CLI: COPR creates the user record on first web login, and until it exists
   every API call fails with

   > Valid GSSAPI authentication supplied for user 'lxk-98', but this user
   > doesn't exist in the Copr build system. Please log-in using the web-UI
   > (without GSSAPI) first.

   If that page returns **504 Gateway Timeout** after the Fedora login
   redirect, it is COPR's dashboard render timing out, not your account. The
   anonymous site stays healthy throughout, so the status page shows nothing.
   Retry — and check `copr-cli whoami` after each attempt, since the record
   may be created even when the page that follows never renders. If it keeps
   failing, report it on the
   [COPR tracker](https://github.com/fedora-copr/copr/issues) or in
   `#copr:fedoraproject.org` on Matrix.

3. Authenticate the CLI. Either Kerberos, which needs no token:

   ```bash
   sudo dnf install copr-cli fedora-packager
   fkinit -u lxk-98      # FAS password + OTP; ticket lasts ~28h
   copr-cli whoami
   ```

   or a token from <https://copr.fedorainfracloud.org/api/>, pasted into
   `~/.config/copr`. A token takes precedence over a Kerberos ticket when
   both are present. Either way, step 2 must have happened first.

   `copr-cli` reports a missing credential as *"Response is not in JSON
   format, there is probably a bug in the API code"*. That is an HTML `401`
   it failed to parse, not a COPR bug — run with `--debug` to see the real
   status.

4. Create the project:

   ```bash
   copr-cli create quickshare-daemon \
     --chroot fedora-43-x86_64 \
     --chroot fedora-44-x86_64 \
     --chroot fedora-45-x86_64 \
     --chroot fedora-rawhide-x86_64 \
     --enable-net on \
     --description "Headless Quick Share (Nearby Share) daemon for Linux" \
     --instructions "dnf copr enable lxk-98/quickshare-daemon && dnf install quickshare-daemon"
   ```

   x86_64 only, because that is the only architecture the release binary is
   built for and the only one this has been tested on. `copr-cli
   list-chroots` shows what is currently offered.

   Confirm `--enable-net` actually took, because the build fails late and
   confusingly without it:

   ```bash
   curl -s 'https://copr.fedorainfracloud.org/api_3/project?ownername=lxk-98&projectname=quickshare-daemon' |
     python3 -c 'import json,sys; print(json.load(sys.stdin)["enable_net"])'
   ```

   The project lives at
   <https://copr.fedorainfracloud.org/coprs/lxk-98/quickshare-daemon/>.

### Building a version

The straightforward way, from a tag that already exists:

```bash
copr-cli build lxk-98/quickshare-daemon \
  https://github.com/LXK-98/quickshare-daemon/releases/download/v0.1.0/quickshare-daemon-0.1.0-1.src.rpm
```

We do not publish an SRPM, so build one first:

```bash
spectool -g -R packaging/quickshare-daemon.spec    # fetches Source0 from the tag
rpmbuild -bs packaging/quickshare-daemon.spec
copr-cli build lxk-98/quickshare-daemon ~/rpmbuild/SRPMS/quickshare-daemon-0.1.0-1.*.src.rpm
```

### Rebuilding automatically on a tag

This is configured. The package is an SCM source with auto-rebuild on:

```bash
copr-cli edit-package-scm quickshare-daemon \
  --name quickshare-daemon \
  --clone-url https://github.com/LXK-98/quickshare-daemon.git \
  --commit main \
  --spec packaging/quickshare-daemon.spec \
  --type git \
  --method rpkg \
  --webhook-rebuild on
```

(`add-package-scm` fails once a package exists — an uploaded SRPM creates one
— so `edit-package-scm` converts it instead.)

The GitHub half has to be added by hand once, because the webhook URL embeds a
secret. From *Settings → Integrations* on the
[COPR project page](https://copr.fedorainfracloud.org/coprs/lxk-98/quickshare-daemon/),
copy the GitHub webhook URL, then either paste it into the repository's
*Settings → Webhooks → Add webhook*, or:

```bash
gh api repos/LXK-98/quickshare-daemon/hooks -f name=web \
  -f 'config[url]=PASTE_THE_COPR_URL' -f 'config[content_type]=json' \
  -F 'events[]=create' -F active=true
```

**Subscribe to “Branch or tag creation” only — not “push”.** COPR rebuilds the
*configured committish* (`main`), not the ref the event carries, so a `push`
subscription would rebuild the same version on every documentation commit.
The `create` event fires for new branches too, which at worst costs one
redundant rebuild.

#### What this means for the order of operations

Because COPR builds `main`'s spec rather than the tag's, **the `Version:` bump
must be on `main` before the tag is pushed.** Step 1 of “Cutting a release”
above already does this, but it stops being a convention and becomes a
requirement once the webhook is live:

1. Bump `Version:` and `%changelog`, commit, push to `main`.
2. Tag and push the tag.
3. The `create` event reaches COPR, which builds `main`'s spec — by now
   carrying the new version — and its `Source0` resolves to the tag that was
   just pushed.

Get the order wrong and COPR rebuilds the previous version, or fails fetching
a `Source0` for a tag that does not exist yet.

### Checking the spec before handing it to COPR

The SRPM builds without any setup:

```bash
spectool -g -R packaging/quickshare-daemon.spec
rpmbuild -bs packaging/quickshare-daemon.spec
```

A full rehearsal of what COPR will do needs mock, which needs group
membership and a re-login:

```bash
sudo usermod -aG mock "$USER"      # log out and back in
mock -r fedora-44-x86_64 --enable-network rebuild ~/rpmbuild/SRPMS/quickshare-daemon-*.src.rpm
```

`--enable-network` is what makes it match a COPR project with net enabled.
Without it the build fails at the first crates.io fetch, which is exactly the
failure COPR would show.

### Things that have bitten this spec

- `URL:` was `https://github.com/%{getenv:COPR_OWNER}/...`. COPR builds the
  SRPM on its own builders where that variable does not exist, so the owner
  came out empty and `Source0` pointed at `github.com//quickshare-daemon`.
  Hardcoded now.
- `%files` lists `%{_userunitdir}`, which only exists if `systemd-rpm-macros`
  is in `BuildRequires`. It is — do not drop it.
- The Nautilus extension is `BuildArch: noarch` in a subpackage so that
  installing the daemon does not pull in `nautilus-python`.
