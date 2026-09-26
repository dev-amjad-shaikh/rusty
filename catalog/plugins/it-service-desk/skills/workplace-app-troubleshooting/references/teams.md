# Microsoft Teams — known causes ranked
| Symptom | Most often | Fix |
|---|---|---|
| Sign-in loop / "trouble signing in" | Work account token (WAM) on Windows, Keychain on Mac, time skew, conditional access | Windows: Settings → Accounts → Access work or school → reconnect; `dsregcmd /status`; Mac: Keychain → delete Microsoft entries after backing up; check clock; identity skill for CA (`microsoft-365.list-sign-ins` error 53003) |
| Calls drop / bad quality | Same physics as Zoom; also the Teams "media optimization" missing in a VDI | Teams → Settings → Devices → test call; QoS in the Teams admin centre (desk) | Wired; close apps; new Teams client |
| Notifications missing | Focus assist/Do Not Disturb, status set, channel notification settings | Settings → Notifications | As found |
| Chat history / files not loading | Client cache | Quit Teams; delete cache: Windows `%APPDATA%\Microsoft\Teams` (classic) or `%LOCALAPPDATA%\Packages\MSTeams_8wekyb3d8bbwe\LocalCache` (new); Mac `~/Library/Group Containers/UBF8T346G9.com.microsoft.teams`, `~/Library/Containers/com.microsoft.teams2` | Reopen |
| Cannot join external meeting | Guest access / federation policy, browser join blocked | Join from the browser (Edge/Chrome) as a test | Policy request to the Teams admin |
| Add-in missing in Outlook | Teams Meeting add-in not loaded | Outlook → File → Options → Add-ins → COM add-ins → tick; run Teams once as the user first |
| Service | Exchange/Teams incident | `microsoft-365.list-service-issues` | Report |
