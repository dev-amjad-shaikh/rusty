---
name: contract-and-signature-status
description: Use when a rep asks where a contract is: sent, viewed, who has not signed, why it was declined, when it expires, whether to resend or void. Reads DocuSign envelopes, recipients and the audit trail, the contract and order records in CRM, and takes the nudge (resend) with the rep's yes; voiding and re-issuing go through the sender.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, salesforce.soql-query, salesforce.list-objects, docusign.list-envelopes, docusign.get-envelope, docusign.list-recipients, docusign.audit-events, docusign.resend-envelope, docusign.void-envelope
dependencies: connector:docusign, connector:salesforce
license: MIT
---
    # Contract and signature status

    ## When to use
    "Has <customer> signed?", "resend the contract", "it says declined", "the envelope expired", "which version is out".

    ## Method
    1. **Find the envelope.** `docusign.list-envelopes` from the send date with `search_text` = the customer or contract number; confirm with the rep when more than one matches (an old version still out for signature is a common trap).
    2. **Read where it stands.** `get-envelope` (status, sent, delivered, completed, expire date, voided reason) and `list-recipients` (each signer: sent → delivered → completed; routingOrder tells who is next; declinedReason is the customer's words). `audit-events` shows whether the signer has even opened it.
    3. **Say it plainly**: "Sent 12 Sep, opened by <name> 13 Sep, waiting on <name> (routing order 2), expires 26 Sep." Never say "in progress".
    4. **Act, least first**:
       - Not opened for 3+ business days → `resend-envelope` with the rep's yes; suggest the rep also message the signer directly.
       - Wrong signer, wrong version, expired → the sender corrects or re-issues; the desk does not void on its own. `void-envelope` only when the rep who sent it says so in the conversation, with the reason they give.
       - Declined → quote the reason to the rep; the answer is a conversation, not a resend.
    5. **CRM side.** `salesforce.soql-query` for the Contract and Order (status, start/end, amount) so the rep knows what happens after signature (booking, provisioning). If the contract record does not match the envelope, say so.
    6. **Record** `contract:<account>` with the envelope id, status and the action taken.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The envelope named, each signer's state and the next signer named, the action taken with the rep's yes, and the CRM contract state checked.
