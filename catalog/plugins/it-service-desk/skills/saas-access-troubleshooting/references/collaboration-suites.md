# Microsoft 365, Google Workspace, Slack — access and licences
- **Licence missing** (Teams/Exchange/Drive not available): `microsoft-365.get-user` assignedLicenses; `google-workspace-admin.get-user` (suspended, orgUnitPath decides services); the fix is a licence request.
- **Group-based access**: SharePoint sites, Teams, shared drives, Slack channels are owned by their owners; the desk does not add people, the owner does. Say who the owner is when it is readable (site/Team owner; Slack channel details).
- **Guest/external**: a guest in Entra or a Slack Connect user has different limits; "cannot see" is often a guest restriction by policy.
- **Deactivated by SCIM/identity sync**: Slack `list-users` deleted=true or a Google suspended account after a lifecycle change; HR/identity, not a product issue.
- **OneDrive/Drive sharing "blocked"**: an external-sharing policy or a DLP rule; the block message names it; the fix is a policy request or an internal share.
