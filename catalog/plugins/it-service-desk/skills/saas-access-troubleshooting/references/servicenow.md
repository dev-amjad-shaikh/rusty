# ServiceNow — access model and traps
- **Roles** (sys_user_has_role) grant tables and UI actions; **groups** (sys_user_grmember) carry roles and assignment. A person "cannot see the incident" usually lacks itil (or the fulfiller role for that table), or the record is in another **domain** (domain separation: sys_domain on the record) or **company**.
- **ACLs** produce "Security constraints prevent access to requested page" or empty lists: the record exists but the read ACL fails. Ask an admin to run the security debugger; do not file a defect.
- **Licensing**: a fulfiller role needs a fulfiller licence; "Unlicensed" users get read-only. That is a request to the platform owner.
- Useful reads: `servicenow.list-records` table sys_user query `user_name=jdoe` fields `active,locked_out,roles,company,domain`; table sys_user_has_role query `user.user_name=jdoe` fields `role.name`; the record itself with `servicenow.get-record`.
- **Requests**: catalog items for roles/groups (`list-catalog-items` search "role" or "group membership"), ordered for the person with their yes and a business reason; `create-record` on sc_request only when no item fits.
- Traps: list filters saved on the person's view hide records ("Assigned to me"); a deactivated user (active=false) keeps their records; portal users (snc_external) cannot see fulfiller UI.
