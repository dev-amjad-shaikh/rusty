# Where the IT desk's vendor documentation lives

The desk's skills were written from the vendors' API documentation. When a
runbook needs a detail the skill's references do not hold — a new field, a
changed endpoint, a platform-specific step — read the vendor's page with
`web.fetch` (the host must be on the egress ceiling; the plugin names them).

| System | Documentation | What it answers |
|---|---|---|
| Microsoft 365 (Entra ID, Exchange, service health) | https://learn.microsoft.com/en-us/graph/api/overview | Users, mailboxes, sign-in, service incidents through Microsoft Graph |
| Microsoft Intune | https://learn.microsoft.com/en-us/graph/api/resources/intune-devices-conceptual | Managed devices, compliance, last check-in |
| Jamf Pro | https://developer.jamf.com/jamf-pro/reference/jamf-pro-api | Mac inventory, management records, FileVault and enrollment state |
| Okta | https://developer.okta.com/docs/reference/core-okta-api/ | Users, factors (MFA), sessions, app assignments |
| Google Workspace Admin | https://developers.google.com/admin-sdk/directory/reference/rest | Users, groups, ChromeOS devices |
| Zoom | https://developers.zoom.us/docs/api/ | Users, meetings, client versions |
| SAP Cloud Identity Services | https://help.sap.com/docs/cloud-identity-services/cloud-identity-services/scim-rest-api | SCIM users and groups |
| Slack | https://api.slack.com/methods | Users, channels, workspace admin reads |

Rules of thumb: read the reference page for the exact operation before
calling it; prefer the vendor's page over a forum answer; note the page's
date — a runbook step older than the page it came from is suspect.
