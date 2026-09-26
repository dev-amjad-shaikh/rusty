# Slack — known causes ranked
| Symptom | Most often | Fix |
|---|---|---|
| Cannot sign in | SSO (workspace requires SAML) — identity skill; wrong workspace URL | Use acme.slack.com; identity skill; `slack.list-users` shows if the account exists and is deactivated |
| Messages not sending / "connecting" | Proxy or gateway blocking WebSocket (wss://), VPN, corporate firewall | Works on a hotspot → network team with the domains *.slack.com, *.slack-edge.com, wss://wss-primary.slack.com |
| Notifications missing | OS notification permission, Slack pause/DND, channel muted, "notify me about" setting | Slack → Preferences → Notifications → Troubleshoot notifications (it runs its own diagnostic) |
| Huddle audio/video | Same as Zoom: device permission, network | Test call in Preferences → Audio & video |
| App slow / high memory | Many workspaces, large channels, the desktop app version | Update; sign out of unused workspaces; Slack → Help → Troubleshooting → Clear cache and restart |
| Cannot see a channel or file | Private channel, guest account, retention deleted it | Ask the owner to add; retention is policy |
| Deactivated | Offboarding or identity sync (SCIM) removed the user | `slack.list-users` deleted flag; HR/identity check |
Cache: Slack → Help → Troubleshooting → Clear cache and restart (safe); Reset app data (signs out).
