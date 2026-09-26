#!/usr/bin/env python3
"""Generate catalog/plugins/it-service-desk: connector manifests written from
the vendors' documented REST APIs, and method skills with per-platform
reference files. Manifests carry no hash; the server seals them at install."""
import json, os, textwrap

ROOT = os.path.join(os.path.dirname(__file__), "..", "..", "catalog", "plugins", "it-service-desk")

def spec(title, props, required, order0=None):
    return {"$schema": "http://json-schema.org/draft-07/schema#", "title": title, "type": "object",
            "required": required, "additionalProperties": False, "properties": props}

def secret(title, order, hidden=False, desc=None):
    p = {"type": "string", "title": title, "rusty_secret": True, "rusty_order": order}
    if hidden: p["rusty_hidden"] = True
    if desc: p["description"] = desc
    return p

def plain(title, order, desc=None, pattern=None):
    p = {"type": "string", "title": title, "rusty_order": order}
    if desc: p["description"] = desc
    if pattern: p["rusty_pattern_descriptor"] = pattern
    return p

def creds_client(title, desc):
    return {"type": "object", "title": title, "description": desc, "required": ["client_id", "client_secret"],
            "additionalProperties": False, "rusty_order": 1,
            "properties": {"client_id": secret("Client ID", 0), "client_secret": secret("Client secret", 1)}}

def creds_oauth_app(title, desc):
    # The shape the Slack connector uses: the person types the app's id and
    # secret; the tokens are issued when they authorize and never typed.
    return {"type": "object", "title": title, "description": desc, "required": ["client_id", "client_secret"],
            "additionalProperties": False, "rusty_order": 1,
            "properties": {"client_id": secret("Client ID", 0), "client_secret": secret("Client secret", 1),
                           "access_token": secret("Access token", 2, hidden=True), "refresh_token": secret("Refresh token", 3, hidden=True),
                           "expires_at": {"type": "string", "title": "Token expires", "rusty_hidden": True, "rusty_order": 4}}}

def op(name, description, method, path, effect, props=None, required=None, auth=None):
    o = {"name": name, "description": description, "method": method, "path": path, "effect": effect,
         "params_schema": {"type": "object"}}
    if props:
        o["params_schema"]["properties"] = props
        if required: o["params_schema"]["required"] = required
    if auth: o["auth"] = auth
    return o

def P(desc, typ="string", **kw):
    d = {"type": typ, "description": desc}; d.update(kw); return d

BEARER = [{"style": "bearer", "token": "{credentials.access_token}"}]

def manifest(id, display, description, doc, base, connection, ops, check, authorization=None):
    m = {"id": id, "version": "1", "display_name": display, "description": description, "documentation_url": doc,
         "base_url": base, "connection_specification": connection, "operations": ops, "check": check}
    if authorization: m["authorization"] = authorization
    return m

# ───────────────────────── connectors ─────────────────────────
connectors = {}

# Jamf Pro — API clients with OAuth client credentials (Settings → API roles and clients).
jamf_auth = [{"style": "oauth2_client_credentials", "token_url": "https://{instance_host}/api/oauth/token",
              "client_id_template": "{credentials.client_id}", "client_secret_template": "{credentials.client_secret}", "scope_template": None}]
connectors["jamf-pro"] = manifest(
    "jamf-pro", "Jamf Pro", "Jamf Pro API: what Jamf knows about a Mac or iPhone — inventory, last check-in, OS, disk, FileVault, installed apps, MDM profiles — and the remote commands a service desk sends (inventory update, restart, lock).",
    "https://developer.jamf.com/jamf-pro/reference/jamf-pro-api", "https://{instance_host}",
    spec("Jamf Pro Connection", {"instance_host": plain("Jamf Pro host", 0, "The tenant's Jamf Pro address.", "acme.jamfcloud.com"),
                                  "credentials": creds_client("API client", "Settings → System → API roles and clients. The role needs Read Computers, Read Mobile Devices, Read Users, Send Computer Remote Command to Update Inventory, Send Computer Restart Command.")},
         ["instance_host", "credentials"]),
    [op("check-connection", "Verify the API client is accepted and the server answers.", "GET", "/api/v1/jamf-pro-version", "read_only", auth=jamf_auth),
     op("search-computers", "Find computers by user, name, serial or asset tag (RSQL filter). Returns id, name, serial, last contact, OS version, user.", "GET", "/api/v1/computers-inventory", "read_only",
        {"filter": P('RSQL filter, e.g. userAndLocation.username=="jdoe" or general.name=="MBP-1234" or hardware.serialNumber=="C02XX"'), "section": P("Sections to include, comma-separated: GENERAL,HARDWARE,USER_AND_LOCATION,OPERATING_SYSTEM,STORAGE,SECURITY", default="GENERAL,HARDWARE,USER_AND_LOCATION,OPERATING_SYSTEM"), "page-size": P("Rows per page", "integer", default=20)}, auth=jamf_auth),
     op("get-computer", "Everything Jamf holds on one computer: hardware, OS, storage (free space), security (FileVault, Gatekeeper, firewall), installed applications, configuration profiles, last check-in and last inventory update.", "GET", "/api/v1/computers-inventory-detail/{id}", "read_only",
        {"id": P("The computer's Jamf id from search-computers")}, ["id"], auth=jamf_auth),
     op("get-computer-mdm-capability", "Whether the computer can receive MDM commands right now and which capabilities it reports.", "GET", "/api/v1/computers-inventory/{id}/mdm-capability", "read_only",
        {"id": P("The computer's Jamf id")}, ["id"], auth=jamf_auth),
     op("search-mobile-devices", "Find iPhones and iPads by user, name or serial.", "GET", "/api/v2/mobile-devices/detail", "read_only",
        {"filter": P('RSQL filter, e.g. userAndLocation.username=="jdoe" or serialNumber=="F9FXX"'), "page-size": P("Rows per page", "integer", default=20)}, auth=jamf_auth),
     op("update-inventory", "Ask the Mac to send a fresh inventory now (the person must be online). Safe to repeat.", "POST", "/api/v1/jamf-management-framework/redeploy/{id}", "idempotent",
        {"id": P("The computer's Jamf id")}, ["id"], auth=jamf_auth),
     op("send-mdm-command", "Send one MDM command to a device: RestartDevice, ShutDownDevice, DeviceLock (with a message), EnableRemoteDesktop, DisableRemoteDesktop. Lock and shutdown interrupt the person; confirm with them first.", "POST", "/api/v2/mdm/commands", "compensatable",
        {"clientData": P("[{\"managementId\": \"<device management id from get-computer>\"}]", "array"), "commandData": P("{\"commandType\": \"RestartDevice\"} or {\"commandType\": \"DeviceLock\", \"message\": \"...\"}", "object")}, ["clientData", "commandData"], auth=jamf_auth)],
    "check-connection")

# Microsoft Graph, two connectors on one API: devices (Intune) and people (Entra ID, Exchange settings, service health).
graph_auth = [{"style": "oauth2_client_credentials", "token_url": "https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token",
               "client_id_template": "{credentials.client_id}", "client_secret_template": "{credentials.client_secret}", "scope_template": "https://graph.microsoft.com/.default"}]
graph_conn = lambda title, perms: spec(title, {"tenant_id": plain("Entra tenant ID", 0, "Directory (tenant) ID from the app registration overview.", "72f988bf-86f1-41af-91ab-2d7cd011db47"),
                                              "credentials": creds_client("App registration", "An app registration with application permissions, admin-consented: " + perms)}, ["tenant_id", "credentials"])
connectors["microsoft-intune"] = manifest(
    "microsoft-intune", "Microsoft Intune", "Microsoft Graph, device management: what Intune knows about a Windows, macOS, iOS or Android device — compliance, last sync, OS, storage, enrollment — and the remote actions a desk takes (sync, restart, rotate the BitLocker key).",
    "https://learn.microsoft.com/en-us/graph/api/resources/intune-devices-conceptual", "https://graph.microsoft.com",
    graph_conn("Intune Connection", "DeviceManagementManagedDevices.Read.All, DeviceManagementManagedDevices.PrivilegedOperations.All, DeviceManagementConfiguration.Read.All."),
    [op("check-connection", "Verify the app registration is accepted.", "GET", "/v1.0/deviceManagement/managedDevices?$top=1", "read_only", auth=graph_auth),
     op("search-devices", "Find managed devices by user, name or serial. Returns id, deviceName, userPrincipalName, operatingSystem, osVersion, complianceState, lastSyncDateTime, freeStorageSpaceInBytes.", "GET", "/v1.0/deviceManagement/managedDevices", "read_only",
        {"$filter": P("OData filter, e.g. userPrincipalName eq 'jdoe@acme.com' or deviceName eq 'ACME-LT-1234' or serialNumber eq 'PF3XX'"), "$select": P("Fields to return", default="id,deviceName,userPrincipalName,operatingSystem,osVersion,complianceState,lastSyncDateTime,freeStorageSpaceInBytes,totalStorageSpaceInBytes,enrolledDateTime,managementAgent,isEncrypted,jailBroken,model,manufacturer,serialNumber"), "$top": P("Rows", "integer", default=20)}, auth=graph_auth),
     op("get-device", "One device in full.", "GET", "/v1.0/deviceManagement/managedDevices/{id}", "read_only", {"id": P("The managed device id")}, ["id"], auth=graph_auth),
     op("get-device-compliance", "Which compliance policies the device fails and why (setting by setting).", "GET", "/v1.0/deviceManagement/managedDevices/{id}/deviceCompliancePolicyStates", "read_only", {"id": P("The managed device id")}, ["id"], auth=graph_auth),
     op("get-device-configuration-states", "Which configuration profiles applied, pending or errored on the device.", "GET", "/v1.0/deviceManagement/managedDevices/{id}/deviceConfigurationStates", "read_only", {"id": P("The managed device id")}, ["id"], auth=graph_auth),
     op("sync-device", "Ask the device to check in with Intune now. Safe to repeat.", "POST", "/v1.0/deviceManagement/managedDevices/{id}/syncDevice", "idempotent", {"id": P("The managed device id")}, ["id"], auth=graph_auth),
     op("restart-device", "Restart the device remotely. It interrupts the person; confirm with them first.", "POST", "/v1.0/deviceManagement/managedDevices/{id}/rebootNow", "compensatable", {"id": P("The managed device id")}, ["id"], auth=graph_auth),
     op("rotate-bitlocker-keys", "Rotate the BitLocker recovery key (Windows). Use after a recovery key was read out to a person.", "POST", "/v1.0/deviceManagement/managedDevices/{id}/rotateBitLockerKeys", "compensatable", {"id": P("The managed device id")}, ["id"], auth=graph_auth)],
    "check-connection")

connectors["microsoft-365"] = manifest(
    "microsoft-365", "Microsoft 365 (Entra ID, Exchange, service health)", "Microsoft Graph, people and services: a person's account state and licences, their sign-in attempts and why they failed, registered MFA methods, mailbox settings and rules, Teams and Exchange service health, and the account actions a desk takes (revoke sessions, reset an MFA method).",
    "https://learn.microsoft.com/en-us/graph/api/overview", "https://graph.microsoft.com",
    graph_conn("Microsoft 365 Connection", "User.Read.All, AuditLog.Read.All, UserAuthenticationMethod.Read.All, MailboxSettings.Read, ServiceHealth.Read.All, User.RevokeSessions.All, Organization.Read.All."),
    [op("check-connection", "Verify the app registration is accepted.", "GET", "/v1.0/organization", "read_only", auth=graph_auth),
     op("get-user", "A person's account: display name, UPN, accountEnabled, last password change, licences (assignedLicenses), usage location, department, manager.", "GET", "/v1.0/users/{upn}", "read_only",
        {"upn": P("The user principal name or object id, e.g. jdoe@acme.com"), "$select": P("Fields", default="id,displayName,userPrincipalName,mail,accountEnabled,lastPasswordChangeDateTime,assignedLicenses,usageLocation,department,jobTitle,createdDateTime")}, ["upn"], auth=graph_auth),
     op("list-sign-ins", "Recent sign-in attempts for a person with the result: status.errorCode (0 = success, 50126 wrong password, 50053 locked, 50074 MFA required, 53003 blocked by Conditional Access, 50076 MFA needed), the app, device, location, and conditionalAccessStatus.", "GET", "/v1.0/auditLogs/signIns", "read_only",
        {"$filter": P("e.g. userPrincipalName eq 'jdoe@acme.com'"), "$top": P("Rows", "integer", default=25), "$orderby": P("Order", default="createdDateTime desc")}, ["$filter"], auth=graph_auth),
     op("list-authentication-methods", "Which MFA methods the person has registered: Authenticator app, phone, FIDO2 key, Windows Hello, temporary access pass.", "GET", "/v1.0/users/{upn}/authentication/methods", "read_only", {"upn": P("UPN or id")}, ["upn"], auth=graph_auth),
     op("get-mailbox-settings", "The person's mailbox settings: automatic replies, time zone, delegate forwarding is visible here.", "GET", "/v1.0/users/{upn}/mailboxSettings", "read_only", {"upn": P("UPN or id")}, ["upn"], auth=graph_auth),
     op("list-inbox-rules", "The person's inbox rules; a forwarding or delete-all rule explains 'my mail disappears'.", "GET", "/v1.0/users/{upn}/mailFolders/inbox/messageRules", "read_only", {"upn": P("UPN or id")}, ["upn"], auth=graph_auth),
     op("list-service-health", "Current health of Exchange Online, Teams, SharePoint, Intune and the rest: is the problem Microsoft's?", "GET", "/v1.0/admin/serviceAnnouncement/healthOverviews", "read_only", auth=graph_auth),
     op("list-service-issues", "Open service incidents with impact descriptions.", "GET", "/v1.0/admin/serviceAnnouncement/issues", "read_only", {"$filter": P("e.g. isResolved eq false", default="isResolved eq false")}, auth=graph_auth),
     op("revoke-sign-in-sessions", "Sign the person out everywhere (refresh tokens invalidated). Used after a compromise or a stuck session; they sign in again on every device.", "POST", "/v1.0/users/{upn}/revokeSignInSessions", "compensatable", {"upn": P("UPN or id")}, ["upn"], auth=graph_auth),
     op("delete-authentication-method", "Remove one registered MFA method (a lost phone). The person re-registers at next sign-in; needs a verified identity first.", "DELETE", "/v1.0/users/{upn}/authentication/microsoftAuthenticatorMethods/{method_id}", "irreversible", {"upn": P("UPN or id"), "method_id": P("The method id from list-authentication-methods")}, ["upn", "method_id"], auth=graph_auth)],
    "check-connection")

# Okta — SSWS API token in the Authorization header.
okta_auth = [{"style": "header", "name": "Authorization", "value_template": "SSWS {credentials.api_token}"}]
connectors["okta"] = manifest(
    "okta", "Okta", "Okta Management API: a person's account status (ACTIVE, LOCKED_OUT, PASSWORD_EXPIRED, SUSPENDED), their enrolled MFA factors, group memberships and app assignments, the system log of their sign-in attempts with the failure reason, and the lifecycle actions a desk takes (unlock, expire password, reset a factor, clear sessions).",
    "https://developer.okta.com/docs/reference/core-okta-api/", "https://{org_host}",
    spec("Okta Connection", {"org_host": plain("Okta org host", 0, "The org's address, without https://.", "acme.okta.com"),
                             "credentials": {"type": "object", "title": "API token", "description": "Security → API → Tokens. A read-only admin token is enough for diagnosis; unlock and factor reset need Help Desk Administrator.", "required": ["api_token"], "additionalProperties": False, "rusty_order": 1, "properties": {"api_token": secret("SSWS API token", 0)}}},
         ["org_host", "credentials"]),
    [op("check-connection", "Verify the token is accepted.", "GET", "/api/v1/org", "read_only", auth=okta_auth),
     op("get-user", "One person by login, email or id: status, lastLogin, passwordChanged, statusChanged, profile.", "GET", "/api/v1/users/{login}", "read_only", {"login": P("Login (usually the email), or the Okta user id")}, ["login"], auth=okta_auth),
     op("list-user-factors", "Enrolled MFA factors: factorType (push, sms, token:software:totp, webauthn), provider, status (ACTIVE, PENDING_ACTIVATION), created.", "GET", "/api/v1/users/{id}/factors", "read_only", {"id": P("Okta user id or login")}, ["id"], auth=okta_auth),
     op("list-user-groups", "Groups the person is in — app access is usually granted by group.", "GET", "/api/v1/users/{id}/groups", "read_only", {"id": P("Okta user id or login")}, ["id"], auth=okta_auth),
     op("list-user-app-links", "Apps assigned to the person (what their dashboard shows).", "GET", "/api/v1/users/{id}/appLinks", "read_only", {"id": P("Okta user id or login")}, ["id"], auth=okta_auth),
     op("system-log", "Sign-in and admin events. outcome.reason names the failure: INVALID_CREDENTIALS, LOCKED_OUT, VERIFICATION_ERROR (MFA), DENIED_BY_POLICY, USER_NOT_ASSIGNED (no app access).", "GET", "/api/v1/logs", "read_only",
        {"filter": P('e.g. actor.alternateId eq "jdoe@acme.com" or target.alternateId eq "jdoe@acme.com"'), "q": P("Free-text, e.g. the app name"), "since": P("ISO-8601 start, e.g. 2026-09-18T00:00:00Z"), "limit": P("Rows", "integer", default=50), "sortOrder": P("ASCENDING or DESCENDING", default="DESCENDING")}, auth=okta_auth),
     op("unlock-user", "Unlock a LOCKED_OUT account. Safe to repeat; verify the person's identity first.", "POST", "/api/v1/users/{id}/lifecycle/unlock", "idempotent", {"id": P("Okta user id")}, ["id"], auth=okta_auth),
     op("expire-password", "Force a password change at next sign-in.", "POST", "/api/v1/users/{id}/lifecycle/expire_password", "compensatable", {"id": P("Okta user id")}, ["id"], auth=okta_auth),
     op("reset-factor", "Remove one enrolled MFA factor (lost phone); the person re-enrols at next sign-in. Verify identity first; this cannot be undone.", "DELETE", "/api/v1/users/{id}/factors/{factor_id}", "irreversible", {"id": P("Okta user id"), "factor_id": P("Factor id from list-user-factors")}, ["id", "factor_id"], auth=okta_auth),
     op("clear-sessions", "Sign the person out of every Okta session (stuck or compromised session).", "DELETE", "/api/v1/users/{id}/sessions", "irreversible", {"id": P("Okta user id"), "oauthTokens": P("Also revoke OAuth tokens", "boolean", default=False)}, ["id"], auth=okta_auth)],
    "check-connection")

# Google Workspace Admin — OAuth app (authorization code); tokens issued at authorize time.
google_authz = {"authorize_url": "https://accounts.google.com/o/oauth2/v2/auth", "token_url": "https://oauth2.googleapis.com/token",
                "scopes": "https://www.googleapis.com/auth/admin.directory.device.chromeos https://www.googleapis.com/auth/admin.directory.user.readonly https://www.googleapis.com/auth/admin.reports.audit.readonly",
                "client_id": "{credentials.client_id}", "client_secret": "{credentials.client_secret}", "extra_params": {"access_type": "offline", "prompt": "consent"}}
connectors["google-workspace-admin"] = manifest(
    "google-workspace-admin", "Google Workspace Admin", "Admin SDK: ChromeOS devices (last sync, OS version, status, the signed-in user), Workspace user accounts (suspended, 2SV enrolled, last login), the login audit with failure reasons, and the ChromeOS actions a desk takes (reenrol wipe, disable).",
    "https://developers.google.com/admin-sdk/directory/reference/rest", "https://admin.googleapis.com",
    spec("Google Workspace Connection", {"credentials": creds_oauth_app("OAuth client", "Google Cloud console → APIs & Services → Credentials, an OAuth client for a Workspace super admin or a delegated admin with the Chrome OS and Users privileges. The tokens are issued when you authorize.")}, ["credentials"]),
    [op("check-connection", "Verify the authorization is accepted.", "GET", "/admin/directory/v1/users?customer=my_customer&maxResults=1", "read_only", auth=BEARER),
     op("search-chromeos-devices", "Find ChromeOS devices by user, serial or asset id. Returns deviceId, serialNumber, status, lastSync, osVersion, annotatedUser, recentUsers, bootMode.", "GET", "/admin/directory/v1/customer/my_customer/devices/chromeos", "read_only",
        {"query": P("e.g. user:jdoe@acme.com or id:5CD1234 or asset_id:ACME-123"), "projection": P("BASIC or FULL", default="FULL"), "maxResults": P("Rows", "integer", default=20)}, auth=BEARER),
     op("get-chromeos-device", "One ChromeOS device in full: activeTimeRanges, cpuStatusReports, diskVolumeReports, systemRamFreeReports, lastKnownNetwork.", "GET", "/admin/directory/v1/customer/my_customer/devices/chromeos/{deviceId}", "read_only", {"deviceId": P("The device id from search-chromeos-devices")}, ["deviceId"], auth=BEARER),
     op("get-user", "A Workspace account: suspended, suspensionReason, isEnrolledIn2Sv, isEnforcedIn2Sv, lastLoginTime, orgUnitPath, changePasswordAtNextLogin.", "GET", "/admin/directory/v1/users/{userKey}", "read_only", {"userKey": P("Primary email or user id")}, ["userKey"], auth=BEARER),
     op("list-login-events", "Login audit for a person: login_success, login_failure (login_failure_type: login_failure_invalid_password, login_failure_2sv_failure, login_failure_account_disabled), suspicious_login, account_disabled_*.", "GET", "/admin/reports/v1/activity/users/{userKey}/applications/login", "read_only",
        {"userKey": P("Primary email or 'all'"), "maxResults": P("Rows", "integer", default=50), "startTime": P("RFC 3339 start, e.g. 2026-09-18T00:00:00Z")}, ["userKey"], auth=BEARER),
     op("chromeos-device-action", "Act on a ChromeOS device: deprovision (with deprovisionReason) or disable. Both stop the device for its user; confirm with the owner first. Re-enrolment needs the device in hand.", "POST", "/admin/directory/v1/customer/my_customer/devices/chromeos/{resourceId}/action", "irreversible",
        {"resourceId": P("The device id"), "action": P("deprovision or disable"), "deprovisionReason": P("same_model_replacement, different_model_replacement, retiring_device, upgrade_transfer")}, ["resourceId", "action"], auth=BEARER)],
    "check-connection", google_authz)

# Zoom — OAuth app (authorization code).
zoom_authz = {"authorize_url": "https://zoom.us/oauth/authorize", "token_url": "https://zoom.us/oauth/token",
              "scopes": "user:read:admin dashboard_meetings:read:admin dashboard_home:read:admin account:read:admin", "client_id": "{credentials.client_id}", "client_secret": "{credentials.client_secret}", "extra_params": {}}
connectors["zoom"] = manifest(
    "zoom", "Zoom", "Zoom API: a person's Zoom account (type, licence, status, client version, last login), their settings (audio, recording, waiting room), meeting quality per participant (audio/video/screen-share QoS, CPU, network), and the meetings they host.",
    "https://developers.zoom.us/docs/api/", "https://api.zoom.us",
    spec("Zoom Connection", {"credentials": creds_oauth_app("Zoom OAuth app", "marketplace.zoom.us → Build App → General app (user-managed or account-level). Scopes: user:read:admin, dashboard_meetings:read:admin, account:read:admin. The tokens are issued when you authorize.")}, ["credentials"]),
    [op("check-connection", "Verify the authorization is accepted.", "GET", "/v2/users/me", "read_only", auth=BEARER),
     op("get-user", "A person's Zoom account: type (Basic/Licensed), status, last_login_time, last_client_version, verified, dept, pmi, timezone.", "GET", "/v2/users/{userId}", "read_only", {"userId": P("Email or user id")}, ["userId"], auth=BEARER),
     op("get-user-settings", "The person's settings: audio type, join before host, waiting room, recording, in-meeting features. Explains 'I cannot record' and 'my participants are stuck in the waiting room'.", "GET", "/v2/users/{userId}/settings", "read_only", {"userId": P("Email or user id")}, ["userId"], auth=BEARER),
     op("list-user-meetings", "Meetings the person hosts: upcoming, scheduled, live, previous.", "GET", "/v2/users/{userId}/meetings", "read_only", {"userId": P("Email or user id"), "type": P("scheduled, live, upcoming, previous_meetings", default="previous_meetings"), "page_size": P("Rows", "integer", default=20)}, ["userId"], auth=BEARER),
     op("list-dashboard-meetings", "Meetings on the account in a window (Dashboard; Business plan or higher). Gives the meeting uuid for participant quality.", "GET", "/v2/metrics/meetings", "read_only", {"from": P("Start date YYYY-MM-DD"), "to": P("End date YYYY-MM-DD"), "type": P("past, pastOne, live", default="past"), "page_size": P("Rows", "integer", default=30)}, ["from", "to"], auth=BEARER),
     op("get-meeting-participants-qos", "Per-participant quality for one meeting: audio and video bitrate, latency, jitter, packet loss, CPU, network type, client version. The evidence behind 'Zoom was choppy'.", "GET", "/v2/metrics/meetings/{meetingId}/participants/qos", "read_only", {"meetingId": P("Meeting id or uuid (double-encode a uuid with / in it)"), "type": P("past or live", default="past"), "page_size": P("Rows", "integer", default=10)}, ["meetingId"], auth=BEARER),
     op("get-account-settings", "Account-level settings and locks — a setting locked here cannot be changed by the person.", "GET", "/v2/accounts/me/settings", "read_only", auth=BEARER)],
    "check-connection", zoom_authz)

# SAP Cloud Identity Services — SCIM 2.0 with basic auth (system client id/secret).
sap_ias_auth = [{"style": "basic", "username": "{credentials.client_id}", "password": "{credentials.client_secret}"}]
connectors["sap-cloud-identity"] = manifest(
    "sap-cloud-identity", "SAP Cloud Identity Services", "Identity Authentication (IAS) SCIM API: a person's SAP identity — active, locked, last login, email verified, groups (which grant SAP application access) — and the corrections a desk makes (activate, unlock).",
    "https://help.sap.com/docs/cloud-identity-services/cloud-identity-services/scim-rest-api", "https://{tenant_host}",
    spec("SAP Cloud Identity Connection", {"tenant_host": plain("IAS tenant host", 0, "The tenant's address, without https://.", "acme.accounts.ondemand.com"),
                                           "credentials": creds_client("System client", "Administration Console → Applications & Resources → Administrators → Add System, with Manage Users and Read Users authorizations.")}, ["tenant_host", "credentials"]),
    [op("check-connection", "Verify the system client is accepted.", "GET", "/scim/ServiceProviderConfig", "read_only", auth=sap_ias_auth),
     op("search-users", "Find a person by user name or email (SCIM filter).", "GET", "/scim/Users", "read_only", {"filter": P('e.g. userName eq "jdoe" or emails.value eq "jdoe@acme.com"'), "count": P("Rows", "integer", default=10)}, ["filter"], auth=sap_ias_auth),
     op("get-user", "One identity in full: active, urn:ietf:params:scim:schemas:extension:sap:2.0:User (loginTime, passwordStatus, mailVerified, status), groups.", "GET", "/scim/Users/{id}", "read_only", {"id": P("SCIM user id")}, ["id"], auth=sap_ias_auth),
     op("list-groups", "Groups (application authorizations) with their members.", "GET", "/scim/Groups", "read_only", {"filter": P('e.g. displayName eq "S4_Finance_Users"'), "count": P("Rows", "integer", default=20)}, auth=sap_ias_auth),
     op("patch-user", "Correct an identity: set active true/false, or unlock (passwordStatus). Confirm with the person first.", "PATCH", "/scim/Users/{id}", "compensatable",
        {"id": P("SCIM user id"), "schemas": P('["urn:ietf:params:scim:api:messages:2.0:PatchOp"]', "array"), "Operations": P('[{"op":"replace","path":"active","value":true}]', "array")}, ["id", "schemas", "Operations"], auth=sap_ias_auth)],
    "check-connection")

# ───────────────────────── skills ─────────────────────────
def skill(name, description, tools, deps, body, refs):
    fm = f"---\nname: {name}\ndescription: {description}\nallowed-tools: {', '.join(tools)}\n"
    if deps: fm += f"dependencies: {', '.join(deps)}\n"
    fm += "license: MIT\n---\n"
    return fm + textwrap.dedent(body).lstrip("\n"), {k: textwrap.dedent(v).lstrip("\n") for k, v in refs.items()}

COMMON = ["search_knowledge", "memory.recall", "memory.remember", "skills.read", "gaps.file", "servicenow.list-records", "servicenow.get-record"]
GAP = """
## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.
"""

skills = {}

skills["it-issue-intake"] = skill(
    "it-issue-intake",
    "Use at the start of every IT support conversation: turn 'my laptop is broken' into a classified issue with the facts a fix needs — who, which device, since when, what changed, what exactly fails — and route it to the right method skill (laptop, network, app, sign-in, SaaS access) or straight to escalation.",
    COMMON + ["servicenow.aggregate", "okta.get-user", "microsoft-365.get-user", "microsoft-intune.search-devices", "jamf-pro.search-computers", "google-workspace-admin.get-user"],
    ["connector:servicenow"],
    """
    # IT issue intake

    ## When to use
    The first turn of any IT support request, before any fix is suggested. Also when a conversation drifts into a second problem.

    ## Method
    1. **Recall.** `memory.recall` with the person's name or email and with the device name if given. An open note from a previous conversation changes the first question: continue it rather than restart.
    2. **Gather the six facts.** Ask only for what is missing; one message, not six.
       - Who: the person and how they can be reached if the session drops.
       - Device: make, OS and version, managed or personal, hostname or serial if visible (Mac: Apple menu → About This Mac; Windows: Settings → System → About; ChromeOS: chrome://version; Linux: `hostnamectl`).
       - Since when, and whether it ever worked.
       - What changed just before: update, new network, new password, travel, new app, new peripheral.
       - The exact failure: the error text verbatim, a screenshot's words, what they clicked and what happened instead.
       - Blast radius: only them, their team, everyone (a quick `servicenow.aggregate` on incident by short_description over the last hour tells you if it is an outage; say so and stop diagnosing the individual).
    3. **Check the record.** If a management or identity connector is present, read the device (`microsoft-intune.search-devices`, `jamf-pro.search-computers`, `google-workspace-admin.get-user`) and the account (`okta.get-user` or `microsoft-365.get-user`) before asking the person for facts the system already holds. Quote what you read and its timestamp.
    4. **Classify** into exactly one primary class and name the skill that takes over:
       - Device (will not boot, slow, battery, disk full, peripheral, OS update) → `laptop-troubleshooting`
       - Network (no internet, Wi-Fi drops, VPN, DNS, a site unreachable) → `network-connectivity-troubleshooting`
       - Application (Zoom, Slack, Teams, Outlook, browser, Office) → `workplace-app-troubleshooting`
       - Sign-in (password, MFA, SSO, locked out) → `identity-and-sign-in-troubleshooting`
       - Access to a business system (ServiceNow, Salesforce, Workday, SAP: cannot see, cannot do, wrong role) → `saas-access-troubleshooting`
       - Not diagnosable remotely, hardware damage, security incident, data loss → `escalate-and-record` now.
    5. **Read the taking-over skill** with `skills.read` (name only) and follow its method in the same conversation. Do not restate the six facts to the person.
    6. **Record** the classified intake with `memory.remember` under the key `intake:<person>:<yyyy-mm-dd>` — one sentence: class, device, since when, the change that preceded it.

    ## Questions that save a round trip
    - "Does it happen on another network (phone hotspot)?" separates device from network.
    - "Does it happen in a private browser window / another app?" separates account from client.
    - "Did you change your password recently?" explains most sudden sign-in and mail failures.
    - "Is anyone near you seeing the same?" separates outage from individual.
    """ + GAP + """
    ## What done looks like
    One class, the six facts, the record read (or the gap filed), the taking-over skill named and its method started, and one memory note written.
    """,
    {"references/questions-by-symptom.md": """
    # Intake questions by symptom

    | Symptom the person says | Ask first | Likely class |
    |---|---|---|
    | "Everything is slow" | Since when? After an update? Fan loud? Disk free space? Same on hotspot? | Device, unless only web is slow (network) |
    | "No internet" | Wi-Fi icon state? Other devices on the same Wi-Fi? Hotspot works? VPN on? | Network |
    | "Cannot sign in" | Which screen: OS login, SSO page, the app itself? Error text? Password changed recently? New phone? | Sign-in |
    | "Outlook not syncing" | Web mail (outlook.office.com) shows new mail? Cached mode? Mailbox size? | App, unless web mail also fails (service or account) |
    | "Zoom audio bad" | Wired or Wi-Fi? Headset? Others hear it or only they? CPU busy? | App, unless everyone on the call is bad (network) |
    | "Cannot see records in Salesforce/ServiceNow" | Can they sign in? Which object/table? Colleague with the same role sees it? | SaaS access |
    | "Screen black / will not boot" | Any light or sound? Charger light? Keyboard backlight? | Device → escalate if no power at all |
    | "Printer" | Which printer name? Queue shows the job? Other people print to it? | Device (see laptop references, printing) |
    """})

skills["laptop-troubleshooting"] = skill(
    "laptop-troubleshooting",
    "Use for any problem on a person's computer — Mac, Windows, ChromeOS or Linux laptop: slow, will not boot, battery, disk full, updates, peripherals, printing. Reads the management record (Intune, Jamf, Google Admin) when connected, otherwise works from what the person reports, and follows the OS-specific reference for the exact checks and commands.",
    COMMON + ["microsoft-intune.search-devices", "microsoft-intune.get-device", "microsoft-intune.get-device-compliance", "microsoft-intune.sync-device", "microsoft-intune.restart-device", "jamf-pro.search-computers", "jamf-pro.get-computer", "jamf-pro.get-computer-mdm-capability", "jamf-pro.update-inventory", "jamf-pro.send-mdm-command", "google-workspace-admin.search-chromeos-devices", "google-workspace-admin.get-chromeos-device", "web.fetch"],
    ["connector:microsoft-intune", "connector:jamf-pro", "connector:google-workspace-admin", "tool:search_knowledge"],
    """
    # Laptop troubleshooting

    ## When to use
    After intake classified the issue as a device problem, or when a person names their laptop as the thing that fails.

    ## Method
    1. **Identify the OS**, then read that reference with `skills.read` (reference `references/macos.md`, `references/windows.md`, `references/chromeos.md`, `references/linux.md`). It holds the checks, the commands to ask the person to run, and the known causes ranked by how often they are the answer.
    2. **Read the device record** before asking the person anything the record holds:
       - Windows or managed macOS/iOS in Intune: `microsoft-intune.search-devices` by user or name, then `get-device` and `get-device-compliance`. Note lastSyncDateTime (a device that has not synced in days is not getting policy or updates), osVersion, freeStorageSpaceInBytes, complianceState and the failing setting.
       - Mac in Jamf: `jamf-pro.search-computers` by user, then `get-computer`. Note lastContactTime, OS version, available disk, FileVault state, the pending or failed profiles, installed apps and their versions.
       - ChromeOS: `google-workspace-admin.search-chromeos-devices` by user, then `get-chromeos-device`. Note lastSync, osVersion, status, diskVolumeReports, systemRamFreeReports.
       Quote every value you use with its timestamp. A stale record is a finding in itself (the device is not talking to management).
    3. **Rank causes** for the reported symptom using the reference's table and the record: disk under 10% free, OS two majors behind, a pending restart, a failed profile, a battery at end of life, a recent driver or agent update.
    4. **Fix in order of least disruption**: setting or cache first, then restart, then a management action (`sync-device`, `update-inventory`, `restart-device`, `send-mdm-command` RestartDevice), then reinstall or re-enrol, then hardware service. Any management action that interrupts the person needs their yes in the conversation first.
    5. **Verify** with the person: the exact thing that failed, tried again. Not "does it seem better".
    6. **Record** with `memory.remember`: `device:<hostname or serial>` — the cause found and the fix that worked, one sentence with the date. Next time the same device comes back, step 1 starts from it.
    7. If two rounds of fixes fail, or the reference says "hardware", hand over to `escalate-and-record` with the facts gathered.
    """ + GAP + """
    ## What done looks like
    The OS reference consulted, the record read (or gap filed), the cause named with evidence, the fix verified by the person, and the device note written.
    """,
    {"references/macos.md": """
    # macOS — checks, commands, known causes

    ## Facts to get
    - Version: Apple menu → About This Mac (or `sw_vers`). Chip: Apple silicon vs Intel changes the recovery steps.
    - Disk: `df -h /` or About This Mac → Storage. Under 10% free explains slowness, failed updates, "disk full" in apps.
    - Uptime: `uptime`. Weeks without a restart explains memory pressure and stuck agents.
    - Management: Jamf `get-computer` → lastContactTime, FileVault, profiles. System Settings → General → Device Management shows the profile on the Mac.
    - Battery: System Settings → Battery → Battery Health (Normal / Service Recommended), cycle count in System Information → Power.
    - Logs when something crashes: Console.app → Crash Reports, the process name and the first line of the backtrace.

    ## Symptom → ranked causes → fix
    | Symptom | Most often | Check | Fix |
    |---|---|---|---|
    | Slow, fan loud | A runaway process (Spotlight `mds_stores` reindexing, a browser tab, a sync client), then disk < 10% free, then memory pressure | Activity Monitor → CPU and Memory tabs, sort by % ; `df -h` | Quit the process; free disk (Downloads, Trash, `~/Library/Caches`, old Xcode/simulators, Docker images); restart; if Spotlight reindexes daily, exclude the folder in Siri & Spotlight settings |
    | Will not turn on | Power: dead battery or charger; then a stuck SMC/NVRAM state | Charger light/USB-C power; hold power 10 s | Charge 30 min; Apple silicon: hold power until "Loading startup options"; Intel: SMC reset (shift-ctrl-option-power 10 s), NVRAM (cmd-option-P-R) |
    | Stuck on login / spinning after password | FileVault unlock with an old password after a directory password change; a login item hang | Does the password work at the FileVault screen? | Use the previous password at FileVault, then the new one at login; Jamf: FileVault password sync via the management framework; Safe Mode (shift on Intel; hold power → Options → shift on Apple silicon) to skip login items |
    | Update will not install | Disk space (updates need 20–35 GB free), a management deferral, a stale management channel | `df -h`; Jamf lastContactTime; System Settings → Software Update shows "managed" | Free space; `jamf-pro.update-inventory`; if deferred by policy say so and give the date |
    | Wi-Fi fine but no internet | Captive portal, VPN half-connected, a stale DNS | See network reference | — |
    | External display not detected | Cable/adapter (USB-C needs DP alt-mode), display sleep state, a stuck WindowServer | Try the other port; Detect Displays (hold option in Displays settings) | Reseat; restart; NVRAM reset on Intel |
    | Keyboard/trackpad erratic | Bluetooth peripheral conflict, moisture, a stuck key | Does it happen in Safe Mode? | Toggle Bluetooth; if physical, hardware service |
    | Kernel panic / random restarts | A third-party kernel or system extension (VPN, EDR, virtualization), then hardware | Console → Crash Reports → "panic" report names the extension | Update or remove the extension; Apple Diagnostics (hold D / power on boot) |
    | "App is damaged and can't be opened" | Gatekeeper quarantine on an unsigned or broken download | `xattr -p com.apple.quarantine <app>` | Re-download from the approved source; managed apps via Self Service; never tell people to disable Gatekeeper |
    | Printer missing or job stuck | Queue paused, wrong driver/AirPrint, printer offline | System Settings → Printers → the queue; `lpstat -p` | Resume the queue; remove and re-add (AirPrint first); print a test page from another Mac to isolate |
    | Time Machine / backup failing | Destination full or unreachable, sparsebundle corruption | Backup log in Console (filter "backupd") | Re-select destination; for network destinations, verify the share mounts |

    ## Management actions (Jamf)
    - `update-inventory` after any fix so the record reflects it.
    - `send-mdm-command` RestartDevice only with the person's yes; DeviceLock only for a lost device and only through escalate-and-record.
    - A profile "Pending" for more than a day means the Mac is not checking in: have the person run `sudo jamf policy` or `sudo jamf manage`, or restart.

    ## Escalate when
    No power after charging 30 min and an SMC/power reset; liquid damage; battery "Service Recommended"; panics that name no third-party extension; Apple Diagnostics reports a code.
    """,
    "references/windows.md": """
    # Windows 10/11 — checks, commands, known causes

    ## Facts to get
    - Version: Settings → System → About, or `winver` (build number matters: a build two feature updates behind is out of support and out of compliance).
    - Disk: `Get-PSDrive C` or Settings → System → Storage. Under 10% free: slowness, update failures, Outlook OST errors.
    - Uptime: Task Manager → Performance → CPU → "Up time". Fast Startup hides restarts: "Shut down" is not a restart; ask for Restart.
    - Management: Intune `get-device` → lastSyncDateTime, complianceState, isEncrypted; on the device Settings → Accounts → Access work or school → Info → Sync.
    - Battery: `powercfg /batteryreport` writes battery-report.html; Full Charge Capacity vs Design Capacity under 60% is end of life.
    - Events: Event Viewer → Windows Logs → System (Critical, Error) around the time; Reliability Monitor (`perfmon /rel`) for a timeline.

    ## Symptom → ranked causes → fix
    | Symptom | Most often | Check | Fix |
    |---|---|---|---|
    | Slow | Disk 100% (Task Manager → Performance), then startup apps, then antivirus/EDR scan, then memory (8 GB with 30 tabs), then a failing HDD | Task Manager; `Get-PSDrive C`; Settings → Apps → Startup | Restart (real restart); free disk; trim startup apps; if disk 100% persists on an HDD, hardware (SSD swap) |
    | Will not boot / blue screen | A driver or update (stop code names it: DRIVER_IRQL_NOT_LESS_OR_EQUAL, INACCESSIBLE_BOOT_DEVICE), then disk, then memory | Photograph the stop code; Reliability Monitor after boot | Safe Mode (Shift+Restart → Troubleshoot → Startup Settings), roll back the driver, uninstall the last update (Settings → Windows Update → Update history → Uninstall); `chkdsk /f`; Windows Memory Diagnostic |
    | BitLocker recovery screen | A firmware/TPM change (BIOS update, docking-station firmware), Secure Boot toggled, a hardware change | Ask what changed | Recovery key from Intune (`get-device` → the desk reads it in the portal) or Entra "My devices"; after use, `microsoft-intune.rotate-bitlocker-keys` |
    | Update stuck / fails (0x800f0922, 0x80070002) | Disk space, a corrupt update cache, pending restart | Settings → Windows Update → Update history | Restart; `DISM /Online /Cleanup-Image /RestoreHealth` then `sfc /scannow`; stop Windows Update service, rename `C:\\Windows\\SoftwareDistribution`, start it; Intune `sync-device` |
    | Not compliant / "your organization requires" | Encryption off, OS build old, no PIN/Hello, firewall or Defender off | `get-device-compliance` names the failing setting | Fix that setting; `sync-device`; compliance evaluates within an hour |
    | Outlook/Teams sign-in loops | Broken Work account token (WAM), then time skew, then conditional access | `dsregcmd /status` (AzureAdJoined, AzureAdPrt YES/NO) | Settings → Accounts → Access work or school → disconnect/reconnect; `dsregcmd /leave` only for Entra-registered, never for joined devices without the desk; check clock |
    | Printer missing / stuck | Print Spooler service, driver mismatch after a Windows update, a Universal Print or server queue offline | services.msc → Print Spooler; `Get-Printer`; the queue window | Restart spooler (`Restart-Service Spooler`), clear `C:\\Windows\\System32\\spool\\PRINTERS`, remove and re-add the printer; print from a colleague's PC to isolate |
    | External monitor / dock | Dock firmware, display driver, USB-C bandwidth (two 4K at 60 Hz needs DP 1.4) | Device Manager → Monitors, Display adapters | Reseat, other port, update dock firmware; `Win+Ctrl+Shift+B` resets the graphics driver |
    | Audio device missing | Wrong default device after a headset, a driver update | Settings → System → Sound → Output | Pick the device; Device Manager → uninstall the audio device → Scan for hardware changes |
    | Wi-Fi adapter gone | Driver, power management turned it off, airplane mode | Device Manager → Network adapters (yellow mark?) | Roll back driver; adapter properties → Power Management → untick "allow the computer to turn off" |
    | "Something went wrong" in Microsoft apps 80090016/80090034 | TPM/Hello key issue on the account | `dsregcmd /status` | Sign out of the app, `certutil -deletehellocontainer`, sign in again (documented Microsoft fix); disconnect/reconnect work account |

    ## Management actions (Intune)
    - `sync-device` after any fix; compliance and policy reflect within an hour.
    - `restart-device` with the person's yes; `rotate-bitlocker-keys` after any recovery key was read out.
    - lastSyncDateTime more than 3 days old: the device is not talking to Intune; on the device, Settings → Accounts → Access work or school → Info → Sync, then check `dsregcmd /status`.

    ## Escalate when
    Stop codes without a named driver after a rollback; disk hardware errors (Event ID 7, 51, 153); no power; liquid; battery under 60% health; a BitLocker recovery loop after the key.
    """,
    "references/chromeos.md": """
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
    """,
    "references/linux.md": """
    # Linux laptops (Ubuntu, Fedora, Debian) — checks, commands, known causes

    ## Facts to get
    - Distro and kernel: `cat /etc/os-release`, `uname -r`. Desktop: GNOME/KDE, Wayland or X11 (`echo $XDG_SESSION_TYPE`).
    - Disk: `df -h /` and `df -h /home`; `/boot` full breaks kernel updates (`df -h /boot`).
    - Logs: `journalctl -p err -b` (this boot), `journalctl -b -1` (last boot, for crashes), `dmesg -T | tail`.
    - Hardware: `lspci -nnk` (drivers in use), `lsusb`, `upower -i $(upower -e | grep BAT)` for battery health.
    - Management, if any: the organisation's agent (osquery, Fleet, Landscape); note the last check-in in its console.

    ## Symptom → ranked causes → fix
    | Symptom | Most often | Check | Fix |
    |---|---|---|---|
    | Slow | A process (`top`/`htop`), swap thrash (`free -h`), a full disk, baloo/tracker indexing | `top`, `free -h`, `df -h` | Kill/restart the process; `sudo apt clean`/`dnf clean`; remove old kernels (`sudo apt autoremove`); pause indexing |
    | Will not boot to desktop | A kernel update with a missing DKMS module (NVIDIA, VirtualBox), a full `/boot`, a broken display manager | Boot the previous kernel from GRUB (hold Shift/Esc); `journalctl -b -1 -p err` | Rebuild DKMS (`sudo dkms autoinstall`), free `/boot`, `sudo systemctl restart gdm`/`sddm` |
    | Wi-Fi missing | Driver/firmware for the chipset (Realtek, Broadcom), rfkill, NetworkManager stopped | `nmcli device`, `rfkill list`, `dmesg | grep -i firmware` | `sudo rfkill unblock all`; install `linux-firmware`/vendor driver; `sudo systemctl restart NetworkManager` |
    | VPN | See network reference; on Linux, DNS split via systemd-resolved (`resolvectl status`) is the usual culprit | `resolvectl status` | Set the VPN's DNS domains on the tunnel interface |
    | Audio | PipeWire/PulseAudio picked the wrong sink, a muted channel | `wpctl status` / `pactl list sinks short` | Pick the sink in settings; `systemctl --user restart pipewire pipewire-pulse wireplumber` |
    | External display | Wayland vs X11 quirks, NVIDIA driver | Try the other session at login | Switch session; update driver |
    | Suspend/resume broken | Kernel/driver, especially NVIDIA and some Wi-Fi | `journalctl -b | grep -i suspend` | Update kernel/driver; disable a module on suspend as a workaround |
    | Package manager broken | Interrupted upgrade, a held or broken package | `sudo apt --fix-broken install`, `sudo dpkg --configure -a`; `sudo dnf distro-sync` | As per check |
    | Disk encryption prompt at boot fails | Wrong keyboard layout at the LUKS prompt, a changed passphrase | Try US layout | `cryptsetup luksChangeKey` with the old key |
    | Printing | CUPS stopped, no driver | `systemctl status cups`, http://localhost:631 | Start CUPS; add the printer via IPP Everywhere |

    ## Escalate when
    Hardware errors in `dmesg` (I/O errors, MCE), battery under 60% health, no power; anything that needs a distro reinstall without a backup.
    """,
    "references/printing.md": """
    # Printing — one method for every OS
    1. Is the printer up? Another person prints to it → yes. If no one can, it is the printer or its queue server: escalate with the printer name and location.
    2. Is the job in the queue? Paused queue: resume. Stuck job at the top: cancel it, then print a test page.
    3. Right driver? Prefer AirPrint/IPP Everywhere (Mac, ChromeOS, Linux) and Universal Print or the vendor's Type 4 driver (Windows). Remove and re-add the printer after a driver change.
    4. Network path: the printer's IP pings from the laptop? A VPN or guest Wi-Fi often cannot reach office printers; say so.
    5. Record the printer name with the fix in memory (`printer:<name>`): the next person with the same printer gets the answer in one turn.
    """})

skills["network-connectivity-troubleshooting"] = skill(
    "network-connectivity-troubleshooting",
    "Use for no internet, Wi-Fi that drops, VPN that will not connect or stays connected without access, DNS and proxy failures, and 'this one site does not load'. A layered method (link, address, DNS, path, application) that isolates the failing layer with the person's own commands, reads the device record when a management connector is present, and checks vendor service health before blaming the laptop.",
    COMMON + ["microsoft-intune.search-devices", "microsoft-intune.get-device", "jamf-pro.search-computers", "jamf-pro.get-computer", "google-workspace-admin.get-chromeos-device", "microsoft-365.list-service-health", "servicenow.aggregate", "web.fetch"],
    ["connector:microsoft-intune", "connector:jamf-pro", "tool:search_knowledge"],
    """
    # Network and connectivity troubleshooting

    ## When to use
    After intake classified the issue as network, or when the failure is "cannot reach" anything.

    ## Method — isolate the layer, top down is slower, bottom up is right
    1. **Blast radius first.** `servicenow.aggregate` incidents by short_description containing "network" or "VPN" in the last hour; `microsoft-365.list-service-health` if the unreachable thing is Microsoft. If many people are affected, say so, point them to the outage, and stop individual diagnosis.
    2. **Link.** Is the laptop connected to a network at all? Wi-Fi icon, Ethernet light. Wi-Fi name matches the intended one (not a neighbour, not a guest network). Signal: 1–2 bars is a link problem.
    3. **Address.** Did it get an address? An address starting 169.254 means DHCP failed. Mac: `ifconfig en0`; Windows: `ipconfig`; Linux: `ip a`; ChromeOS: chrome://network#state.
    4. **Gateway and DNS.** Ping the gateway (`ipconfig`/`netstat -nr` shows it). Then resolve a name: `nslookup example.com`. Then reach an IP without DNS: `ping 1.1.1.1`. IP works but names do not → DNS. Neither → path (captive portal, firewall, VPN).
    5. **Path.** `traceroute` / `tracert` to the destination; where does it stop? A captive portal answers everything with a redirect: open a plain http page (neverssl.com) to get the portal.
    6. **VPN.** Read `references/vpn.md`. The question "which VPN client and what does its status line say" comes before anything else.
    7. **Application.** One site or service fails while the rest work: it is the site, a proxy rule, a certificate (clock skew makes every certificate invalid: check the date), or an allow-list. Read `references/dns-proxy-and-one-site.md`.
    8. **Device record.** If a management connector is present, read the device: OS version and last sync (a Wi-Fi driver or VPN client two versions behind is a finding). Quote what you read.
    9. **Fix** at the failing layer only, least disruptive first: forget and rejoin Wi-Fi, renew DHCP, flush DNS, toggle VPN, restart, then driver/client update, then escalate to the network team with the traceroute.
    10. **Verify** the exact original failure, and **record** `network:<person>:<yyyy-mm-dd>` with the layer and the fix. If the same person reports the same layer twice in a month, say so and file a gap: the fix was a workaround.
    """ + GAP + """
    ## What done looks like
    The failing layer named with the command output that shows it, the fix applied at that layer, verified, and recorded.
    """,
    {"references/wifi.md": """
    # Wi-Fi — known causes ranked
    1. **Wrong network or captive portal.** Guest networks need a portal; corporate 802.1X needs a valid certificate or credentials. Open http://neverssl.com to surface a portal.
    2. **Roaming between access points** drops calls hourly: the laptop sticks to a far AP. Forget and rejoin; on Mac, Wi-Fi Diagnostics (option-click Wi-Fi icon → Wireless Diagnostics) shows RSSI (< -70 dBm is poor) and noise. On Windows, `netsh wlan show interfaces` shows Signal.
    3. **Power management** turns the adapter off (Windows: adapter properties → Power Management). Docked laptops with the lid closed and Wi-Fi power saving are a classic.
    4. **Band steering / 2.4 GHz congestion.** Prefer 5 GHz/6 GHz; on Windows adapter properties → Preferred Band.
    5. **Driver or firmware.** Intel Wi-Fi driver updates fix more drops than anything else; on Mac, the OS update carries it.
    6. **802.1X certificate expired**: the device connects and then disconnects, or "authentication failed". Management renews the SCEP/PKCS profile: Intune `get-device-configuration-states`, Jamf profiles.
    7. **Random MAC address** (iOS/Android/macOS private address, Windows random hardware address) breaks MAC-allow-listed networks: turn it off for that network only.
    8. **DHCP exhaustion** in a busy office: 169.254 address for many people at once → network team.
    Commands: Mac `networksetup -listallhardwareports`, `sudo wdutil info`; Windows `netsh wlan show interfaces`, `netsh wlan show wlanreport`; Linux `nmcli device wifi list`, `iw dev wlan0 link`.
    """,
    "references/vpn.md": """
    # VPN — known causes ranked
    Ask: which client (Cisco Secure Client/AnyConnect, GlobalProtect, Zscaler ZPA, Tailscale, WireGuard, FortiClient, OpenVPN), and the exact status text.
    1. **Wrong password / MFA not completed**: the client says "Login failed" or waits for a push. Check the identity side (`identity-and-sign-in-troubleshooting`).
    2. **Connected but nothing works**: split-tunnel DNS. The tunnel is up but names resolve on the local network. Mac: `scutil --dns`; Windows: `Get-DnsClientNrptPolicy`; Linux: `resolvectl status`. Fix: reconnect; the client's DNS settings; on Linux set the tunnel's DNS domains.
    3. **Connects then drops every N minutes**: rekey/session lifetime, or Wi-Fi roaming underneath (see wifi.md). Note the interval; a fixed interval is a policy, a random one is the link.
    4. **Blocked network**: hotel/airport blocks UDP 443/500/4500 or IKE. Try the client's TCP fallback or a phone hotspot to prove it.
    5. **Posture/compliance check failed**: the client says the device is non-compliant (EDR not running, disk not encrypted, OS old). Read the management record; fix compliance; `sync-device`.
    6. **Certificate expired**: device certificate for the VPN profile; management renews it.
    7. **Client version too old** after a gateway upgrade: update the client from the approved source.
    8. **Local network conflict**: home router on 10.0.0.0/24 or 192.168.1.0/24 overlapping the corporate range; the client cannot route. Change the home subnet or use the client's "allow local LAN" toggle if policy permits.
    9. **Always-on VPN with no captive portal exception**: cannot reach the portal to get online. The client usually has a "captive portal remediation" window; otherwise use a hotspot briefly.
    Logs: Cisco `DART` bundle; GlobalProtect → Settings → Troubleshooting → Collect Logs; Zscaler → More → Export Logs. Ask for the last 20 lines around the failure, not the whole bundle.
    """,
    "references/dns-proxy-and-one-site.md": """
    # DNS, proxy, and "just this one site"
    - **Clock skew** makes every TLS site fail with certificate errors: check the date and time zone first.
    - **DNS**: `nslookup site` fails but `ping 1.1.1.1` works → flush (`sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder` on Mac; `ipconfig /flushdns` on Windows; `resolvectl flush-caches` on Linux), then try the corporate resolver explicitly (`nslookup site <resolver ip>`). If the corporate resolver fails for one name that public DNS resolves, it is a block or an internal-only name (needs VPN).
    - **Proxy / secure web gateway** (Zscaler, Netskope, Umbrella): a block page or a certificate from the gateway means policy, not the laptop. Read the block reason on the page; the fix is a request to the security team, not a workaround. A proxy PAC that is stale explains "some sites hang": Mac `networksetup -getautoproxyurl Wi-Fi`; Windows `netsh winhttp show proxy` and Settings → Network → Proxy.
    - **IPv6**: a site fails only on some networks; disabling IPv6 on the adapter is a diagnostic, not a fix. Report it to the network team with the site.
    - **Browser-only**: works in a private window → extension or cache; works in another browser → that browser's proxy/DNS settings (Firefox has its own DNS-over-HTTPS; Chrome has its own secure DNS).
    - **MTU**: sites load partly then hang, VPN especially: lower the MTU on the tunnel (1400) as a test.
    - **Certificate pinned/inspected**: an app (not a browser) fails behind TLS inspection; the app needs the gateway root certificate in its own trust store (Java, Python, Docker, git each keep their own). Say which store.
    """})

skills["workplace-app-troubleshooting"] = skill(
    "workplace-app-troubleshooting",
    "Use for problems in the apps people work in all day — Zoom, Microsoft Teams, Slack, Outlook and Exchange mail and calendar, browsers, Microsoft 365 desktop apps. Separates account from client from service (does web work? is the vendor down?), reads the vendor's own diagnostics when connected (Zoom quality, mailbox rules, service health), and follows the per-app reference for the known causes and fixes.",
    COMMON + ["zoom.get-user", "zoom.get-user-settings", "zoom.list-user-meetings", "zoom.list-dashboard-meetings", "zoom.get-meeting-participants-qos", "zoom.get-account-settings", "microsoft-365.get-user", "microsoft-365.get-mailbox-settings", "microsoft-365.list-inbox-rules", "microsoft-365.list-service-health", "microsoft-365.list-service-issues", "slack.list-users", "web.fetch"],
    ["connector:zoom", "connector:microsoft-365", "connector:slack", "tool:search_knowledge"],
    """
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
    """ + GAP + """
    ## What done looks like
    Service, account or client named with the evidence, the fix from the reference applied and verified, the note written.
    """,
    {"references/zoom.md": """
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
    Cache: Mac `~/Library/Application Support/zoom.us`, Windows `%APPDATA%\\Zoom`; uninstall with Zoom's own cleaner (CleanZoom) before reinstalling.
    """,
    "references/teams.md": """
    # Microsoft Teams — known causes ranked
    | Symptom | Most often | Fix |
    |---|---|---|
    | Sign-in loop / "trouble signing in" | Work account token (WAM) on Windows, Keychain on Mac, time skew, conditional access | Windows: Settings → Accounts → Access work or school → reconnect; `dsregcmd /status`; Mac: Keychain → delete Microsoft entries after backing up; check clock; identity skill for CA (`microsoft-365.list-sign-ins` error 53003) |
    | Calls drop / bad quality | Same physics as Zoom; also the Teams "media optimization" missing in a VDI | Teams → Settings → Devices → test call; QoS in the Teams admin centre (desk) | Wired; close apps; new Teams client |
    | Notifications missing | Focus assist/Do Not Disturb, status set, channel notification settings | Settings → Notifications | As found |
    | Chat history / files not loading | Client cache | Quit Teams; delete cache: Windows `%APPDATA%\\Microsoft\\Teams` (classic) or `%LOCALAPPDATA%\\Packages\\MSTeams_8wekyb3d8bbwe\\LocalCache` (new); Mac `~/Library/Group Containers/UBF8T346G9.com.microsoft.teams`, `~/Library/Containers/com.microsoft.teams2` | Reopen |
    | Cannot join external meeting | Guest access / federation policy, browser join blocked | Join from the browser (Edge/Chrome) as a test | Policy request to the Teams admin |
    | Add-in missing in Outlook | Teams Meeting add-in not loaded | Outlook → File → Options → Add-ins → COM add-ins → tick; run Teams once as the user first |
    | Service | Exchange/Teams incident | `microsoft-365.list-service-issues` | Report |
    """,
    "references/slack.md": """
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
    """,
    "references/outlook-exchange.md": """
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
    Profiles/cache: Windows `%LOCALAPPDATA%\\Microsoft\\Outlook\\*.ost`; new Outlook for Windows has no OST (web-based). Mac: `~/Library/Group Containers/UBF8T346G9.Office/Outlook`.
    """,
    "references/browsers.md": """
    # Browsers — Chrome, Edge, Safari, Firefox
    - Works in a private/incognito window → an extension or cached data. Disable extensions; clear site data for that site only, not everything.
    - Works in another browser → that browser's proxy or secure DNS setting, or an enterprise policy (chrome://policy, edge://policy) blocking the site or feature.
    - Certificate error everywhere → clock; on one site behind a gateway → the gateway root is not in that browser's trust (Firefox keeps its own store: about:config `security.enterprise_roots.enabled` true).
    - Managed browsers reset settings (homepage, extensions) on purpose: say it is policy.
    - Sign-in to a Google or Microsoft account in the browser is separate from the OS account; a profile signed into a personal account explains "wrong Drive/OneDrive".
    - Memory/slow: dozens of tabs, a heavy extension; Chrome → More tools → Task manager shows which.
    - Downloads blocked → gateway or policy; the block page says which.
    """,
    "references/office-apps.md": """
    # Microsoft 365 desktop apps (Word, Excel, PowerPoint, OneDrive)
    - "Unlicensed product": the account signed into Office is not the licensed one, or licence removed. `microsoft-365.get-user` assignedLicenses. File → Account → sign out, sign in with the work account.
    - Activation errors 0x8004FC12, 0x80070005: Office Licensing service; run Office repair (Apps → Microsoft 365 → Modify → Quick Repair, then Online Repair).
    - OneDrive not syncing: a file name with invalid characters or a path > 400 chars, a locked file, "files on demand" state; OneDrive icon → Help & Settings → View sync problems names the file. Reset: `%LOCALAPPDATA%\\Microsoft\\OneDrive\\onedrive.exe /reset`.
    - Autosave off: the file is not in OneDrive/SharePoint or is .xls/.doc; save as .xlsx/.docx in a synced location.
    - Excel slow: volatile formulas, external links, a 200 MB workbook; not the laptop.
    - Crashes on open: add-ins (start with /safe); a corrupt normal.dotm (rename it).
    - Mac Office: sign-in issues clear with `~/Library/Group Containers/UBF8T346G9.Office` licence removal via the Microsoft License Removal Tool, then sign in again.
    """})

skills["identity-and-sign-in-troubleshooting"] = skill(
    "identity-and-sign-in-troubleshooting",
    "Use for cannot sign in, locked out, password or MFA problems, SSO errors, 'your sign-in was blocked', lost or new phone for the authenticator. Reads the identity provider's own record and sign-in log (Okta, Microsoft Entra, Google Workspace) so the failure reason is read, not guessed, verifies the person before any account action, and takes the least action that restores access.",
    COMMON + ["okta.get-user", "okta.list-user-factors", "okta.list-user-groups", "okta.list-user-app-links", "okta.system-log", "okta.unlock-user", "okta.expire-password", "okta.reset-factor", "okta.clear-sessions", "microsoft-365.get-user", "microsoft-365.list-sign-ins", "microsoft-365.list-authentication-methods", "microsoft-365.revoke-sign-in-sessions", "microsoft-365.delete-authentication-method", "google-workspace-admin.get-user", "google-workspace-admin.list-login-events", "sap-cloud-identity.search-users", "sap-cloud-identity.get-user"],
    ["connector:okta", "connector:microsoft-365", "connector:google-workspace-admin"],
    """
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
    """ + GAP + """
    ## What done looks like
    The reason code read from the log, the person verified before any action, the least action taken, access confirmed by the person, the note written without secrets.
    """,
    {"references/error-codes.md": """
    # Sign-in failure reasons and what they mean

    ## Microsoft Entra (sign-in log `status.errorCode`)
    | Code | Meaning | Fix |
    |---|---|---|
    | 0 | Success | The problem is after sign-in (app or access) |
    | 50126 | Wrong password | Self-service reset; check caps lock and keyboard layout; a saved old credential in an app |
    | 50053 | Account locked (smart lockout) or IP blocked | Wait the lockout window; a device with an old password keeps locking it: find it in the log (device/app) |
    | 50057 | Account disabled | HR/offboarding or admin action; an enable is a request |
    | 50055 / 50144 | Password expired | Change it (self-service), or via an admin reset |
    | 50074 / 50076 / 50079 | MFA required / not completed / registration required | Complete or register MFA; new phone → method reset |
    | 53003 | Blocked by Conditional Access | Read the policy name in the log: device compliance, location, app. Device → laptop skill; location → VPN/network; app → policy |
    | 53000 / 53001 | Device not compliant / not domain joined | Laptop skill: compliance |
    | 50105 | Signed in but not assigned to the app | Access request |
    | 65001 | Consent required | Admin consent for the app |
    | 700016 / 7000215 | App misconfigured (client id/secret) | App owner, not the person |
    | 50058 / 50140 | Silent sign-in failed / keep me signed in interrupt | Benign; retry interactively |
    | 500121 | MFA challenge failed (wrong code, denied push) | Retry; time sync on the phone for TOTP; new phone → reset |
    | 50199 | Sign-in blocked pending user confirmation of a new sign-in | Approve or deny in the prompt |

    ## Okta (`outcome.reason`, event types `user.session.start`, `user.authentication.auth_via_mfa`)
    | Reason | Meaning | Fix |
    |---|---|---|
    | INVALID_CREDENTIALS | Wrong password | Reset; check a stuck client (mail app) retrying old password → that is also the lockout source |
    | LOCKED_OUT | Too many failures | `unlock-user` after verification; find the source of failures in the log |
    | PASSWORD_EXPIRED | Expired | Change it |
    | USER_NOT_ASSIGNED / app.access.denied | No app assignment | Access request; `list-user-app-links` |
    | VERIFICATION_ERROR / mfa failed | Wrong or expired code, push denied | Retry; new phone → `reset-factor` |
    | DENIED_BY_POLICY | Sign-on policy (network zone, device trust) | Read the policy in the event; device trust → laptop skill |
    | SUSPENDED / DEPROVISIONED | Account state | HR/admin action |
    | INVALID_TOKEN / session expired | Stale session | Sign out everywhere; `clear-sessions` if stuck |

    ## Google Workspace (`login_failure_type`)
    | Type | Meaning | Fix |
    |---|---|---|
    | login_failure_invalid_password | Wrong password | Reset |
    | login_failure_2sv_failure / login_failure_2sv_required | 2SV | Backup codes; admin can generate backup codes; new phone re-enrol |
    | login_failure_account_disabled / suspended | Account | Admin/HR |
    | suspicious_login / login_challenge | Google challenged a new location or device | Answer the challenge; admin can "temporarily turn off login challenge" for 10 minutes |
    | account_disabled_password_leak | Google found the password leaked | Reset now; check sessions |

    ## SAML/SSO errors seen on the app side
    - "AADSTS…" codes are Entra; look them up above.
    - "Your request has been blocked" with a Cloudflare/Zscaler page is network policy, not identity.
    - App says "user not found" after SSO succeeds: the app has no account for the person (provisioning/SCIM lag or not assigned). Access skill.
    - Clock skew on the laptop (> 5 min) breaks SAML assertions: check the clock.
    """})

skills["saas-access-troubleshooting"] = skill(
    "saas-access-troubleshooting",
    "Use when a person can sign in but cannot see or do something in a business system — ServiceNow, Salesforce, Workday, SAP, Slack, Google Workspace, Microsoft 365 — or needs access they do not have: wrong role, missing licence, a record they cannot open, a report that is empty for them. Separates permission from data from product behaviour, reads the system's own record of the person where connected, and turns a permission gap into a proper access request instead of a workaround.",
    COMMON + ["servicenow.aggregate", "servicenow.list-catalog-items", "servicenow.order-catalog-item", "servicenow.create-record", "salesforce.soql-query", "salesforce.list-objects", "okta.list-user-groups", "okta.list-user-app-links", "microsoft-365.get-user", "sap-cloud-identity.search-users", "sap-cloud-identity.get-user", "sap-cloud-identity.list-groups", "slack.list-users", "google-workspace-admin.get-user"],
    ["connector:servicenow", "connector:salesforce", "connector:okta", "connector:sap-cloud-identity"],
    """
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
    """ + GAP + """
    ## What done looks like
    Permission, data or behaviour named with the system's own record quoted, the right request raised or the fix verified, the note written with the request number.
    """,
    {"references/servicenow.md": """
    # ServiceNow — access model and traps
    - **Roles** (sys_user_has_role) grant tables and UI actions; **groups** (sys_user_grmember) carry roles and assignment. A person "cannot see the incident" usually lacks itil (or the fulfiller role for that table), or the record is in another **domain** (domain separation: sys_domain on the record) or **company**.
    - **ACLs** produce "Security constraints prevent access to requested page" or empty lists: the record exists but the read ACL fails. Ask an admin to run the security debugger; do not file a defect.
    - **Licensing**: a fulfiller role needs a fulfiller licence; "Unlicensed" users get read-only. That is a request to the platform owner.
    - Useful reads: `servicenow.list-records` table sys_user query `user_name=jdoe` fields `active,locked_out,roles,company,domain`; table sys_user_has_role query `user.user_name=jdoe` fields `role.name`; the record itself with `servicenow.get-record`.
    - **Requests**: catalog items for roles/groups (`list-catalog-items` search "role" or "group membership"), ordered for the person with their yes and a business reason; `create-record` on sc_request only when no item fits.
    - Traps: list filters saved on the person's view hide records ("Assigned to me"); a deactivated user (active=false) keeps their records; portal users (snc_external) cannot see fulfiller UI.
    """,
    "references/salesforce.md": """
    # Salesforce — access model and traps
    Access = **profile** (object/field CRUD) + **permission sets** + **role** (record visibility up the hierarchy) + **OWD and sharing rules** (record-level) + **licence**.
    SOQL to read the person: `SELECT Id, IsActive, Profile.Name, UserRole.Name, UserType, LastLoginDate FROM User WHERE Username='jdoe@acme.com'`; permission sets `SELECT PermissionSet.Name FROM PermissionSetAssignment WHERE Assignee.Username='jdoe@acme.com'`; a record's owner `SELECT OwnerId, Owner.Name FROM Opportunity WHERE Id='006...'`; who can see a record `SELECT UserOrGroupId, AccessLevel, RowCause FROM OpportunityShare WHERE OpportunityId='006...'`.
    Known causes:
    - "Insufficient privileges" on a record → sharing (OWD Private, not owner, not above in role hierarchy, no sharing rule/team). Fix: owner shares or adds to the team; a sharing rule is admin work.
    - Field is missing on the page → field-level security on the profile/permission set, or page layout. A request.
    - Report empty → the report's scope ("My opportunities"), a filter, or sharing; run "All opportunities" as a test.
    - "This user is inactive" / "licence limit" → licence; a request.
    - Cannot log in from a new place → login IP ranges / login hours on the profile.
    - CPQ/Quotes: SBQQ objects have their own permission sets; a "Quote Calculator" error is usually a missing CPQ permission set.
    - Lightning vs Classic: a feature "missing" is often the other interface.
    """,
    "references/workday.md": """
    # Workday — access model and traps
    - Security is **domain security policies** granted to **security groups** (role-based: Manager, HR Partner; user-based: Payroll Admin; job-based). A worker sees a task if their group has the domain, and a **business process security policy** decides who can initiate/approve.
    - "You do not have access to this task" → the group lacks the domain; a request to the Workday security admin with the exact task name (the error and the task name from the search bar are what they need).
    - A manager cannot see a report's people → the role assignment on the supervisory organisation is missing or stale (reorg lag); HR corrects the role assignment.
    - Inbox item missing / cannot approve → business process step routes to a different role; ask who is the assigned approver (the BP "Process History").
    - Delegation: a delegate cannot see tasks outside the delegated BPs; that is by design.
    - Mobile app vs web: some tasks are web-only.
    - Payroll, compensation and personal data questions are HR desk matters, not IT; hand over with the task name.
    """,
    "references/sap.md": """
    # SAP — access model and traps
    - **S/4HANA / ECC**: authorization objects in **roles** (PFCG) assigned to the user (SU01). "You are not authorized" → the missing authorization object shows in transaction SU53 right after the failure; ask the person to run SU53 and screenshot the first red line. The request goes to the SAP security team with that object and the transaction code.
    - **Fiori launchpad**: tiles come from catalogs/groups in the role; a missing tile is a role issue, an error inside the tile is often an OData service authorization (also SU53) or a gateway error (/IWFND/ERROR_LOG for the basis team).
    - **SAP Cloud Identity (IAS)**: `sap-cloud-identity.get-user` shows active/locked and groups; a user missing a group cannot reach the application even with a valid login. Locked after failed logins → `patch-user` after identity verification.
    - **SuccessFactors**: role-based permissions (RBP) by permission group; HR desk matters.
    - **Concur / Ariba**: separate identities unless SSO; "user not found" after SSO = not provisioned.
    - Password vs SSO: SAP GUI logon uses the SAP password or SNC/Kerberos; a GUI "password logon no longer possible" means the user is locked in SU01 (basis unlock), not in IAS.
    """,
    "references/collaboration-suites.md": """
    # Microsoft 365, Google Workspace, Slack — access and licences
    - **Licence missing** (Teams/Exchange/Drive not available): `microsoft-365.get-user` assignedLicenses; `google-workspace-admin.get-user` (suspended, orgUnitPath decides services); the fix is a licence request.
    - **Group-based access**: SharePoint sites, Teams, shared drives, Slack channels are owned by their owners; the desk does not add people, the owner does. Say who the owner is when it is readable (site/Team owner; Slack channel details).
    - **Guest/external**: a guest in Entra or a Slack Connect user has different limits; "cannot see" is often a guest restriction by policy.
    - **Deactivated by SCIM/identity sync**: Slack `list-users` deleted=true or a Google suspended account after a lifecycle change; HR/identity, not a product issue.
    - **OneDrive/Drive sharing "blocked"**: an external-sharing policy or a DLP rule; the block message names it; the fix is a policy request or an internal share.
    """})

skills["escalate-and-record"] = skill(
    "escalate-and-record",
    "Use when a support conversation cannot be resolved remotely, needs a team with different access (network, security, HR, SAP basis, hardware service), is a security incident, or when two rounds of fixes failed. Raises the right ticket in ServiceNow with the facts already gathered (never asking the person twice), sets priority from impact and urgency, records the case in memory, and files a gap when the desk lacked a tool or a connection.",
    COMMON + ["servicenow.create-incident", "servicenow.create-record", "servicenow.update-record", "servicenow.list-catalog-items", "servicenow.order-catalog-item", "servicenow.aggregate"],
    ["connector:servicenow"],
    """
    # Escalate and record

    ## When to use
    Any of: hardware damage or no power; data loss; a security incident (compromise, forwarding rule, phishing clicked, lost device); a fix that needs access the desk does not have; an outage affecting many; two rounds of fixes that failed; the person asks for a ticket.

    ## Method
    1. **Do not re-ask.** Everything intake and the method skill gathered goes into the ticket: who, device (hostname/serial), OS and version, since when, what changed, exact error, what was tried and what each attempt showed, the evidence read from systems (quote with timestamps).
    2. **Incident or request?** Something broken → `servicenow.create-incident`. Something wanted (access, licence, a replacement device, a new printer) → the catalog: `list-catalog-items`, then `order-catalog-item` with the person's yes; `create-record` on sc_request only when no item fits. A security matter → an incident with category Security and the security assignment group from knowledge (`search_knowledge` "security incident assignment").
    3. **Priority** from impact × urgency, and say the words:
       - Impact: one person / a team / a site or everyone.
       - Urgency: work stopped with no workaround / degraded with a workaround / inconvenience.
       One person, work stopped, no workaround → P3 with a note; a team → P2; site-wide or security → P1. Follow the organisation's matrix if knowledge has one.
    4. **Check for duplicates**: `servicenow.aggregate` incidents with the same short description in the last day; if an outage ticket exists, link the person to it (a child incident or a note) rather than a new P1.
    5. **Write the ticket** so the next engineer starts where you stopped: short description in the form "<system> — <symptom> — <person/site>"; description with the facts in the order above; work notes with the evidence quoted.
    6. **Tell the person** the ticket number, who it went to, the priority in plain words, and what to expect next. Offer a workaround if one exists.
    7. **Record** `case:<person>:<ticket>` in memory with the class, cause so far, and the ticket number. Update the earlier `intake:` note with the ticket.
    8. **File gaps** with `gaps.file` for anything the desk could not do because a tool or connection was missing, so the platform learns what to add. One gap per missing capability, with the ticket as evidence.

    ## What done looks like
    The right record type raised with every fact already gathered, a priority with the reasoning, no duplicate of an outage, the person told the number and the next step, memory and gaps written.
    """,
    {"references/priority-matrix.md": """
    # Priority matrix (default; the organisation's own matrix in knowledge wins)
    | Impact \\ Urgency | Work stopped, no workaround | Degraded, workaround exists | Inconvenience |
    |---|---|---|---|
    | Site / everyone | P1 | P2 | P3 |
    | A team | P2 | P3 | P4 |
    | One person | P3 | P4 | P4 |
    Security incidents (compromise, lost device with data, phishing with credentials entered) are P1 regardless; hardware replacement is a request, not an incident.
    """})

# ───────────────────────── write ─────────────────────────
os.makedirs(f"{ROOT}/connectors", exist_ok=True)
for cid, m in connectors.items():
    with open(f"{ROOT}/connectors/{cid}.json", "w") as f: json.dump(m, f, indent=2, ensure_ascii=False); f.write("\n")
for name, (md, refs) in skills.items():
    d = f"{ROOT}/skills/{name}"; os.makedirs(f"{d}/references", exist_ok=True)
    open(f"{d}/SKILL.md", "w").write(md)
    for path, text in refs.items(): open(f"{d}/{path}", "w").write(text)
plugin = {"id": "it-service-desk", "name": "IT Service Desk", "version": "1.0.0", "publisher": "Rusty",
          "description": "Everything a first-line IT desk does — intake, laptops (Mac, Windows, ChromeOS, Linux), network and VPN, workplace apps (Zoom, Teams, Slack, Outlook), sign-in and MFA, access to business systems, escalation — as method skills with per-platform references, and the connectors they read from (Jamf, Intune, Microsoft 365, Okta, Google Workspace, Zoom, SAP Cloud Identity) written from the vendors' API documentation. Connect what you have; each skill files a gap for what it could not read.",
          "connectors": [f"connectors/{cid}.json" for cid in connectors], "skills": [f"skills/{n}" for n in skills]}
json.dump(plugin, open(f"{ROOT}/plugin.json", "w"), indent=2, ensure_ascii=False)
tot = sum(len(open(os.path.join(r, f)).read()) for r, _, fs in os.walk(ROOT) for f in fs)
print(f"wrote {len(connectors)} connectors, {len(skills)} skills, {sum(len(r) for _, r in skills.values())} references, {tot//1024} KB")
for name, (md, _) in skills.items():
    fm = md.split('---')[1]; tools = [l for l in fm.splitlines() if l.startswith('allowed-tools')][0].count(',') + 1
    desc = [l for l in fm.splitlines() if l.startswith('description')][0]
    print(f"  {name}: {tools} tools, description {len(desc.encode())} B")
