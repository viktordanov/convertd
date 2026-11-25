# docker

`docker/` builds the one rs-convert image: convertd, a pinned LibreOffice, a pinned PDFium, a fixed font set and `build-info.json`, for `linux/amd64` and `linux/arm64`.

The product image does not convert documents yet. The `runtime` target is written but not built or tested, because convertd does not wire the real engines until wave two (plan 001 step 14). The parts below it are built and proved on both architectures: the engines, the fonts, the library check and the `dev` image.

## Contents

1. [What the image holds](#what-the-image-holds)
2. [Targets](#targets)
3. [Fonts and build-info.json](#fonts-and-build-infojson)
4. [Library check](#library-check)
5. [Moving to a new LibreOffice or PDFium](#moving-to-a-new-libreoffice-or-pdfium)
6. [Size](#size)
7. [What is verified](#what-is-verified)

<!-- memoria:section id="overview" files="Dockerfile install-engines.sh dpkg/docs.cfg dpkg/fonts.cfg" -->
## What the image holds

The image is `debian:trixie-slim` with only what convertd uses. Each part has a reason:

- **LibreOffice 26.8.3** from the `.deb` tarball of The Document Foundation (TDF). The image installs 13 of its packages, which the `engines` stage names. The full set does not install, because `postgresql-sdbc` needs Base. Java, Base, Python, help and other UI languages are not installed. The build fails if `java` or `python3` is present.
- **PDFium `chromium/8179`** from `bblanchon/pdfium-binaries`, with its `LICENSE` and the third-party licenses in `/opt/pdfium`.
- **Fonts** that replace the common Office families with metric-compatible free fonts. [Fonts and build-info.json](#fonts-and-build-infojson) gives the set.
- **No network tools.** curl, ca-certificates and procps are not in the product image. All downloads happen in the `engines` build stage.
- **No documentation.** The dpkg excludes in `dpkg/docs.cfg` drop docs, man pages and message catalogs before dpkg unpacks them. They keep the `copyright` file of each package.

The image runs as user and group `10001:10001` (`convertd`), with `HOME=/tmp`. The Kubernetes manifest and the engine check make the root file system read-only, and `/tmp` is the only writable path. Container Apps is believed to have no read-only option. The image ships nothing under `/tmp`, because a mount there hides it. There is no init process: convertd is PID 1 and reaps its own children.

The paths and variables that other parts of the service depend on:

| Path or variable | Value |
| --- | --- |
| `/opt/libreoffice` | symlink to `/opt/libreoffice26.8`, so configuration never names the version |
| `LO_PROGRAM_DIR` | `/opt/libreoffice/program` |
| `PDFIUM_LIB_DIR` | `/opt/pdfium/lib` |
| `PDFIUM_BUILD` | `8179`, because PDFium has no version call |
| `/opt/convertd/build-info.json` | engine versions and fonts, the default of `BUILD_INFO_PATH` |
| `/etc/convertd/fonts.json` | `{family: metric_compatible}` for the `FONT_SUBSTITUTED` warning |
| `TMP_DIR`, `PORT`, `LOG_MODE` | `/tmp/convert`, `8080`, `prod` |
| `LANG`, `SAL_USE_VCLPLUGIN` | `C.UTF-8`, `svp` (headless LibreOffice) |
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="Dockerfile" -->
## Targets

One `Dockerfile` holds every target, so the product and the test image use the same engines and the same install script. The build context is the repository root.

| Target | What it holds | State |
| --- | --- | --- |
| `runtime` (default) | `runtime-base` plus convertd, the OCI labels and the health check | Written, not built: convertd has no real engines yet |
| `runtime-base` | Debian, the engines, the fonts, `build-info.json` and the user | Built and checked on arm64 and amd64 |
| `dev` | `rust:1.99-trixie` plus the same engines, rustfmt, clippy, `locales` and `procps` | Built on arm64, for the engine tests of lane 002 |
| `engine-check` | `runtime-base` plus the two spike binaries | Built and run on both architectures. Plan 003 step 7 removes it |

The other stages are internal. `engines` downloads and checks the pinned files on the build platform. `lib-check` runs the [library check](#library-check). `rust-cross`, `spike-build`, `chef-tool`, `planner` and `builder` compile Rust on the build platform and cross-compile to the target, so no `rustc` runs under emulation.

The Docker VM on the development Mac has about 4 GB. Build one image at a time. Keep `CARGO_JOBS` at 2. A build that ends with signal 9 ran out of memory. Build the base and the dev image like this:

```sh
docker buildx build -f docker/Dockerfile --target runtime-base \
  --platform linux/arm64 --load -t rs-convert-base:arm64 .
docker buildx build -f docker/Dockerfile --target dev --load -t rs-convert-dev .
```

Run the tests in `dev` with the source mounted and a job-local target folder:

```sh
docker run --rm -v "$PWD":/src -v "$scratch/target":/target \
  -e CARGO_TARGET_DIR=/target rs-convert-dev cargo test
```

`build_docker.sh` in the repository root builds and pushes the `runtime` image as `<repo>:v<x.y.z>`, with the version from `Cargo.toml`. A release build needs a clean tree that carries the tag `v<x.y.z>`. The script is not proved yet (plan 003 step 6).

The build arguments:

| Argument | Meaning |
| --- | --- |
| `LO_VERSION`, `LO_BUILD` | `26.8.3`, and the four-part build `26.8.3.1` |
| `LO_SHA256_AMD64`, `LO_SHA256_ARM64` | SHA-256 of each LibreOffice tarball |
| `PDFIUM_BUILD`, `PDFIUM_SHA256_AMD64`, `PDFIUM_SHA256_ARM64` | `8179`, and SHA-256 of each PDFium tarball |
| `RUST_IMAGE`, `RUNTIME_BASE`, `CARGO_CHEF_VERSION` | base images by index digest, and the cargo-chef version |
| `FONTS_CJK` | `1` adds Noto CJK (size not measured) |
| `CARGO_JOBS` | Cargo jobs per platform, 2 by default |
| `CONVERTD_VERSION`, `CONVERTD_COMMIT`, `CONVERTD_BUILD_DATE` | the OCI version, revision and created labels. convertd also compiles in `CONVERTD_COMMIT` for `/v1/capabilities` and the engine fingerprint. `convertd --version` prints the version from `Cargo.toml` |
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="fonts/fonts.json fonts/10-libreoffice-fonts.conf fonts/35-rs-convert-aliases.conf fonts/probe-families.txt dpkg/fonts.cfg write-build-info.sh install-engines.sh" -->
## Fonts and build-info.json

The font set covers Latin, Greek, Cyrillic, symbols, math and emoji. These families are installed, and the build fails if `fc-list` does not find one of them:

- Liberation Sans, Serif and Mono, Carlito and Caladea: metric-compatible with Arial, Times New Roman, Courier New, Calibri and Cambria.
- DejaVu (core), and the Noto subset in `dpkg/fonts.cfg`: Sans, Serif, Sans Mono, Sans Symbols, Sans Symbols2, Sans Math and Color Emoji.
- The fonts that LibreOffice bundles, OpenSymbol among them. They also cover Arabic, Hebrew, Armenian, Georgian and Lao.

Devanagari, Thai, Ethiopic and CJK have no font. Text in these scripts renders as empty boxes. `FONTS_CJK=1` adds Noto CJK.

`fonts/35-rs-convert-aliases.conf` maps Aptos, Aptos Display and Calibri Light to Carlito. These Office families have no metric-compatible free font, so `fonts.json` marks them `false`. Debian already maps the other Office families.

`write-build-info.sh` writes `/opt/convertd/build-info.json` at build time in the final image. The versions come from the pinned build arguments. `fonts` is the sorted `fc-list` output. `font_substitutes` maps each family in `fonts/probe-families.txt` to the family that `fc-match` picks, aliases included. `default_substitute` is the `fc-match` answer for `sans-serif`, the fallback for any other name. convertd never runs fontconfig itself. On 26.8.3.1 the file is the same on both architectures. These are some of its values:

```json
{"libreoffice": "26.8.3.1", "pdfium": "8179",
 "font_substitutes": {"Calibri": "Carlito", "Aptos": "Carlito", "Georgia": "Noto Serif",
   "Consolas": "DejaVu Sans Mono", "Book Antiqua": "Noto Sans", "Menlo": "Noto Sans"},
 "default_substitute": "Noto Sans"}
```

The fontconfig cache is built into the image, so no worker writes a user cache at start. This works only if fontconfig and LibreOffice use the same directory names. LibreOffice registers `share/fonts/truetype` and `program/resource/common/fonts` by their real path, `/opt/libreoffice26.8/…`, and fontconfig keys its cache on the path as written. Thus `install-engines.sh` rewrites `fonts/10-libreoffice-fonts.conf` to the real directory before `fc-cache` runs. With the symlink path, each LibreOffice start wrote two cache files under `$HOME/.cache/fontconfig`.

To add licensed fonts, build a derived image. It copies the fonts, runs `fc-cache -s -f`, and runs `/opt/convertd/write-build-info.sh` again as root, so `build-info.json` stays true. The script needs `LO_VERSION` set to the four-part build, such as `26.8.3.1`, because the image does not set it. Private fonts then never enter this repository.
<!-- /memoria:section -->

<!-- memoria:section id="library-check" files="Dockerfile libreoffice/ldd-allow.txt" -->
## Library check

The `lib-check` stage runs `readelf -d` on every LibreOffice and PDFium shared object, natively on the build platform. Each `DT_NEEDED` library must exist in the library folders of the target, in the LibreOffice program folder or in the PDFium folder. The stage does not use `ldd`, because `ldd` runs the loader of the target, and the loader is unreliable under emulation. The stage fails if `readelf` cannot see `libc.so.6` in `libmergedlo.so`, so an empty scan cannot pass.

`libreoffice/ldd-allow.txt` lists the misses that the check accepts, as exact `<object>: <library>` lines. An allowed plugin thus cannot hide a missing library in another object. On 26.8.3.1, 162 objects pass with 27 allowed lines, the same on both architectures. All of them are in plugins that a headless conversion never loads: the avmedia playback backends (GStreamer, GTK 4, Qt 6), the LibreOfficeKit GTK widget, the Java OfficeBean and the X11 VCL plugin.
<!-- /memoria:section -->

<!-- memoria:section id="pins" files="Dockerfile install-engines.sh" -->
## Moving to a new LibreOffice or PDFium

Every engine download has an exact version and a SHA-256 check, and an empty hash fails the build. Debian packages are not pinned: a rebuild gets the current trixie packages for the pinned base image. The commit that changes a pin records each new value and where it came from. Do the monthly rebuild the same way: bump the base image digests and the engine pins together.

For a new LibreOffice release:

1. Download `LibreOffice_<version>_Linux_x86-64_deb.tar.gz` and `LibreOffice_<version>_Linux_aarch64_deb.tar.gz` with their `.asc` files. They are in `download.documentfoundation.org/libreoffice/stable/<version>/deb/`, under `x86_64/` and `aarch64/`.
2. Compare `sha256sum` of each tarball with the `.sha256` file next to it. Both values must be equal.
3. Verify each `.asc` with `gpg --verify` in a throwaway `GNUPGHOME`. Use the key that `keyserver.ubuntu.com` gives for `build@documentfoundation.org`.
4. Set `LO_BUILD` to the four-part build in the name of the top folder of the tarball, `LibreOffice_<build>_Linux_<arch>_deb/`.
5. Set `LO_VERSION` and both `LO_SHA256_*` values.

The signing key on 26.8.3 was `C2839ECAD9408FBE9531C3E9F434A1EFAFEEAEA3`. This fingerprint is not confirmed against a TDF page, because no readable TDF page shows it. A new series, such as 27.2, needs no other change: the package names and `/opt/libreoffice27.2` come from `LO_VERSION`.

TDF removes a point release from `/stable/` when the next one ships. The `engines` stage then falls back to `downloadarchive.documentfoundation.org/libreoffice/old/<LO_BUILD>/`. This fallback URL is not verified: 26.8.3 is still the current release, and the archive host refused connections when the pins were set. The pinned SHA-256 still checks whatever it serves.

For a new PDFium build, download `pdfium-linux-x64.tgz` and `pdfium-linux-arm64.tgz` from the `bblanchon/pdfium-binaries` release. Compare their `sha256sum` with the asset digests that the GitHub release gives. Then set `PDFIUM_BUILD` and both `PDFIUM_SHA256_*` values.

Then prove the new pins:

1. Build `engine-check` for `linux/arm64` and for `linux/amd64`.
2. If `lib-check` fails, read the lines that it prints. Add a line to `libreoffice/ldd-allow.txt` only for a plugin that conversion never loads, with the reason.
3. Run both spike binaries in each image, as [What is verified](#what-is-verified) shows.
4. Compare `/opt/convertd/build-info.json` with the old one. A changed font list changes the `FONT_SUBSTITUTED` warnings.
<!-- /memoria:section -->

<!-- memoria:section id="measurement" files="Dockerfile" -->
## Size

These sizes are measurements of the `runtime-base` target, without convertd and without CJK fonts. They were taken on 2026-11-25 on an arm64 Mac (Docker 29.8.2, OrbStack). The amd64 image was built under emulation.

| Image | Uncompressed (`docker image inspect`) | Compressed (`docker save \| gzip -6`) |
| --- | --- | --- |
| `runtime-base`, arm64 | 727 MB | 300 MB |
| `runtime-base`, amd64 | 699 MB | 306 MB |
| `dev`, arm64 | 2.78 GB | not measured |

The working target for the product image is at most 600 MB uncompressed. The largest parts are LibreOffice core (333 MB installed), the icon themes in `images` (73 MB), the bundled fonts (53 MB) and URE (52 MB). Plan 003 step 4 tries the image without `images`. The spike image of `docs/m0-spikes.md` was 708 MB, without the Noto fonts.
<!-- /memoria:section -->

<!-- memoria:section id="verification" files="Dockerfile smoke.sh" -->
## What is verified

The `engine-check` image proves the engines under the production limits. Each run uses a read-only root, a tmpfs on `/tmp`, user `10001:10001`, no capabilities, `no-new-privileges` and no network:

```sh
docker buildx build -f docker/Dockerfile --target engine-check \
  --platform linux/arm64 --load -t rs-convert-engine-check .
docker run --rm --read-only --tmpfs /tmp:rw,size=1g,mode=1777 --user 10001:10001 \
  --cap-drop ALL --security-opt no-new-privileges --network none \
  -v "$corpus":/corpus:ro -e UserInstallation=file:///tmp/p1 -e NO_TRIM=1 -e FORGET=1 \
  --entrypoint bash rs-convert-engine-check -c \
  'spike-lok bench "$LO_PROGRAM_DIR" 2 /corpus/small.docx /corpus/deck.pptx /corpus/sheet.xlsx'
```

On 2026-11-25 this check gave these results:

- **arm64, native:** a DOCX, a PPTX and an XLSX convert to PDF through `/opt/libreoffice/program`, and PDFium renders every page through `/opt/pdfium/lib`.
- **amd64, under emulation on the arm64 Mac:** the same conversions and renders pass. Emulation proves function only, not timing.
- **Both:** no fontconfig user cache is written under `/tmp`, and `lib-check` passes.

The check does not prove the product image, which is not built yet. It also does not prove amd64 at native speed. That proof needs the CI image job on the amd64 runners of GitHub. This job runs only by hand and monthly until convertd converts and the smoke file exists (plan 003 step 7). `smoke.sh` then checks the image size, the time to ready, one DOCX over HTTP and the font cache.

Docker mounts a tmpfs with `noexec` by default. A program that runs from `/tmp` needs `--tmpfs /tmp:rw,exec`.
<!-- /memoria:section -->
