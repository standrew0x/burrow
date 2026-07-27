# Burrow

A local-first visual reference manager for Windows. Capture, organize, and
search design references without leaving the desktop.

---

## Prerequisites

| Tool | Install |
| --- | --- |
| git | — |
| Node 22+ | — |
| Rust / cargo | `winget install Rustlang.Rustup` |
| MSVC Build Tools + Windows SDK | `winget install Microsoft.VisualStudio.2022.BuildTools` with the VCTools workload |
| WebView2 runtime | ships with Windows 11 |
| GitHub CLI (`gh`), optional | `winget install GitHub.cli` |

Rust's `x86_64-pc-windows-msvc` target cannot link without the MSVC toolchain
and Windows SDK. The `x86_64-pc-windows-gnu` toolchain is **not** a workaround
here — Tauri's WebView2 and `windows-rs` dependencies expect MSVC.

## Scaffolding the app

Already done, but for reference — `create-tauri-app` takes `--force` to write
into a non-empty directory, which is how it was layered on top of this repo:

```bash
npm create tauri-app@latest burrow -- --force -m npm -t react-ts --identifier co.burrow.app --tauri-version 2 -y
```

Run from the *parent* directory, with `burrow` as the project name. There is no
`--manifest-path` flag. Because `--force` will overwrite a root `.gitignore` and
`README.md` from its own template, commit before running it and diff after.

## Development

```bash
npm install
npm run tauri dev
```

Confirm the ignore rules hold before any commit:

```bash
git status --short
```

`target/`, `node_modules/`, and `models/` must not appear.

## Repository decisions

**`Cargo.lock` is committed.** This is a shipping application, not a library.
Committing the lockfile is what makes a CI build byte-reproducible, and it is
the cache key for `Swatinem/rust-cache`.

**Model weights are never committed.** SigLIP encoders are 100–350MB each and
git retains every revision permanently. They are published once as GitHub
Release assets, pinned by SHA-256 in `scripts/models.lock.json`, and fetched by
`scripts/fetch-models.ps1` at build time. Git LFS is not an alternative — the
free tier is 1 GiB storage and 1 GiB/month bandwidth, which a few CI runs
exhaust.

**Dev libraries live outside the repo.** This app's whole job is accumulating
images. Point dev builds at `%LOCALAPPDATA%\burrow` — the same path shipping
builds use. `fixtures/`, `dev-library/`, and `blobs/` are ignored as a
backstop, but the habit matters more than the safety net.

**CI runs on tags, not pushes.** Free-tier private repos get 2,000 CI
minutes/month and Windows runners bill at **2x**, so the real budget is ~1,000
Windows minutes. A cold Rust/Tauri build burns 10–20. Day-to-day checks run
locally through `.githooks/pre-push`; `ci.yml` only fires on pull requests.

Enable the hook once per clone:

```bash
git config core.hooksPath .githooks
```

## Releasing

```bash
git tag v0.1.0 && git push origin v0.1.0
```

`release.yml` builds on `windows-latest`, fetches models, bundles NSIS + MSI,
and opens a **draft** GitHub Release. Smoke-test the installer, then publish —
publishing is what exposes the new version to every user's auto-updater.

### Before the first real release

1. **Updater keys.** `npm run tauri signer generate -- -w tauri-updater.key`.
   Private key → repo secret `TAURI_SIGNING_PRIVATE_KEY`. Public key →
   `plugins.updater.pubkey` in `tauri.conf.json`. The private key must never
   land on disk in this repo; a leak lets anyone push a signed update to every
   install.

2. **Code signing.** Unsigned installers hit a SmartScreen wall that kills
   conversion. Since 2023 OV certs require an HSM, so realistically that means
   Azure Trusted Signing (~$10/mo) or a token-based cert (~$250–400/yr). The
   wiring is stubbed with instructions in `.github/workflows/release.yml`.

   Sign via Tauri's `bundle.windows.signCommand`, not as a post-build step —
   the `.exe` has to be signed *before* it is embedded in the installer, and
   the updater signature computed over the final installer bytes. Post-hoc
   signing invalidates the updater manifest.

## Layout

```
.github/workflows/release.yml   tag-triggered build, sign, publish
.github/workflows/ci.yml        fmt + clippy + test on PRs
.githooks/pre-push              local gate, saves CI minutes
scripts/models.lock.json        pinned ONNX weights (url + sha256)
scripts/fetch-models.ps1        hash-verified downloader
```
