# docker

`docker/` builds the one rs-convert image: convertd, a pinned LibreOffice, a pinned PDFium, a fixed font set and `build-info.json`, for `linux/amd64` and `linux/arm64`.

The product image converts documents on arm64 and amd64. On 2026-11-26 the `runtime` image converted a DOCX, a PDF and a PPTX under Docker on arm64, and the CI image job converted a DOCX with it natively on both architectures ([What is verified](#what-is-verified)). It is not run yet on Kubernetes or gVisor. The engines, the fonts and the library check are proved on both architectures.

## Contents

1. [What the image holds](#what-the-image-holds)
2. [Targets](#targets)
3. [Fonts and build-info.json](#fonts-and-build-infojson)
4. [Library check](#library-check)
5. [Moving to a new LibreOffice or PDFium](#moving-to-a-new-libreoffice-or-pdfium)
6. [Size](#size)
7. [What is verified](#what-is-verified)
8. [Releases](#releases)

<!-- memoria:section id="overview" files="Dockerfile install-engines.sh dpkg/docs.cfg dpkg/fonts.cfg dpkg/libreoffice.cfg libreoffice/rs-convert-paper.xcd" -->
## What the image holds

The image is `debian:trixie-slim` with only what convertd uses. Each part has a reason:

- **LibreOffice 26.8.3** from the `.deb` tarball of The Document Foundation (TDF). The image installs 13 of its packages, which the `engines` stage names. The full set does not install, because `postgresql-sdbc` needs Base. Java, Base, Python, help and other UI languages are not installed. The build fails if `java` or `python3` is present. `dpkg/libreoffice.cfg` also drops the parts that a headless conversion never reads: the icon themes except colibre, the offline help, the gallery, the templates, the Basic wizards and the credits document.
- **PDFium `chromium/8179`** from `bblanchon/pdfium-binaries`, with its `LICENSE` and the third-party licenses in `/opt/pdfium`.
- **Fonts** that replace the common Office families with metric-compatible free fonts. [Fonts and build-info.json](#fonts-and-build-infojson) gives the set.
- **Paper locales.** The compiled `en_GB.UTF-8` and `en_US.UTF-8` locales, in `/usr/lib/locale`, without the `locales` package. The workers get `LC_PAPER` from `DEFAULT_PAPER`, and LibreOffice needs this data to read it. LibreOffice reads `LC_PAPER` only while `ooSetupSystemLocale` is empty, and LibreOfficeKit sets it to en-US at start. `libreoffice/rs-convert-paper.xcd` therefore finalizes it as empty at the install layer. The formatting locale stays the system one, en-US. A build gate checks the `.xcd`, and that `LC_PAPER` gives 210 × 297 mm and 216 × 279 mm.
- **No network tools.** curl, ca-certificates and procps are not in the product image. All downloads happen in the `engines` build stage.
- **No documentation.** The dpkg excludes in `dpkg/docs.cfg` drop docs, man pages and message catalogs before dpkg unpacks them. They keep the `copyright` file of each package.

The image runs as user and group `10001:10001` (`convertd`), with `HOME=/tmp`. The Kubernetes manifest and the engine check make the root file system read-only, and `/tmp` is the only writable path. The image ships nothing under `/tmp`, because a mount there hides it. There is no init process: convertd is PID 1 and reaps its own children.

Every file that the `Dockerfile` copies from the checkout gets an explicit mode with `--chmod`, because `COPY` keeps the mode of the checkout. A checkout made under umask 077 thus gives the same image. `COPY --chmod` also gives its mode to a folder that it creates, so the target folders are created first with mode 0755. A build gate also fails if user 65534 cannot read `fonts.json`, the fontconfig files, `build-info.json`, `soffice.bin` or `libpdfium.so`. The CI image job builds from a checkout with umask 077 modes.

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
| `runtime` (default) | `runtime-base` plus convertd, the OCI labels and the health check | Built and run on arm64 and amd64. The CI image job converts on both |
| `runtime-base` | Debian, the engines, the fonts, `build-info.json` and the user | Built and checked on arm64 and amd64 |
| `dev` | `rust:1.99-trixie` plus the same engines, rustfmt, clippy, `locales` and `procps` | Built on arm64, for the engine tests of lane 002 |

The other stages are internal. `engines` downloads and checks the pinned files on the build platform. `lib-check` runs the [library check](#library-check). `rust-cross`, `chef-tool`, `planner` and `builder` compile Rust on the build platform and cross-compile to the target, so no `rustc` runs under emulation.

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
| `FONTS_CJK` | `1` adds Noto CJK, 91 MB installed. The image is then over the 635 MB budget of CI |
| `CARGO_JOBS` | Cargo jobs per platform, 2 by default |
| `CONVERTD_VERSION`, `CONVERTD_COMMIT`, `CONVERTD_BUILD_DATE` | the OCI version, revision and created labels. convertd also compiles in `CONVERTD_COMMIT` for `/v1/capabilities` and the engine fingerprint. `convertd --version` prints the version from `Cargo.toml` |
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="fonts/fonts.json fonts/10-libreoffice-fonts.conf fonts/35-rs-convert-aliases.conf fonts/probe-families.txt dpkg/fonts.cfg dpkg/libreoffice.cfg write-build-info.sh install-engines.sh cache-fonts.sh" -->
## Fonts and build-info.json

The font set covers Latin, Greek, Cyrillic, symbols, math and emoji, and the scripts in the list that follows. These families are installed, and the build fails if `fc-list` does not find one of them:

- Liberation Sans, Serif and Mono, Carlito and Caladea: metric-compatible with Arial, Times New Roman, Courier New, Calibri and Cambria.
- DejaVu, and the Noto families Sans, Serif, Sans Mono, Sans Symbols, Sans Symbols2, Sans Math and Color Emoji.
- The fonts that LibreOffice bundles, OpenSymbol among them. They also cover Arabic, Hebrew, Armenian, Georgian and Lao. Their Noto Sans also covers Devanagari.
- From `fonts-noto-core`, one Noto family for each of these scripts: Thai, Ethiopic, Bengali, Gujarati, Gurmukhi, Kannada, Malayalam, Oriya, Tamil, Telugu, Sinhala, Khmer, Myanmar, Tibetan, Thaana, Syriac, Mongolian, Cherokee, Yi and Ol Chiki.
- WenQuanYi Micro Hei, a fallback for Chinese, Japanese and Korean. It has one glyph form for all three. `FONTS_CJK=1` adds Noto CJK, with the correct glyph forms for each region.

A script with no font renders blank, or as wrong glyphs. The PDF text layer still holds the text. Only a page that renders fully blank gets a warning, `BLANK_PAGE`. Thus the build also fails if no font has a letter of each script in this list, or of Devanagari, Arabic or Hebrew. Rare and historic scripts still have no font, for example Javanese, Balinese and Adlam. `docs/ledger/fonts-study/` gives the measurements.

LibreOffice bundles its own copies of Liberation, Carlito, Caladea, DejaVu, Noto Sans and Noto Serif, and Debian packages install them too. The image keeps one copy of each family: the copy that fontconfig picked when both were present. That is Debian's Carlito and LibreOffice's copy of the others, the same version or a newer one. `dpkg/fonts.cfg` and `dpkg/libreoffice.cfg` drop the other copy, and `build-info.json` stays the same. A new LibreOffice release can change these versions, so the pin procedure checks them again.

`fonts/35-rs-convert-aliases.conf` maps Aptos, Aptos Display and Calibri Light to Carlito. These Office families have no metric-compatible free font, so `fonts.json` marks them `false`. Debian already maps the other Office families.

`write-build-info.sh` writes `/opt/convertd/build-info.json` at build time in the final image. The versions come from the pinned build arguments. `fonts` is the sorted `fc-list` output. `font_substitutes` maps each family in `fonts/probe-families.txt` to the family that `fc-match` picks, aliases included. `default_substitute` is the `fc-match` answer for `sans-serif`, the fallback for any other name. `coverage` is the union of the charsets of every face that `fc-list` lists, as sorted, merged and inclusive ranges of code points in decimal (`[[32, 126], [160, 591], …]`). `GLYPHS_MISSING` compares the text of a document with it. The image has no Python, so the script merges the ranges with POSIX `awk`, and it fails the build when `fc-list` reports no charset. convertd never runs fontconfig itself. On 26.8.3.1 the file is the same on both architectures. These are some of its values:

```json
{"libreoffice": "26.8.3.1", "pdfium": "8179",
 "font_substitutes": {"Calibri": "Carlito", "Aptos": "Carlito", "Georgia": "Noto Serif",
   "Consolas": "DejaVu Sans Mono", "Book Antiqua": "Noto Sans", "Menlo": "Noto Sans"},
 "default_substitute": "Noto Sans"}
```

The fontconfig cache is built into the image, so no worker writes a user cache at start. This works only if fontconfig and LibreOffice use the same directory names. LibreOffice registers `share/fonts/truetype` and `program/resource/common/fonts` by their real path, `/opt/libreoffice26.8/…`, and fontconfig keys its cache on the path as written. Thus `install-engines.sh` rewrites `fonts/10-libreoffice-fonts.conf` to the real directory before `fc-cache` runs. With the symlink path, each LibreOffice start wrote two cache files under `$HOME/.cache/fontconfig`.

fontconfig also trusts a cache file only if the modification time of its directory matches in seconds and in nanoseconds. A layer that passes through a tar file keeps whole seconds only. A `docker-container` buildx builder, the GitHub Actions cache and a registry push all do this. Before the fix, an image from such a builder wrote 9 user cache files at start, one for each font directory. `cache-fonts.sh` therefore sets every font directory to a whole-second time, runs `fc-cache`, and fails the build if an unprivileged user still sees a stale cache.

To add licensed fonts, build a derived image. It copies the fonts, runs `/opt/convertd/cache-fonts.sh`, and runs `/opt/convertd/write-build-info.sh` again as root, so `build-info.json` stays true. The script needs `LO_VERSION` set to the four-part build, such as `26.8.3.1`, because the image does not set it. Private fonts then never enter this repository.
<!-- /memoria:section -->

<!-- memoria:section id="library-check" files="Dockerfile libreoffice/ldd-allow.txt" -->
## Library check

The `lib-check` stage runs `readelf -d` on every LibreOffice and PDFium shared object, natively on the build platform. Each `DT_NEEDED` library must exist in the library folders of the target, in the LibreOffice program folder or in the PDFium folder. The stage does not use `ldd`, because `ldd` runs the loader of the target, and the loader is unreliable under emulation. The stage fails if `readelf` cannot see `libc.so.6` in `libmergedlo.so`, so an empty scan cannot pass.

`libreoffice/ldd-allow.txt` lists the misses that the check accepts, as exact `<object>: <library>` lines. An allowed plugin thus cannot hide a missing library in another object. On 26.8.3.1, 162 objects pass with 27 allowed lines, the same on both architectures. All of them are in plugins that a headless conversion never loads: the avmedia playback backends (GStreamer, GTK 4, Qt 6), the LibreOfficeKit GTK widget, the Java OfficeBean and the X11 VCL plugin.
<!-- /memoria:section -->

<!-- memoria:section id="pins" files="Dockerfile install-engines.sh fetch-verified.sh" -->
## Moving to a new LibreOffice or PDFium

Every engine download has an exact version and a SHA-256 check, and an empty hash fails the build. Debian packages are not pinned: a rebuild gets the current trixie packages for the pinned base image. The commit that changes a pin records each new value and where it came from. Do the monthly rebuild the same way: bump the base image digests and the engine pins together.

For a new LibreOffice release:

1. Download `LibreOffice_<version>_Linux_x86-64_deb.tar.gz` and `LibreOffice_<version>_Linux_aarch64_deb.tar.gz` with their `.asc` files. They are in `download.documentfoundation.org/libreoffice/stable/<version>/deb/`, under `x86_64/` and `aarch64/`.
2. Compare `sha256sum` of each tarball with the `.sha256` file next to it. Both values must be equal.
3. Verify each `.asc` with `gpg --verify` in a throwaway `GNUPGHOME`. Use the key that `keyserver.ubuntu.com` gives for `build@documentfoundation.org`.
4. Set `LO_BUILD` to the four-part build in the name of the top folder of the tarball, `LibreOffice_<build>_Linux_<arch>_deb/`.
5. Set `LO_VERSION` and both `LO_SHA256_*` values.

The signing key on 26.8.3 was `C2839ECAD9408FBE9531C3E9F434A1EFAFEEAEA3`. This fingerprint is not confirmed against a TDF page, because no readable TDF page shows it. A new series, such as 27.2, needs no other change: the package names and `/opt/libreoffice27.2` come from `LO_VERSION`.

`fetch-verified.sh` tries a list of URLs in order and keeps the first file that has the pinned SHA-256. For LibreOffice the list is `LO_STABLE_MIRRORS`, then the TDF archive. The first entry is the TDF redirector, which picks a mirror near the build machine. A mirror can send an incomplete certificate chain, which Debian curl rejects with error 60. On 2026-11-26 this stopped the image job on a GitHub amd64 runner. Thus fixed mirrors in Germany, the United States and Denmark come next.

TDF removes a point release from `/stable/` when the next one ships. The `engines` stage then uses `downloadarchive.documentfoundation.org/libreoffice/old/<LO_BUILD>/`. This archive URL is not verified for an archived release, because 26.8.3 is still the current release. The archive host also redirects to the mirrors. The pinned SHA-256 still checks whatever a URL serves.

For a new PDFium build, download `pdfium-linux-x64.tgz` and `pdfium-linux-arm64.tgz` from the `bblanchon/pdfium-binaries` release. Compare their `sha256sum` with the asset digests that the GitHub release gives. Then set `PDFIUM_BUILD` and both `PDFIUM_SHA256_*` values.

Then build the images with the new pins. Keep the old `runtime` image for the comparison:

1. Build the `runtime` image and the `dev` image for `linux/arm64`, as [Targets](#targets) shows.
2. If `lib-check` fails, read the lines that it prints. Add a line to `libreoffice/ldd-allow.txt` only for a plugin that conversion never loads, with the reason.
3. Compare `/opt/convertd/build-info.json` with the old one. A changed font list changes the `FONT_SUBSTITUTED` warnings. For each duplicate family in `dpkg/fonts.cfg`, make sure that `fc-list` shows the kept copy with the higher or the same version.

Then test the new images. Generate the corpus first with `corpus/generate.py corpus/out --hostile`, and build `corpus-bench` for the architecture of the image:

1. In the `dev` image, run the engine suites with the corpus: `cargo test --features test-hooks --test engines_office --test engines_render --test engines_poison --test pool --test reap_race`, with `CONVERTD_CORPUS_DIR` set. Run them once with `-- --nocapture`, and make sure that no test prints `skipped` for a missing engine.
2. Run `docker/smoke.sh <new image> docker/smoke/small.docx 635` with `CURL_IMAGE` set.
3. Run `docker/corpus-check.sh <new image> <corpus-bench>` with `HARNESS_IMAGE` set. All cases of `corpus/expected.json` must pass.
4. Run `docker/ab-pages.sh <old image> <new image> corpus/out` with `CURL_IMAGE` set. Every status must be the same, and every page must have the same hash, unless the release notes explain the change.

A push to `main` that changes `docker/` then runs the image job. It builds both architectures natively and repeats the smoke test and the corpus check.
<!-- /memoria:section -->

<!-- memoria:section id="measurement" files="Dockerfile dpkg/libreoffice.cfg" -->
## Size

These sizes are measurements without CJK fonts, on an arm64 Mac (Docker 29.8.2, OrbStack), on 2026-11-26, before and after the LibreOffice trims of `dpkg/libreoffice.cfg`. The amd64 images were built under emulation. The amd64 `runtime` estimate is `runtime-base` plus the convertd layer. The CI image job then measured both `runtime` images natively on GitHub runners: 578 MB on amd64 and 604 MB on arm64, after the trims.

| Image | Before the trims | After the trims | Compressed after (`docker save \| gzip -6`) |
| --- | --- | --- | --- |
| `runtime`, arm64 | 733.0 MB | 605.0 MB | 215 MB (303 MB before) |
| `runtime-base`, amd64 | 699.3 MB | 571.3 MB | not measured |
| `runtime`, amd64 (estimate) | 705.7 MB | 577.7 MB | not measured |
| `runtime`, amd64 (CI, native) | 706 MB | 578 MB | not measured |
| `dev`, arm64 | 2.78 GB | 2.65 GB | not measured |

Uncompressed sizes come from `docker image inspect`. The working target for the product image is at most 600 MB uncompressed, on amd64, the production architecture. The trims save 128 MB on each architecture. The largest savings are the icon themes (72 MB), the gallery (14 MB), the offline help (11 MB of content in 5334 files), the duplicate fonts and the templates (8 MB). The spike image of `docs/m0-spikes.md` was 708 MB, without the Noto fonts.

Later on 2026-11-26, the fonts for the scripts of living languages and WenQuanYi Micro Hei added 11.6 MB. The arm64 `runtime` image then measured 616.5 MB, under the CI budget of 635 MB. The same files go into the amd64 image, so it comes to about 590 MB. The next CI image job measures it natively. The final arm64 `runtime` image, built from `main` at `1eaf24b` with the performance work, measured 615.9 MB (615917307 bytes). Its convertd is 5.8 MB. The paper locales then added about 3 MB: 618.9 MB at `1eafd89` on the branch of `DEFAULT_PAPER`.
<!-- /memoria:section -->

<!-- memoria:section id="verification" files="Dockerfile smoke.sh ab-pages.sh corpus-check.sh" -->
## What is verified

Before convertd had real engines, an `engine-check` image proved the engines under the production limits: a read-only root, a tmpfs on `/tmp`, user `10001:10001`, no capabilities, `no-new-privileges` and no network. It ran the M0 spike binaries. Both the image and the spikes are removed now, because the `runtime` image and the CI image job cover the same ground. On 2026-11-25 that check gave these results:

- **arm64, native:** a DOCX, a PPTX and an XLSX convert to PDF through `/opt/libreoffice/program`, and PDFium renders every page through `/opt/pdfium/lib`.
- **amd64, under emulation on the arm64 Mac:** the same conversions and renders pass. Emulation proves function only, not timing.
- **Both:** no fontconfig user cache is written under `/tmp`, and `lib-check` passes.

On 2026-11-26 the arm64 `runtime` image, built from `main` with the real engines, gave these results:

- **Start:** with a read-only root, a tmpfs on `/tmp`, no capabilities, `no-new-privileges`, `OFFICE_WORKERS=1` and `RENDER_WORKERS=2`, `/readyz` answers 200 after about 1 s.
- **Health check:** Docker reports `healthy` about 6 s after start, and `convertd healthcheck` exits 0.
- **Conversions:** through a port published on `127.0.0.1`, a DOCX of 35 pages converts to PDF, a PDF to page images in a `tar.zst` bundle, and a PPTX to a bundle. Each request gets status 200.
- **Stop:** `docker stop` sends `SIGTERM`, and convertd exits with code 0 after 281 ms. The log has no `ERROR` or `WARN` line.

An earlier `runtime` build of the same day, with no real engines in convertd, gave these results:

- **Configuration:** user `10001:10001`, `WORKDIR /tmp`, `ENTRYPOINT ["/usr/local/bin/convertd"]`, `CMD ["serve"]`, and the OCI labels with the commit and both engine versions.
- **Start:** with a read-only root, a tmpfs on `/tmp`, no capabilities and no network, convertd runs as PID 1 and creates `/tmp/convert`. `/healthz` answers 200.
- **Health check:** `convertd healthcheck` exits 1 while `/readyz` answers 503, and also when nothing listens.
- **Build cache:** after a change in `src/`, the cooked dependency layer comes from the cache, and the rebuild takes 18 s.

These results prove conversion on arm64 under Docker. They do not prove Kubernetes or gVisor.

On 2026-11-26 the LibreOffice trims passed these gates on arm64, against a baseline image built from the same `main` without them:

- **Pages:** `ab-pages.sh` converted the 42 files of `corpus/out` to PNG pages at 72 dpi with both images. All 42 statuses and all 192 page hashes are the same, 160 of them distinct. That includes `canary/remote-image.docx`, which shows the missing-image icon.
- **Corpus:** `corpus-bench check` passed 59 of 59 cases on both images, with the same results apart from timings. The listener control passed, and the service log had no error.
- **Engine tests:** in the trimmed `dev` image, `engines_office`, `engines_render` and `engines_poison` passed (20 tests, one ignored). Two tests skipped: one needs `CONVERTD_SMALL_DISK`, and one finds no `encrypted/deck20.pptx` in the corpus.
- **Build and smoke:** `build-info.json` is the same, `lib-check` passes, and `smoke.sh` passes.

The CI image job gives the amd64 proof at native speed, on the amd64 runners of GitHub. The job runs by hand, monthly, and on pushes to `main` and pull requests that touch `docker/`, `src/engines/`, `src/worker/`, `src/preflight/`, `corpus/`, `.github/actions/image-gates/` or `Cargo.lock`. A push compares with the commit before it, and a push with no such commit runs the job. It builds the image natively on each architecture and runs `smoke.sh` with `smoke/small.docx`. Its first passing run on 2026-11-26, before the trims, measured 706 MB on amd64 and 732 MB on arm64. The amd64 image was ready after 773 ms and converted the DOCX in 56 ms. The run after the trims (45796989297) measured 578 MB on amd64 and 604 MB on arm64. The amd64 image was ready after 764 ms and converted the DOCX in 61 ms. The command:

```sh
docker/smoke.sh rs-convert:ci docker/smoke/small.docx 635
```

`smoke.sh` runs the image with the production limits and checks five things:

- The image size against the budget. The job uses 635 MB, the measured 605 MB of the arm64 `runtime` image plus 5 %, or the `IMAGE_MAX_MB` variable.
- The time to ready, and a start with no `WARN` or `ERROR` line in the log.
- One DOCX to PDF over HTTP. curl must exit 0, the body must have its `Content-Length` and end with `%%EOF`, and `Convert-Page-Count` must be 2. The script prints the conversion time and the `Server-Timing` header.
- That no job directory is left after the response.
- That no fontconfig user cache is written.

After the smoke test, the job runs `corpus-check.sh`. Both gates live in `.github/actions/image-gates`, which `release.yml` runs too. `corpus-check.sh` is the `corpus-bench check` of lane 005, every case of `corpus/expected.json`, against the image. The harness runs in a pinned Debian container inside the network namespace of the service. Only there does the canary listener see a connection from LibreOffice. On 2026-11-26 this check passed 59 of 59 cases on the arm64 image by hand. The first automatic image run on `main` (45805165059) passed it on amd64.

`smoke/small.docx` is the corpus entry `docx-small`, 36986 bytes. `corpus/generate.py` makes the same bytes, and its SHA-256 is in `corpus/expected.json`.

Docker mounts a tmpfs with `noexec` by default. A program that runs from `/tmp` needs `--tmpfs /tmp:rw,exec`.
<!-- /memoria:section -->

## Releases

A tag `vX.Y.Z` runs `.github/workflows/release.yml`. The tag must be `v` plus the `[package]` version of `Cargo.toml`, or the run stops. Tags such as `client/vX.Y.Z`, for the Go module, do not run it. The workflow has five jobs:

1. **version** reads the version and decides the tags.
2. **build** builds the `runtime` image natively on amd64 and arm64. It runs the gates of the image job: the umask guard, `smoke.sh` with the size budget and the clean start, and the corpus check. It holds no registry credential.
3. **push** builds each image again from the cache of the build job. It checks that the config digest is the gated one, and pushes the image by digest, with provenance and an SBOM.
4. **publish** joins the two digests into one multi-arch manifest, `ghcr.io/<owner>/rs-convert:X.Y.Z`.
5. **release** creates the GitHub release for the tag, with generated notes and the image digest.

The manifest also gets `X.Y` when the release is the newest of its line, and `latest` when it is the newest of all. A patch to an older line thus moves neither tag. The workflow refuses to overwrite an existing `X.Y.Z`. It checks before the push and again before the manifest. Only the push and publish jobs may write packages, and only the release job may write the repository's contents.

Run by hand (`workflow_dispatch`), the workflow is a dry run: the version and build jobs only, with no registry login, push or release. The push leg has not run yet.

GitHub makes a new package private on its first publish, and the owner then makes it public once. On the package page, open **Package settings**, then **Danger Zone**, **Change visibility**, **Public**. GitHub cannot make a public package private again. The image's `org.opencontainers.image.source` label links the package to the repository, so the package takes the repository's access permissions, but not its visibility.

`build_docker.sh` stays for builds outside CI, such as the CJK variant. It tags `<repo>:v<x.y.z>`; the workflow tags `X.Y.Z`, without the `v`.
