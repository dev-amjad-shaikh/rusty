---
name: workplace-app-troubleshooting
description: Use for problems in the apps people work in all day — Zoom, Microsoft Teams, Slack, Outlook and Exchange mail and calendar, browsers, Microsoft 365 desktop apps. Separates account from client from service (does web work? is the vendor down?), reads the vendor's own diagnostics when connected (Zoom quality, mailbox rules, service health), and follows the per-app reference for the known causes and fixes.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.list-records, servicenow.get-record, zoom.get-user, zoom.get-user-settings, zoom.list-user-meetings, zoom.list-dashboard-meetings, zoom.get-meeting-participants-qos, zoom.get-account-settings, microsoft-365.get-user, microsoft-365.get-mailbox-settings, microsoft-365.list-inbox-rules, microsoft-365.list-service-health, microsoft-365.list-service-issues, slack.list-users, web.fetch
dependencies: connector:zoom, connector:microsoft-365, connector:slack, tool:search_knowledge
license: MIT
---
    # Workplace application troubleshooting

    ## When to use
    After intake classified the issue as an application, or when a person names Zoom, Teams, Slack, Outlook, a browser or an Office app.

    ## Method
    1. **Three-way split** before any fix — ask, or read:
       - *Service*: is the vendor down? `microsoft-365.list-service-health` (Exchange, Teams, SharePoint); for Zoom and Slack read their status pages with `web.fetch` (status.zoom.us, status.slack.com) when the host is allowed, else ask the person to open it. An open incident ends individual diagnosis: report it.
       - *Account*: does the web version work (outlook.office.com, app.slack.com, zoom.us/wc)? Web works, desktop fails → client. Web fails too → account or service.
       - *Client*: version (each app's About), OS, a recent update, a plugin or extension.
    2. **Read the vendor record** when connected: Zoom `get-user` (licence type, last client version) and `get-user-settings`; for a bad call `list-user-meetings` → `get-meeting-participants-qos` (packet loss > 2%, jitter > 30 ms, latency > 150 ms is the person's network, CPU > 80% is the laptop); Microsoft 365 `get-mailbox-settings` and `list-inbox-rules` for missing or forwarded mail.
    3. **Read the app reference** with `skills.read`: `references/zoom.md`, `references/teams.md`, `references/slack.md`, `references/outlook-exchange.md`, `references/browsers.md`, `references/office-apps.md`. Rank the causes with the evidence from step 2.
    4. **Fix**, least disruptive first: setting → sign out and in → clear the app's cache (each reference names the folder) → update → reinstall. Never delete a mail profile or OST without saying the mail is on the server and will re-download.
    5. **Verify** the original action (join a test meeting, send a test mail, post in a test channel).
    6. **Record** `app:<app>:<person>` with cause and fix. Three people with the same client version and the same failure in a week → file a gap: it is the version, not the people.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    Service, account or client named with the evidence, the fix from the reference applied and verified, the note written.
