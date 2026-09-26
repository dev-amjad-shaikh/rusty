# CPQ and pricing — where quotes get stuck
| Symptom | Most often | Check | Fix |
|---|---|---|---|
| "Quote calculation failed" / spinner | Missing CPQ permission set, a price rule error, a product without a price book entry in the quote's price book | Quote's price book vs product's entries; user's permission sets | Add the entry (sales ops); assign the permission set |
| Cannot add a product | Product inactive, not in the price book, option constraint on the bundle | `SELECT IsActive FROM Product2`; PricebookEntry for the price book | Sales ops |
| Discount field locked | Above the rep's authority; the approval rule locks it | Approval matrix in knowledge | Submit for approval |
| Quote not on the opportunity | Not marked Primary | SBQQ__Primary__c | Set primary |
| Wrong currency or term | Opportunity currency vs quote; subscription term default | Quote fields | Sales ops corrects the opportunity |
| "Approval required" but no approver assigned | The approval chain has no user for that level (left the company) | Approval chain record | Sales ops assigns |
| Expired quote | SBQQ__ExpirationDate__c passed | Date | Clone the quote |
## Discount policy — how to read a matrix
A matrix usually has authority levels by discount band and by term (e.g. rep ≤10 %, manager ≤20 %, VP ≤30 %, CFO above) with multipliers for multi-year and volume. Read the band for the *effective* discount (including free months, credits, services thrown in), not only the line discount.
## Non-standard terms that always go to legal
Uncapped liability, indemnity beyond the standard, data residency or sub-processor changes, payment terms beyond net-60, termination for convenience, MFN or exclusivity, source-code escrow.
