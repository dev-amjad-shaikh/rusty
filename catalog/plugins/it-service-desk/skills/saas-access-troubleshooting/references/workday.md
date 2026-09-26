# Workday — access model and traps
- Security is **domain security policies** granted to **security groups** (role-based: Manager, HR Partner; user-based: Payroll Admin; job-based). A worker sees a task if their group has the domain, and a **business process security policy** decides who can initiate/approve.
- "You do not have access to this task" → the group lacks the domain; a request to the Workday security admin with the exact task name (the error and the task name from the search bar are what they need).
- A manager cannot see a report's people → the role assignment on the supervisory organisation is missing or stale (reorg lag); HR corrects the role assignment.
- Inbox item missing / cannot approve → business process step routes to a different role; ask who is the assigned approver (the BP "Process History").
- Delegation: a delegate cannot see tasks outside the delegated BPs; that is by design.
- Mobile app vs web: some tasks are web-only.
- Payroll, compensation and personal data questions are HR desk matters, not IT; hand over with the task name.
