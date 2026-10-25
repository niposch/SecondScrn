# Third-party notices

SecondScrn is MIT licensed. Its dependencies retain their own copyrights,
licences and notices. This file and the `licenses/` directory accompany the
binary distributions; the Android APK carries notices under `assets/licenses/`.

* [Rust dependency inventory](docs/rust-dependencies.md) lists exact resolved
  versions, upstream sources and declared licence expressions (including build,
  test and target-specific dependencies). [RUST-NOTICES.txt](licenses/RUST-NOTICES.txt)
  collects their packaged copyright, licence and notice files.
* [UPSTREAM-NOTICES.txt](licenses/UPSTREAM-NOTICES.txt) supplies shared and
  supplemental upstream texts, including MIT, Apache-2.0 and UniFFI's MPL-2.0.
  For crc-catalog and r-efi the Apache-2.0 option is used. str0m's split crates
  share its top-level MIT licence. Source links and authors are in the inventory.
* [Resolved Windows packages](docs/windows-dependencies.md) and
  [WINDOWS-NOTICES.txt](licenses/WINDOWS-NOTICES.txt) cover the restored NuGet
  packages, Microsoft binary terms and package notices. The self-contained
  .NET/Windows App SDK components retain their accompanying terms and notices.
* AndroidX, Jetpack Compose, Kotlin and kotlinx.coroutines use Apache-2.0.
  JNA is used under its Apache-2.0 option. See the versions and upstream
  references in the [dependency inventory](docs/dependencies.md). The
  Apache-2.0 text is included in UPSTREAM-NOTICES.txt; notices supplied by
  packaged Java resources remain with those resources.
* The Qualcomm Snapdragon GSR 1 shader is Copyright (c) 2023 Qualcomm
  Innovation Center, Inc. All rights reserved, under BSD-3-Clause. Its exact
  terms are in [LICENSE-SGSR.txt](crates/android-client/src/shaders/LICENSE-SGSR.txt),
  also copied into the binary packages.
* The vendored str0m HEVC depacketizer modification is described in
  [SECONDSCRN-PATCH.md](vendor/str0m/SECONDSCRN-PATCH.md). Its licence is preserved.
  `vendor/str0m/cert.pem` is an upstream development/test certificate and private
  key, publicly distributed with that dependency; SecondScrn does not use it
  for device identities. Each installation generates its own private identity.

UniFFI 0.31.2 source (MPL-2.0) is available from
[mozilla/uniffi-rs, tag v0.31.2](https://github.com/mozilla/uniffi-rs/tree/v0.31.2).
The SecondScrn source, generated-binding build instructions and vendored
modifications are available in this repository. See individual licence texts
for their source-availability provisions.

The Virtual Display Driver is downloaded and installed separately. It is not
part of SecondScrn's installer; its own distribution carries its notices.
Platform video codecs are provided by Windows Media Foundation and Android
MediaCodec. Open-source software licences do not imply additional codec patent
rights beyond the rights their texts grant.
