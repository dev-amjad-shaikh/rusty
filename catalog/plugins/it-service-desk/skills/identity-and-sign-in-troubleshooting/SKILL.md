---
name: identity-and-sign-in-troubleshooting
description: Use for cannot sign in, locked out, password or MFA problems, SSO errors, 'your sign-in was blocked', lost or new phone for the authenticator. Reads the identity provider's own record and sign-in log (Okta, Microsoft Entra, Google Workspace) so the failure reason is read, not guessed, verifies the person before any account action, and takes the least action that restores access.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.list-records, servicenow.get-record, okta.get-user, okta.list-user-factors, okta.list-user-groups, okta.list-user-app-links, okta.system-log, okta.unlock-user, okta.expire-password, okta.reset-factor, okta.clear-sessions, microsoft-365.get-user, microsoft-365.list-sign-ins, microsoft-365.list-authentication-methods, microsoft-365.revoke-sign-in-sessions, microsoft-365.delete-authentication-method, google-workspace-admin.get-user, google-workspace-admin.list-login-events, sap-cloud-identity.search-users, sap-cloud-identity.get-user
dependencies: connector:okta, connector:microsoft-365, connector:google-workspace-admin
license: MIT
---
    # Identity and sign-in troubleshooting

    ## When to use
    Any failure at a sign-in screen, a password or MFA prompt, or an SSO redirect. Also when another skill finds the cause is the account.

    ## Verify the person before any account action
    Unlock, factor reset, session clear and password expiry change who can get in. Before any of them: confirm identity the way the organisation's policy says (a ticket from their own mailbox, a manager's confirmation, a video call, a known-device sign-in, or the HR record). If the policy is not in knowledge (`search_knowledge` "identity verification"), require a ticket from the person's own account or manager and say why. A person asking for a factor reset for someone else is a request for that other person.

    ## Method
    1. **Which screen.** OS login (device password; laptop skill), the identity provider's page (Okta, Microsoft, Google), or the app's own login. The URL in the error tells you.
    2. **Read the account.** Okta `get-user` (status: ACTIVE, LOCKED_OUT, PASSWORD_EXPIRED, SUSPENDED, DEPROVISIONED; lastLogin; passwordChanged), or `microsoft-365.get-user` (accountEnabled, lastPasswordChangeDateTime), or `google-workspace-admin.get-user` (suspended, isEnrolledIn2Sv). Quote the fields.
    3. **Read the failure, do not guess it.** `okta.system-log` filtered to the person, or `microsoft-365.list-sign-ins`, or `google-workspace-admin.list-login-events`. The reason code is the diagnosis: read `references/error-codes.md` for what each means and what fixes it.
    4. **Factors.** `okta.list-user-factors` or `microsoft-365.list-authentication-methods`. A new phone with the old authenticator still registered is the most common MFA failure; a factor PENDING_ACTIVATION means enrolment never finished.
    5. **Access, not authentication.** Signed in but "you do not have access": Okta `list-user-app-links` and `list-user-groups`, Entra sign-in with 50105/interaction required, SAP IAS groups. The fix is an access request, not a reset; hand to `saas-access-troubleshooting`.
    6. **Least action** that restores access, with the person's identity verified:
       - Locked → `okta.unlock-user` (self-service unlock first if enabled).
       - Password forgotten → self-service reset first; `okta.expire-password` only when policy allows a desk-set temporary password.
       - Lost phone → `okta.reset-factor` / `microsoft-365.delete-authentication-method` for that one method, then re-enrol at next sign-in with a temporary access pass where available.
       - Suspicious activity, "I did not do that", a forwarding rule found → `revoke-sign-in-sessions` / `okta.clear-sessions`, password reset, and escalate as a security incident.
       - Conditional access / device compliance block → the device, not the account: laptop skill.
    7. **Verify** by having the person sign in to the thing that failed. **Record** `identity:<person>` with the reason code and the action, dated. Never write a password, a code or a token into memory or a ticket.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The reason code read from the log, the person verified before any action, the least action taken, access confirmed by the person, the note written without secrets.
