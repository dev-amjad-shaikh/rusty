# Category set

Closed set. Route each item to the queue its category maps to.

| Category | Queue | Examples |
|---|---|---|
| `billing` | `billing-queue` | invoices, charge disputes, refunds |
| `bug` | `engineering-queue` | crashes, regressions, error reports |
| `feature_request` | `product-queue` | asks for new capability |
| `account` | `support-queue` | access, credentials, profile changes |
| `uncategorized` | `triage-queue` | spam, ambiguous, out of scope |

Rules:

- Exactly one category per item.
- When an item spans categories, pick the one whose queue can act first —
  a crash that also mentions a refund is a `bug` until it stops crashing.
- `uncategorized` is a real answer. Forcing a fit is worse than asking a
  human.
