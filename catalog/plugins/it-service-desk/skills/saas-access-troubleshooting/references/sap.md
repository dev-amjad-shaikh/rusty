# SAP — access model and traps
- **S/4HANA / ECC**: authorization objects in **roles** (PFCG) assigned to the user (SU01). "You are not authorized" → the missing authorization object shows in transaction SU53 right after the failure; ask the person to run SU53 and screenshot the first red line. The request goes to the SAP security team with that object and the transaction code.
- **Fiori launchpad**: tiles come from catalogs/groups in the role; a missing tile is a role issue, an error inside the tile is often an OData service authorization (also SU53) or a gateway error (/IWFND/ERROR_LOG for the basis team).
- **SAP Cloud Identity (IAS)**: `sap-cloud-identity.get-user` shows active/locked and groups; a user missing a group cannot reach the application even with a valid login. Locked after failed logins → `patch-user` after identity verification.
- **SuccessFactors**: role-based permissions (RBP) by permission group; HR desk matters.
- **Concur / Ariba**: separate identities unless SSO; "user not found" after SSO = not provisioned.
- Password vs SSO: SAP GUI logon uses the SAP password or SNC/Kerberos; a GUI "password logon no longer possible" means the user is locked in SU01 (basis unlock), not in IAS.
