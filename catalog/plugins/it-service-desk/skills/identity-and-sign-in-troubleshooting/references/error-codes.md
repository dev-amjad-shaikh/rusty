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
