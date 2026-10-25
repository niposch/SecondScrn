# Virtual Display Driver

SecondScrn uses the MIT-licensed [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
(VirtualDrivers, "MttVDD", IddCx) to give Windows a real additional monitor.
It is an external dependency: SecondScrn neither modifies nor bundles it.

## Installation

From the app: **Display → Install driver…** (one UAC prompt). This runs
`Driver\setup-vdd.ps1`, which:

1. obtains the official package with
   `winget install VirtualDrivers.Virtual-Display-Driver --scope machine`
   (the SignPath-signed release);
2. runs `install-vdd.ps1`, which verifies the catalog signature, creates
   `C:\VirtualDisplayDriver\vdd_settings.xml` with **monitor count 0** (no
   phantom monitor until a tablet connects) — an existing settings file is
   kept — grants local users write access to that one file, and installs the
   device `Root\MttVDD` with `devcon`.

Log: `%TEMP%\secondscrn-vdd-install.log`.

Manual alternative: install *VDD Control* from the VirtualDrivers releases and
click *Install*, then make sure `C:\VirtualDisplayDriver\vdd_settings.xml` is
writable by your user.

## How SecondScrn manages it (no elevation)

* **Control:** the driver's local pipe `\\.\pipe\MTTVirtualDisplayPipe`
  (`SETDISPLAYCOUNT`, `RELOAD_DRIVER`) and `vdd_settings.xml`.
* **Resolutions:** the size being streamed, its rotated counterpart and the
  tablet's native size are added to the settings file if missing (marked
  `<!-- added by SecondScrn -->`), and the driver is reloaded — with the
  virtual monitor detached first, so Windows doesn't re-apply the topology
  mid-change. At most four such entries exist at a time; entries SecondScrn
  added earlier are removed when something new is added. Entries you added
  yourself are never touched. The cap matters: every entry is multiplied by
  every `<g_refresh_rate>`, and VDD 2.35 crashed with ~20 entries × 6 rates.
* **Attach/detach:** the VDD target is activated/deactivated with
  `SetDisplayConfig`. Only the VDD path is changed; every other active display
  is snapshotted beforehand, configurations missing one are never applied, and
  any display that drops out is re-activated.
* **Mode:** `ChangeDisplaySettingsEx` on the VDD monitor.
* **Position:** where you put the monitor in *Settings → Display* is saved
  (`%LOCALAPPDATA%\SecondScrnData\virtual-display-position.json`, relative to
  the side it is attached to) and restored on every attach, resolution change
  and rotation.
* **Scale:** the tablet can change the monitor's Windows scale factor (the
  same private display-config call the Settings app uses).
* **Lifetime:** the monitor is kept for a configurable linger time after a
  disconnect (default 30 s), removed afterwards, and removed at startup if a
  previous run crashed.

Monitors of other virtual display drivers (VR headsets, Virtual Desktop,
other tools) are never touched. If you use VDD for something else as well,
note that SecondScrn treats VDD monitors as its own.

## Removal

Not done by the SecondScrn uninstaller (other software may use the driver).
To remove it, run elevated:

```powershell
& "$env:LOCALAPPDATA\SecondScrn\current\Driver\uninstall-vdd.ps1"
```

(or uninstall via VDD Control / Device Manager → Display adapters →
*Virtual Display Driver* → Uninstall device, delete driver).

## Troubleshooting

| Symptom | Check |
|---|---|
| "not installed" | Device Manager → Display adapters → *Virtual Display Driver* present? |
| "not running" | device disabled or failed; re-enable it or reinstall |
| Monitor appears but stays at another resolution | `secondscrn-host-cli vdd status` lists offered modes; the settings file must be writable |
| Device Manager shows code 43, Event Viewer "WUDFUnhandledException … mttvdd.dll" | the driver crashed (e.g. too many resolutions in `vdd_settings.xml`); remove extra entries, then disable and re-enable *Virtual Display Driver* in Device Manager |
| A physical monitor turned off during testing | `secondscrn-host-cli targets`, then `secondscrn-host-cli activate <id>` |
