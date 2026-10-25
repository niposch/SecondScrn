# Releasing

## Windows (Velopack)

```powershell
./packaging/windows/build-release.ps1 -Version 0.2.0            # -> releases/
./packaging/windows/build-release.ps1 -Version 0.2.0 -SignParams "/a /fd sha256 /tr http://timestamp.digicert.com /td sha256"
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

Without it, release builds are unsigned. Release builds are minified (R8);
`proguard-rules.pro` keeps JNA, the UniFFI bindings and the JNI bridge.
`versionName` comes from the workspace version in `Cargo.toml`.

## Checklist

1. Bump `version` in `Cargo.toml` (also used by the Android build) and pass the
   same version to `build-release.ps1`.
2. `cargo test`, `cargo test -p secondscrn-windows-host -- --ignored` on a
   machine with GPU + driver, Android emulator smoke test.
3. Update `docs/status.md` and `THIRD_PARTY_NOTICES.md` (run
   `cargo about`/review new dependencies).
4. Build both platforms, test install → update from the previous release.
5. Tag `v<version>`, publish the release files and the APK.
