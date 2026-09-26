# Zoom — known causes ranked
| Symptom | Most often | Evidence | Fix |
|---|---|---|---|
| Choppy audio/video | Wi-Fi (packet loss, jitter), then CPU (virtual background, 4K camera, 50 tabs), then a bad headset/driver | `get-meeting-participants-qos`: loss > 2%, jitter > 30 ms → network; CPU > 80% → laptop | Wired or closer to the AP; turn off virtual background/HD; close apps; update client |
| No audio at all | Wrong device selected, OS mic permission denied, another app holds the device | Zoom → Settings → Audio → test; macOS System Settings → Privacy → Microphone; Windows Settings → Privacy → Microphone | Select the device; grant permission; quit the other app; restart |
| Camera black | Permission, another app holds it, a privacy shutter, a virtual camera driver | Same as audio, Camera | Same |
| Cannot record / no cloud recording | Licence (Basic) or setting locked at account level | `get-user` type; `get-user-settings` recording; `get-account-settings` lock | Licence request or admin setting; local recording as fallback |
| Participants stuck in waiting room, "host has another meeting" | Settings, PMI reuse, an old meeting still open | `get-user-settings`; `list-user-meetings` live | End the stale meeting; waiting room setting |
| Calendar integration missing / wrong meeting links | Outlook add-in vs Zoom for Outlook, or Google Workspace add-on not authorized | Which client | Reinstall the add-in from the approved source; re-authorize |
| "Sign in with SSO" fails | Company domain not typed (acme.zoom.us), identity issue | The vanity URL | Use the SSO button with the company domain; then identity skill |
| Screen share black on Mac | Screen Recording permission | System Settings → Privacy → Screen Recording | Grant, quit and reopen Zoom |
| Echo | Two devices in the room in the same meeting, speaker into mic | Ask | One audio source per room; headset |
Cache: Mac `~/Library/Application Support/zoom.us`, Windows `%APPDATA%\Zoom`; uninstall with Zoom's own cleaner (CleanZoom) before reinstalling.
