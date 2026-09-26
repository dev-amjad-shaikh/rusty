---
name: saas-access-troubleshooting
description: Use when a person can sign in but cannot see or do something in a business system — ServiceNow, Salesforce, Workday, SAP, Slack, Google Workspace, Microsoft 365 — or needs access they do not have: wrong role, missing licence, a record they cannot open, a report that is empty for them. Separates permission from data from product behaviour, reads the system's own record of the person where connected, and turns a permission gap into a proper access request instead of a workaround.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.list-records, servicenow.get-record, servicenow.aggregate, servicenow.list-catalog-items, servicenow.order-catalog-item, servicenow.create-record, salesforce.soql-query, salesforce.list-objects, okta.list-user-groups, okta.list-user-app-links, microsoft-365.get-user, sap-cloud-identity.search-users, sap-cloud-identity.get-user, sap-cloud-identity.list-groups, slack.list-users, google-workspace-admin.get-user
dependencies: connector:servicenow, connector:salesforce, connector:okta, connector:sap-cloud-identity
license: MIT
---
    # SaaS access troubleshooting

    ## When to use
    The person is signed in (if not, identity skill first) and cannot see, open, edit, run or receive something in a business system, or asks for access.

    ## Method
    1. **Name the thing precisely.** Which system, which object or table or module, which record or report, what they tried, what it said. "Insufficient privileges", "record not found", "you do not have permission", a blank list and a 403 are four different causes.
    2. **Permission or data?** Ask a control question: can a colleague with the same role see it? Can the person see one record of the same type? Yes to both → data (the record is private, deleted, in another org/company/tenant). No → permission (role, profile, licence, group, sharing rule).
    3. **Read the system's record of the person** where a connector exists, and quote it:
       - ServiceNow: `servicenow.list-records` on sys_user (active, roles via sys_user_has_role), and the record they cannot open with `servicenow.get-record` (its domain/company, ACL error text).
       - Salesforce: `salesforce.soql-query` on User (IsActive, ProfileId, UserRoleId, UserType), PermissionSetAssignment, and the record (OwnerId, sharing). `references/salesforce.md` has the queries.
       - Workday: the worker's security groups are visible only in Workday (or the HR connector when present); ask the person for the exact task name and error.
       - SAP: `sap-cloud-identity.get-user` groups (application authorizations); S/4 roles are in the backend: escalate with the transaction code and the SU53-style message.
       - Identity provider: Okta `list-user-app-links` / `list-user-groups`, Entra `get-user` licences. No app link → not assigned; that is the cause.
    4. **Read the product reference** with `skills.read`: `references/servicenow.md`, `references/salesforce.md`, `references/workday.md`, `references/sap.md`, `references/collaboration-suites.md` for the permission model and the known traps (Salesforce sharing rules, ServiceNow domain separation, Workday business process security, SAP authorization objects).
    5. **Resolve**:
       - Permission → an access request in the right place: ServiceNow catalog (`list-catalog-items` then `order-catalog-item` with the person's yes), or the system's own request path named in the reference. Never grant, never suggest sharing another person's credentials, never "just make them admin".
       - Data → tell the person what the record's state is (private, another company, archived) and who owns it; the fix is with the owner.
       - Product behaviour (a filter, a list view, a default) → the reference's fix; verify with them.
    6. **Verify** and **record** `access:<system>:<person>` with the cause and what was requested; if a request was raised, its number. If the same missing role comes up for three people in a team, file a gap: the role should be in the onboarding profile.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    Permission, data or behaviour named with the system's own record quoted, the right request raised or the fix verified, the note written with the request number.
