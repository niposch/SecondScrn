# Releasing

## Windows (Velopack)

```powershell
./packaging/windows/build-release.ps1 -Version 0.0.2            # -> releases/
./packaging/windows/build-release.ps1 -Version 0.0.2 -SignParams "/a /fd sha256 /tr http://timestamp.digicert.com /td sha256"
```

The script builds the Rust core (release), publishes the WinUI app
self-contained for win-x64, checks that the native core, `SecondScrn.pri`
and the driver scripts are present, and runs `vpk pack`. Output:

* `SecondScrn-win-Setup.exe` — per-user installer (no admin), installs to
  `%LOCALAPPDATA%\SecondScrn`, creates shortcuts and an uninstall entry.
* `SecondScrn-<v>-full.nupkg`, `-delta.nupkg` (when the previous release is in
  the output folder), `releases.win.json`, `RELEASES` — the update feed.
* `SecondScrn-win-Portable.zip`.

Publish all files as assets of a GitHub release; the app checks
`https://github.com/niposch/SecondScrn` releases (see `UpdateService`). For
testing an update before publishing, start the installed app with
`SECONDSCRN_UPDATE_SOURCE=<folder with the release files>`.

Behaviour:

* User data (identity, pairings, settings, logs) lives in
  `%LOCALAPPDATA%\SecondScrnData`, outside the install folder, so updates and
  reinstalls keep pairings. Uninstall removes it and the *start with Windows*
  entry.
* The virtual display driver is **not** part of the package and is never
  installed, updated or removed by Velopack. Driver setup is a separate,
  explicit, elevated step (see [virtual-display-driver.md](virtual-display-driver.md)).
* The tablet reconnects automatically after an update restart (the host tells
  it to).

### Signing

Without a code-signing certificate the installer and executables are
unsigned (SmartScreen warns). With a certificate, pass `-SignParams`
(forwarded to `vpk --signParams`, i.e. signtool arguments); Velopack signs the
app binaries, `Update.exe` and `Setup.exe`. Azure Trusted Signing is supported
by `vpk` as well (`--azureTrustedSignFile`).

## Android

```powershell
cd apps/android
./gradlew assembleRelease     # app/build/outputs/apk/release/
./gradlew bundleRelease       # Play Store bundle
```

Signing: create `apps/android/keystore.properties` (not committed):

```
storeFile=/path/to/release.jks
storePassword=…
keyAlias=secondscrn
keyPassword=…
```

The first public release uses a dedicated RSA signing key, stored locally in
`.secrets/android-release.jks`, with the signing configuration in
`apps/android/keystore.properties`. Both are gitignored. Back up both files
privately: Android updates must retain the same signing key. Never upload
them as release assets or include them in a source archive. A debug-signed
development installation must be uninstalled before installing the public
release; warn testers that doing so removes pairings and settings.

Without a signing configuration, release builds are unsigned and must not be
published as an installable APK. Release builds are minified (R8);
`proguard-rules.pro` keeps JNA, the UniFFI bindings and the JNI bridge.
`versionName` comes from the workspace version in `Cargo.toml`.

## Versions before 1.0: lockstep

Until 1.0 the PC and tablet apps only work together at the identical
version (`protocol::versions_compatible`); from 1.0 the protocol version
decides. Every release therefore ships both apps, and users update both.

* The PC app never installs updates silently: it notifies, and Settings
  installs on request. Its prompts say the tablet app must be updated to the
  same version.
* On a mismatch the older side is pointed at exactly the other side's
  version, not the newest release: the tablet offers the APK of the PC's
  version (`SecondScrn-<v>-android.apk` from that release), the PC offers to
  install the tablet's version (Velopack feed of that release) or links the
  matching tablet app. Downgrading the tablet would need an uninstall, which
  loses its pairings.
* Release assets must keep these names (`v<version>` tag,
  `SecondScrn-<version>-android.apk`, `releases.win.json`): the apps build the
  URLs from them.
* Tablets on 0.0.1 predate this and only show "the host and tablet app
  versions are incompatible"; say "update both apps" in the release notes.

## Checklist

1. Bump `version` in `Cargo.toml` (also used by the Android build), the Windows
   csproj version, and `Cargo.lock`; pass the same version to `build-release.ps1`.
2. `cargo test`, `cargo test -p secondscrn-windows-host -- --ignored` on a
   machine with GPU + driver, Android emulator smoke test.
3. Update `docs/status.md`, the README and release notes. Refresh notices with
   `python tools/dependency_inventory.py` and `python tools/bundle_notices.py`
   after restoring dependencies; review upstream licence changes. Both apps
   bundle the collected notices.
4. Build both platforms, test install → update from the previous release.
5. Tag `v<version>`, publish the release files and the APK.

## First release (0.0.1)

Publish the Windows Setup.exe, Portable.zip, full.nupkg, `RELEASES` and
`releases.win.json`, plus the signed APK named `SecondScrn-0.0.1-android.apk`.
The first release has no delta package and no previous public version for an
update-install test. Include `SHA256SUMS.txt` covering the binary assets.
Verify the APK with the installed Android build-tools `apksigner verify`
command. See [release notes](release-notes-0.0.1.md) for scope and limitations.
