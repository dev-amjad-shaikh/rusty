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
