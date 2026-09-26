# Outlook and Exchange Online — known causes ranked
| Symptom | Most often | Evidence | Fix |
|---|---|---|---|
| Mail stops arriving in Outlook, web is fine | Cached mode sync stuck, OST corrupt or > 50 GB, a mail profile problem | Web mail has it | Send/Receive → Work Offline toggle; Update Folder; if stuck: new profile (Control Panel → Mail → Profiles; mail re-downloads from the server); Mac: Outlook → Help → Reset (new Outlook) or rebuild the profile |
| Mail disappears or goes to someone else | An inbox rule (moved/deleted/forwarded), a compromised account with a forwarding rule | `microsoft-365.list-inbox-rules`, `get-mailbox-settings` forwarding | Remove the rule; if the person did not create it, security incident: escalate, `revoke-sign-in-sessions`, password reset |
| Password prompt loops | Modern auth disabled in the profile, a stale credential in Credential Manager/Keychain, conditional access | `microsoft-365.list-sign-ins` error codes | Remove saved credentials (Windows Credential Manager "MicrosoftOffice16_Data"; Mac Keychain "Exchange"); sign in again; CA → identity skill |
| Calendar invitations wrong times | Time zone in mailbox settings differs from the OS | `get-mailbox-settings` timeZone | Set the time zone in Outlook/web settings |
| Cannot open shared mailbox / calendar | Permissions or auto-mapping | Ask the owner to share; the desk adds via Exchange admin (a request) | — |
| Search returns nothing | Windows Search index, Outlook index (Mac Spotlight) | Outlook → Search → Indexing status | Rebuild index; for Mac `mdimport` the Outlook profile folder |
| Mailbox full | 50/100 GB limit; large attachments | Web → Settings → Storage | Archive (Online Archive), delete Sent/Deleted large items; the quota is policy |
| Attachments blocked | File type policy | Error names the type | Use OneDrive link |
| Outlook crashes on start | An add-in | Start in safe mode (`outlook.exe /safe`) works → disable add-ins one by one | Remove the add-in |
| Service | Exchange incident | `list-service-issues` | Report |
Profiles/cache: Windows `%LOCALAPPDATA%\Microsoft\Outlook\*.ost`; new Outlook for Windows has no OST (web-based). Mac: `~/Library/Group Containers/UBF8T346G9.Office/Outlook`.
