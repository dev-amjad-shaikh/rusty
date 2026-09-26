# ChromeOS — checks, known causes

## Facts to get
- Version: chrome://version (ChromeOS build) — an Auto Update Expiration (AUE) reached means no more updates; Google Admin `get-chromeos-device` shows lastSync, osVersion, status.
- Management: chrome://policy shows the applied policies and the last fetch time; `status` DISABLED/DEPROVISIONED in Admin means the device is intentionally stopped.
- Storage/memory: `get-chromeos-device` diskVolumeReports and systemRamFreeReports; on the device Settings → Device → Storage management.
- Diagnostics: the Diagnostics app (launcher → Diagnostics) runs battery, CPU, memory tests; chrome://network#state for connectivity.

## Symptom → ranked causes → fix
| Symptom | Most often | Fix |
|---|---|---|
| Slow | Too many tabs/extensions, low RAM (4 GB devices), Android apps running | Close tabs; chrome://extensions remove; Settings → Apps → restart Android; Powerwash as last resort (data in the cloud) |
| Will not turn on / black screen | Battery drained, EC hung | Charge 30 min; hold Refresh + Power 10 s (EC reset) |
| Stuck on "Enterprise enrollment" or asks to enrol again | Device was deprovisioned or wiped; enrolment requires the enrolment account | Admin: device status; re-enrol with Ctrl+Alt+E at the sign-in screen using the enrolment account |
| Cannot sign in / "This account is not allowed" | User not in an OU permitted for this device, 2SV enforced but not enrolled, account suspended | `google-workspace-admin.get-user` → suspended, isEnforcedIn2Sv vs isEnrolledIn2Sv, orgUnitPath |
| Wi-Fi connects, no internet | Captive portal, certificate-based Wi-Fi with an expired cert (chrome://network#state), proxy policy | Open a plain http page for the portal; for 802.1X ask admin for the cert policy; chrome://policy for proxy |
| Printing | Printer not in the managed printer list, CUPS/IPP unsupported | Admin → Devices → Chrome → Printers; add by IPP address |
| Update stuck | AUE reached, or policy pins a version, or disk full | Version vs AUE date; chrome://policy DeviceTargetVersionPrefix; storage |
| Screen/touchpad | Hardware | Diagnostics app; hardware service |

## Management actions (Google Admin)
- `chromeos-device-action` disable/deprovision only for lost or retired devices and only through escalate-and-record; both stop the device for its user.
- A device with lastSync older than the last policy change is not applying it: have the person sign out and in; verify the device is not in a suspended state.

## Escalate when
No power after an EC reset; hardware test failures; AUE reached and the person needs a device replacement (that is a request, not an incident).
