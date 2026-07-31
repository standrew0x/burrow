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
and opens a **draft** GitHub Release. Smoke-test the installer, then publish.

Both bundle targets are confirmed to build locally:
`Burrow_0.1.0_x64-setup.exe` (3.5 MB) and `Burrow_0.1.0_x64_en-US.msi` (5 MB).

### Before the first real release

1. **Auto-updates are off,** deliberately. There is no updater plugin and
   `createUpdaterArtifacts` is unset, so no `.sig` files are produced and
   `includeUpdaterJson` stays commented out in the workflow — asking for an
   updater manifest without those makes `tauri-action` hunt for signatures that
   never existed. Turning updates on means doing all four steps listed in
   `release.yml`; any one alone is a no-op or a failed release.

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
src-tauri/src/store.rs          content-addressed blob store
src-tauri/src/db.rs             schema + user_version migrations
src-tauri/src/image_ops.rs      decode, thumbnail, palette sampling
src-tauri/src/color.rs          OkLab conversion + k-means palette
src-tauri/src/ingest.rs         four-phase import, list, colour search
src-tauri/src/commands.rs       Tauri command surface
src-tauri/examples/ingest.rs    dev tool: import without the UI
.github/workflows/release.yml   tag-triggered build, sign, publish
.github/workflows/ci.yml        fmt + clippy + test on PRs
.githooks/pre-push              local gate, saves CI minutes
scripts/models.lock.json        pinned ONNX weights (url + sha256)
scripts/fetch-models.ps1        hash-verified downloader
```

## Ingest pipeline

Import runs in four phases so the expensive work parallelises while SQLite
writes stay serial:

1. hash every candidate in parallel — read + blake3
2. one query drops digests already held, and digests repeated within the batch
3. decode / thumbnail / palette the survivors in parallel, writing blobs
4. one transaction inserts the rows

Phase 3 re-reads each file rather than carrying phase 1's bytes forward. A drop
of 500 photos is several GB; holding all of it to save a re-read the page cache
will serve is the wrong trade. Phase 3 is chunked at 16 files because a decoded
48MP image is ~190MB as RGBA8 and an unbounded fan-out can exhaust RAM.

Blobs are written *before* the DB row. A crash between the two leaves an
orphaned blob — wasted disk, reclaimable by a GC pass — rather than a row
pointing at a file that was never written, which would render as a permanently
broken tile.

Measured on 161 real photos (up to 70MP, 1.4GB total): 44s cold, 0.9s to
re-scan as duplicates, 6MB of thumbnails.

### Why OkLab

Clustering happens in OkLab, not sRGB. sRGB distance does not track perceived
difference — two greens a fixed distance apart look far closer than two blues
the same distance apart — so k-means in sRGB produces clusters that disagree
with what a designer would call "the same colour". k-means seeding is a
deterministic xorshift so a re-index cannot silently change stored swatches and
invalidate saved colour searches.

### Trying it

```bash
cargo run --release --example ingest -- C:\Temp\burrow-lib C:\some\image\folder
```
