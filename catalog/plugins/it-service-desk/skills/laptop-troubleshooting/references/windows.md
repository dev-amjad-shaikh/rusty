# Windows 10/11 — checks, commands, known causes

## Facts to get
- Version: Settings → System → About, or `winver` (build number matters: a build two feature updates behind is out of support and out of compliance).
- Disk: `Get-PSDrive C` or Settings → System → Storage. Under 10% free: slowness, update failures, Outlook OST errors.
- Uptime: Task Manager → Performance → CPU → "Up time". Fast Startup hides restarts: "Shut down" is not a restart; ask for Restart.
- Management: Intune `get-device` → lastSyncDateTime, complianceState, isEncrypted; on the device Settings → Accounts → Access work or school → Info → Sync.
- Battery: `powercfg /batteryreport` writes battery-report.html; Full Charge Capacity vs Design Capacity under 60% is end of life.
- Events: Event Viewer → Windows Logs → System (Critical, Error) around the time; Reliability Monitor (`perfmon /rel`) for a timeline.

## Symptom → ranked causes → fix
| Symptom | Most often | Check | Fix |
|---|---|---|---|
| Slow | Disk 100% (Task Manager → Performance), then startup apps, then antivirus/EDR scan, then memory (8 GB with 30 tabs), then a failing HDD | Task Manager; `Get-PSDrive C`; Settings → Apps → Startup | Restart (real restart); free disk; trim startup apps; if disk 100% persists on an HDD, hardware (SSD swap) |
| Will not boot / blue screen | A driver or update (stop code names it: DRIVER_IRQL_NOT_LESS_OR_EQUAL, INACCESSIBLE_BOOT_DEVICE), then disk, then memory | Photograph the stop code; Reliability Monitor after boot | Safe Mode (Shift+Restart → Troubleshoot → Startup Settings), roll back the driver, uninstall the last update (Settings → Windows Update → Update history → Uninstall); `chkdsk /f`; Windows Memory Diagnostic |
| BitLocker recovery screen | A firmware/TPM change (BIOS update, docking-station firmware), Secure Boot toggled, a hardware change | Ask what changed | Recovery key from Intune (`get-device` → the desk reads it in the portal) or Entra "My devices"; after use, `microsoft-intune.rotate-bitlocker-keys` |
| Update stuck / fails (0x800f0922, 0x80070002) | Disk space, a corrupt update cache, pending restart | Settings → Windows Update → Update history | Restart; `DISM /Online /Cleanup-Image /RestoreHealth` then `sfc /scannow`; stop Windows Update service, rename `C:\Windows\SoftwareDistribution`, start it; Intune `sync-device` |
| Not compliant / "your organization requires" | Encryption off, OS build old, no PIN/Hello, firewall or Defender off | `get-device-compliance` names the failing setting | Fix that setting; `sync-device`; compliance evaluates within an hour |
| Outlook/Teams sign-in loops | Broken Work account token (WAM), then time skew, then conditional access | `dsregcmd /status` (AzureAdJoined, AzureAdPrt YES/NO) | Settings → Accounts → Access work or school → disconnect/reconnect; `dsregcmd /leave` only for Entra-registered, never for joined devices without the desk; check clock |
| Printer missing / stuck | Print Spooler service, driver mismatch after a Windows update, a Universal Print or server queue offline | services.msc → Print Spooler; `Get-Printer`; the queue window | Restart spooler (`Restart-Service Spooler`), clear `C:\Windows\System32\spool\PRINTERS`, remove and re-add the printer; print from a colleague's PC to isolate |
| External monitor / dock | Dock firmware, display driver, USB-C bandwidth (two 4K at 60 Hz needs DP 1.4) | Device Manager → Monitors, Display adapters | Reseat, other port, update dock firmware; `Win+Ctrl+Shift+B` resets the graphics driver |
| Audio device missing | Wrong default device after a headset, a driver update | Settings → System → Sound → Output | Pick the device; Device Manager → uninstall the audio device → Scan for hardware changes |
| Wi-Fi adapter gone | Driver, power management turned it off, airplane mode | Device Manager → Network adapters (yellow mark?) | Roll back driver; adapter properties → Power Management → untick "allow the computer to turn off" |
| "Something went wrong" in Microsoft apps 80090016/80090034 | TPM/Hello key issue on the account | `dsregcmd /status` | Sign out of the app, `certutil -deletehellocontainer`, sign in again (documented Microsoft fix); disconnect/reconnect work account |

## Management actions (Intune)
- `sync-device` after any fix; compliance and policy reflect within an hour.
- `restart-device` with the person's yes; `rotate-bitlocker-keys` after any recovery key was read out.
- lastSyncDateTime more than 3 days old: the device is not talking to Intune; on the device, Settings → Accounts → Access work or school → Info → Sync, then check `dsregcmd /status`.

## Escalate when
Stop codes without a named driver after a rollback; disk hardware errors (Event ID 7, 51, 153); no power; liquid; battery under 60% health; a BitLocker recovery loop after the key.
