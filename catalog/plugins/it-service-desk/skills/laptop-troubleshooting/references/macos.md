# macOS — checks, commands, known causes

## Facts to get
- Version: Apple menu → About This Mac (or `sw_vers`). Chip: Apple silicon vs Intel changes the recovery steps.
- Disk: `df -h /` or About This Mac → Storage. Under 10% free explains slowness, failed updates, "disk full" in apps.
- Uptime: `uptime`. Weeks without a restart explains memory pressure and stuck agents.
- Management: Jamf `get-computer` → lastContactTime, FileVault, profiles. System Settings → General → Device Management shows the profile on the Mac.
- Battery: System Settings → Battery → Battery Health (Normal / Service Recommended), cycle count in System Information → Power.
- Logs when something crashes: Console.app → Crash Reports, the process name and the first line of the backtrace.

## Symptom → ranked causes → fix
| Symptom | Most often | Check | Fix |
|---|---|---|---|
| Slow, fan loud | A runaway process (Spotlight `mds_stores` reindexing, a browser tab, a sync client), then disk < 10% free, then memory pressure | Activity Monitor → CPU and Memory tabs, sort by % ; `df -h` | Quit the process; free disk (Downloads, Trash, `~/Library/Caches`, old Xcode/simulators, Docker images); restart; if Spotlight reindexes daily, exclude the folder in Siri & Spotlight settings |
| Will not turn on | Power: dead battery or charger; then a stuck SMC/NVRAM state | Charger light/USB-C power; hold power 10 s | Charge 30 min; Apple silicon: hold power until "Loading startup options"; Intel: SMC reset (shift-ctrl-option-power 10 s), NVRAM (cmd-option-P-R) |
| Stuck on login / spinning after password | FileVault unlock with an old password after a directory password change; a login item hang | Does the password work at the FileVault screen? | Use the previous password at FileVault, then the new one at login; Jamf: FileVault password sync via the management framework; Safe Mode (shift on Intel; hold power → Options → shift on Apple silicon) to skip login items |
| Update will not install | Disk space (updates need 20–35 GB free), a management deferral, a stale management channel | `df -h`; Jamf lastContactTime; System Settings → Software Update shows "managed" | Free space; `jamf-pro.update-inventory`; if deferred by policy say so and give the date |
| Wi-Fi fine but no internet | Captive portal, VPN half-connected, a stale DNS | See network reference | — |
| External display not detected | Cable/adapter (USB-C needs DP alt-mode), display sleep state, a stuck WindowServer | Try the other port; Detect Displays (hold option in Displays settings) | Reseat; restart; NVRAM reset on Intel |
| Keyboard/trackpad erratic | Bluetooth peripheral conflict, moisture, a stuck key | Does it happen in Safe Mode? | Toggle Bluetooth; if physical, hardware service |
| Kernel panic / random restarts | A third-party kernel or system extension (VPN, EDR, virtualization), then hardware | Console → Crash Reports → "panic" report names the extension | Update or remove the extension; Apple Diagnostics (hold D / power on boot) |
| "App is damaged and can't be opened" | Gatekeeper quarantine on an unsigned or broken download | `xattr -p com.apple.quarantine <app>` | Re-download from the approved source; managed apps via Self Service; never tell people to disable Gatekeeper |
| Printer missing or job stuck | Queue paused, wrong driver/AirPrint, printer offline | System Settings → Printers → the queue; `lpstat -p` | Resume the queue; remove and re-add (AirPrint first); print a test page from another Mac to isolate |
| Time Machine / backup failing | Destination full or unreachable, sparsebundle corruption | Backup log in Console (filter "backupd") | Re-select destination; for network destinations, verify the share mounts |

## Management actions (Jamf)
- `update-inventory` after any fix so the record reflects it.
- `send-mdm-command` RestartDevice only with the person's yes; DeviceLock only for a lost device and only through escalate-and-record.
- A profile "Pending" for more than a day means the Mac is not checking in: have the person run `sudo jamf policy` or `sudo jamf manage`, or restart.

## Escalate when
No power after charging 30 min and an SMC/power reset; liquid damage; battery "Service Recommended"; panics that name no third-party extension; Apple Diagnostics reports a code.
