---
name: amber-install
description: >-
  Requirements and troubleshooting for the amber microVM sandbox. The sandbox
  downloads a prebuilt amber automatically on first use, so there is usually
  nothing to install. Use this only when a run fails with a host requirement
  (HVF/KVM), a download problem, or when you want to build amber from source.
allowed-tools: Bash(*)
---

# amber sandbox: requirements & troubleshooting

You normally **don't need to do anything**: the first sandbox run downloads a
prebuilt amber bundle (binary + resin kernel + in-guest agent + userland) for
this platform into `~/.cache/amber/` and uses it. This skill is for when that
isn't possible or fails.

## Host requirements

- **arm64 only.** amber runs an arm64 Linux guest; the host must be Apple Silicon
  (macOS) or arm64 Linux. x86 hosts are not supported.
- **macOS (Apple Silicon):** uses Hypervisor.framework. The prebuilt binary is
  ad-hoc codesigned with the HVF entitlement; the fetch step clears the download
  quarantine so it can run. Nothing else to configure.
- **Linux (arm64):** needs `/dev/kvm`; your user must be in the `kvm` group
  (`sudo usermod -aG kvm "$USER"` then re-login).
- **Docker** is needed only to build a template's OCI image userland the first
  time per image (e.g. `alpine:3`); it pulls and converts the image.

## Force or pin the download

```bash
# Re-download the latest prebuilt and print its dir (used as AMBER_HOME):
"${CLAUDE_PLUGIN_ROOT}/scripts/amber-fetch.sh"
```

- `AMBER_REPO=owner/repo`: pull from a fork's releases.
- `AMBER_RELEASE=v0.1.0`: pin a specific release (default: latest).

## Use a source checkout instead (development)

If you have the amber repo and want to run your own build, point the sandbox at
it and build with the Makefile (every `cargo build` invalidates the macOS
signature, so always go through `make`):

```bash
export AMBER_HOME=/path/to/amber
cd "$AMBER_HOME"
make build                # builds + codesigns
make kernel               # resin kernel -> assets/Image (Docker, one-time)
./scripts/build-agent.sh  # in-guest agent -> assets/amber-agent
./scripts/fetch-assets.sh # userland -> assets/irx
```

The prebuilt bundles are produced by the repo's `release` GitHub Actions workflow
(`.github/workflows/release.yml`): the same steps, run on tag.

## Troubleshooting

- **Download fails / no release yet** → build from source (above), or set
  `AMBER_HOME` to a bundle you have.
- **macOS "Hypervisor.framework" / signature errors** → the binary lost its
  signature; re-fetch, or in a checkout run `make sign`.
- **Linux "permission denied" on `/dev/kvm`** → join the `kvm` group.
- **exec hangs / "agent did not connect"** → restart the daemon from the bundle
  dir: `./amber down && ./amber up`.
