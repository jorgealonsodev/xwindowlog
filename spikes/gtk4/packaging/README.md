# Local GTK4 prototype package

This package adds a menu entry for the existing GTK4 feasibility prototype. It is **not the full MVP**: G04 remains partial, and manual interaction and accessibility are unverified. It contains only the UI, with no daemon, service, or autostart changes.

## Build and check

Run `spikes/gtk4/packaging/build-deb.sh` from the checkout. It builds offline, derives runtime dependencies from the built ELF with `dpkg-shlibdeps`, and writes the host-specific package to `spikes/gtk4/packaging/dist/`. Run `spikes/gtk4/packaging/smoke-test.sh` to check it without installing or launching the GUI.

The package uses the build host's native architecture. This build targets Linux Mint 22.3 (Ubuntu noble base), amd64; it is not a universal compatibility claim.

## Launcher

The launcher prefers `/usr/bin/xwindowlog`, then absolute `$HOME/.cargo/bin/xwindowlog`; if neither exists, it opens the UI without `--cli` and preserves the existing hint. It does not search `PATH` for the CLI.

## Install (human only)

```sh
sudo apt install ./spikes/gtk4/packaging/dist/xwindowlog-gtk4-spike_0.1.0-1+local_amd64.deb
sudo apt remove xwindowlog-gtk4-spike
```
