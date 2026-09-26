# Envelope and recipient states
- **created**: drafted, not sent. **sent**: notification sent. **delivered**: a recipient opened it. **completed**: all signed. **declined**: a recipient declined (declinedReason). **voided**: cancelled by the sender (voidedReason). **signed** (recipient): signed but the envelope waits on others.
- Routing order: signers in order 1 get it first; order 2 sees nothing until order 1 completes. "Waiting on the customer" is often waiting on our own counter-signer at order 2.
- Expiry: envelopes expire after the account's default (often 120 days) unless set; an expired envelope must be re-issued.
- Bounced email: the audit trail shows "Email bounced"; correct the recipient (sender) rather than resend.
- Reminder settings are per envelope; resend sends the notification again now.
