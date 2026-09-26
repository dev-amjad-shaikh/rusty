#!/usr/bin/env python3
"""Generate catalog/plugins/sales-desk and catalog/plugins/hr-desk on the same
structure as the IT desk: connector manifests from the vendors' documented
APIs, method skills with per-product references, declared dependencies."""
import sys, os
sys.path.insert(0, os.path.dirname(__file__))
from plugin_gen import *  # noqa

CATALOG = os.path.join(os.path.dirname(__file__), "..", "..", "catalog", "plugins")
BEARER = [{"style": "bearer", "token": "{credentials.access_token}"}]
COMMON = ["search_knowledge", "memory.recall", "memory.remember", "skills.read", "gaps.file"]

# ═════════════════════════════ SALES DESK ═════════════════════════════
sales_connectors = {}

docusign_authz = {"authorize_url": "https://account.docusign.com/oauth/auth", "token_url": "https://account.docusign.com/oauth/token",
                  "scopes": "signature extended", "client_id": "{credentials.client_id}", "client_secret": "{credentials.client_secret}", "extra_params": {}}
sales_connectors["docusign"] = manifest(
    "docusign", "DocuSign", "DocuSign eSignature API: where a contract stands — envelope status (sent, delivered, completed, declined, voided), who has and has not signed, the audit trail — and the nudges a desk sends (resend) or the stop it applies (void).",
    "https://developers.docusign.com/docs/esign-rest-api/reference/", "https://{account_host}/restapi/v2.1/accounts/{account_id}",
    spec("DocuSign Connection", {"account_host": plain("API base host", 0, "From the account's user info: demo.docusign.net or na2.docusign.net.", "na2.docusign.net"),
                                 "account_id": plain("Account ID", 1, "The API account id (GUID) from Settings → Apps and Keys.", "12345678-aaaa-bbbb-cccc-1234567890ab"),
                                 "credentials": creds_oauth_app("DocuSign integration key", "Settings → Apps and Keys → Add App and Integration Key, with a secret key. The tokens are issued when you authorize.")}, ["account_host", "account_id", "credentials"]),
    [op("check-connection", "Verify the authorization is accepted.", "GET", "/users?count=1", "read_only", auth=BEARER),
     op("list-envelopes", "Envelopes changed since a date, optionally by status or a search text (customer name, contract number). Returns envelopeId, status, emailSubject, sentDateTime, completedDateTime.", "GET", "/envelopes", "read_only",
        {"from_date": P("ISO-8601 start, e.g. 2026-09-01"), "status": P("sent, delivered, completed, declined, voided (comma-separated)"), "search_text": P("Free text over subject and recipients"), "count": P("Rows", "integer", default=20)}, ["from_date"], auth=BEARER),
     op("get-envelope", "One envelope: status, sender, sentDateTime, deliveredDateTime, completedDateTime, expireDateTime, voidedReason.", "GET", "/envelopes/{envelopeId}", "read_only", {"envelopeId": P("Envelope id")}, ["envelopeId"], auth=BEARER),
     op("list-recipients", "Who must sign and where it stands: each signer's status (sent, delivered, completed, declined), signedDateTime, routingOrder, declinedReason.", "GET", "/envelopes/{envelopeId}/recipients", "read_only", {"envelopeId": P("Envelope id")}, ["envelopeId"], auth=BEARER),
     op("audit-events", "The envelope's audit trail: viewed, signed, resent, corrected, with timestamps and actors.", "GET", "/envelopes/{envelopeId}/audit_events", "read_only", {"envelopeId": P("Envelope id")}, ["envelopeId"], auth=BEARER),
     op("resend-envelope", "Resend the signing notification to the current recipients. Safe to repeat; tell the rep you did it.", "PUT", "/envelopes/{envelopeId}?resend_envelope=true", "idempotent", {"envelopeId": P("Envelope id")}, ["envelopeId"], auth=BEARER),
     op("void-envelope", "Void an in-flight envelope with a reason. Cannot be undone; needs the sender's yes.", "PUT", "/envelopes/{envelopeId}", "irreversible", {"envelopeId": P("Envelope id"), "status": P("Must be voided", default="voided"), "voidedReason": P("Why, shown to recipients")}, ["envelopeId", "status", "voidedReason"], auth=BEARER)],
    "check-connection", docusign_authz)

gong_auth = [{"style": "basic", "username": "{credentials.access_key}", "password": "{credentials.access_key_secret}"}]
sales_connectors["gong"] = manifest(
    "gong", "Gong", "Gong API: recorded sales calls — who was on them, when, the transcript-derived topics, trackers and next steps — for one account or one rep over a period.",
    "https://gong.app.gong.io/settings/api/documentation", "https://api.gong.io/v2",
    spec("Gong Connection", {"credentials": {"type": "object", "title": "API key", "description": "Company Settings → Ecosystem → API → Create key: an access key and its secret (technical administrator).", "required": ["access_key", "access_key_secret"], "additionalProperties": False, "rusty_order": 0,
                                             "properties": {"access_key": secret("Access key", 0), "access_key_secret": secret("Access key secret", 1)}}}, ["credentials"]),
    [op("check-connection", "Verify the key is accepted.", "GET", "/users?limit=1", "read_only", auth=gong_auth),
     op("list-calls", "Calls in a window: id, title, started, duration, participants' emails, direction, the CRM opportunity it is linked to.", "GET", "/calls", "read_only",
        {"fromDateTime": P("ISO-8601 start, e.g. 2026-09-01T00:00:00Z"), "toDateTime": P("ISO-8601 end"), "cursor": P("Paging cursor")}, ["fromDateTime", "toDateTime"], auth=gong_auth),
     op("get-call", "One call's metadata and participants.", "GET", "/calls/{id}", "read_only", {"id": P("Call id")}, ["id"], auth=gong_auth),
     op("calls-extensive", "Calls with content: topics, trackers, points of interest (next steps, questions, objections), talk ratios, and the CRM context. Filter by date and by participant email or call ids.", "POST", "/calls/extensive", "read_only",
        {"filter": P('{"fromDateTime": "...", "toDateTime": "...", "callIds": [...]} or with "primaryUserIds"', "object"), "contentSelector": P('{"exposedFields": {"content": {"topics": true, "trackers": true, "pointsOfInterest": true}, "interaction": {"speakers": true}, "parties": true}}', "object")}, ["filter", "contentSelector"], auth=gong_auth),
     op("list-users", "Gong users (reps) with emails, for mapping a rep to their calls.", "GET", "/users", "read_only", {"cursor": P("Paging cursor")}, auth=gong_auth)],
    "check-connection")

outreach_authz = {"authorize_url": "https://api.outreach.io/oauth/authorize", "token_url": "https://api.outreach.io/oauth/token",
                  "scopes": "prospects.read sequences.read sequenceStates.read mailings.read users.read accounts.read", "client_id": "{credentials.client_id}", "client_secret": "{credentials.client_secret}", "extra_params": {}}
sales_connectors["outreach"] = manifest(
    "outreach", "Outreach", "Outreach API: a prospect's engagement — which sequences they are in, the step they are on, replies, bounces, opt-outs — and the mailings sent to them, so a rep's 'did they get my email' has an answer.",
    "https://developers.outreach.io/api/reference/", "https://api.outreach.io/api/v2",
    spec("Outreach Connection", {"credentials": creds_oauth_app("Outreach OAuth app", "Outreach → Settings → Integrations → Outreach API: an app with the read scopes. The tokens are issued when you authorize.")}, ["credentials"]),
    [op("check-connection", "Verify the authorization is accepted.", "GET", "/users?page[size]=1", "read_only", auth=BEARER),
     op("list-prospects", "Find prospects by email, name or account.", "GET", "/prospects", "read_only", {"filter[emails]": P("An email address"), "filter[name]": P("Full name"), "filter[account][name]": P("Account name"), "page[size]": P("Rows", "integer", default=10)}, auth=BEARER),
     op("get-prospect", "One prospect: stage, owner, optedOut, engagedAt, clickCount, openCount, replyCount, touchedAt.", "GET", "/prospects/{id}", "read_only", {"id": P("Prospect id")}, ["id"], auth=BEARER),
     op("list-sequence-states", "The prospect's sequence memberships: which sequence, state (active, paused, finished, bounced, replied, optedOut), current step, next step at.", "GET", "/sequenceStates", "read_only", {"filter[prospect][id]": P("Prospect id"), "page[size]": P("Rows", "integer", default=20)}, ["filter[prospect][id]"], auth=BEARER),
     op("list-mailings", "Emails sent to a prospect: subject, state (delivered, bounced, opened, replied), deliveredAt, openedAt, repliedAt, bouncedAt.", "GET", "/mailings", "read_only", {"filter[prospect][id]": P("Prospect id"), "sort": P("Order", default="-deliveredAt"), "page[size]": P("Rows", "integer", default=20)}, ["filter[prospect][id]"], auth=BEARER),
     op("list-sequences", "Sequences on the account, with enabled state and step counts.", "GET", "/sequences", "read_only", {"filter[name]": P("Sequence name"), "page[size]": P("Rows", "integer", default=20)}, auth=BEARER)],
    "check-connection", outreach_authz)

SALES_SF = ["salesforce.soql-query", "salesforce.list-objects"]
sales_skills = {}

sales_skills["sales-request-intake"] = skill(
    "sales-request-intake",
    "Use at the start of every request from a seller, a sales manager or sales ops: turn 'can you help with this deal' into a classified request with the deal's facts — account, opportunity, stage, amount, close date, what is blocking — and route it to the right method skill (deal desk, pipeline hygiene, account brief, contract status, billing and renewal) or to sales-escalate-and-record.",
    COMMON + SALES_SF + ["hubspot.list-deals", "hubspot.list-companies", "slack.list-users"],
    ["connector:salesforce"],
    """
    # Sales request intake

    ## When to use
    The first turn of any sales support request; again when the conversation moves to a second deal.

    ## Method
    1. **Recall** `memory.recall` with the account and the opportunity name: an open thread from last week changes the first question.
    2. **Gather the deal facts** in one message, only what is missing: the account, the opportunity (name or id), stage and amount, close date, the product or SKU set, who the rep and the manager are, and the one thing that is blocking.
    3. **Read the CRM record before asking** what it already holds. Salesforce: `salesforce.soql-query` — `references/soql.md` has the queries for the opportunity, its products, the account's open deals and the last activity. HubSpot: `hubspot.list-deals` / `list-companies` by name. Quote the fields with their last-modified date; a CloseDate in the past or a stage that has not moved in 30 days is a finding to say out loud.
    4. **Classify** into one class and name the skill that takes over:
       - Price, discount, quote, approval, terms, non-standard clause → `deal-desk-quote-and-discount`
       - Stale, wrong or missing CRM data; forecast category; next steps; a stage the rep cannot advance → `pipeline-hygiene-and-forecast`
       - "Tell me about this account before the call"; competitive, product or objection questions → `account-and-contact-research-brief`
       - Contract out for signature, who has not signed, resend, redline status → `contract-and-signature-status`
       - Order booked but not provisioned, invoice, payment, renewal date, churn risk → `billing-renewal-and-order-status`
       - A request for sales ops, legal, finance or product that the desk cannot resolve → `sales-escalate-and-record`
    5. **Read the taking-over skill** with `skills.read` and continue in the same conversation without restating the facts.
    6. **Record** `intake:<opportunity or account>:<yyyy-mm-dd>` with class, the blocker, and the owner.
    """ + GAP + """
    ## What done looks like
    One class, the deal facts, the CRM record quoted (or the gap filed), the taking-over skill started, a memory note.
    """,
    {"references/soql.md": """
    # SOQL the desk uses (Salesforce)
    - Opportunity: `SELECT Id, Name, StageName, Amount, CloseDate, Probability, ForecastCategoryName, NextStep, LastActivityDate, LastModifiedDate, Owner.Name, Account.Name, Type FROM Opportunity WHERE Name LIKE '%<name>%' ORDER BY LastModifiedDate DESC LIMIT 5`
    - Products on it: `SELECT PricebookEntry.Product2.Name, Quantity, UnitPrice, TotalPrice, Discount FROM OpportunityLineItem WHERE OpportunityId='006...'`
    - Account and its open deals: `SELECT Id, Name, Industry, Owner.Name, AnnualRevenue, (SELECT Name, StageName, Amount, CloseDate FROM Opportunities WHERE IsClosed=false) FROM Account WHERE Name LIKE '%<name>%'`
    - Last activity: `SELECT Subject, ActivityDate, Owner.Name FROM Task WHERE WhatId='006...' ORDER BY ActivityDate DESC LIMIT 5`
    - Contacts: `SELECT Name, Title, Email, Phone FROM Contact WHERE AccountId='001...'`
    - Open cases (support health): `SELECT CaseNumber, Subject, Status, Priority, CreatedDate FROM Case WHERE AccountId='001...' AND IsClosed=false`
    - Quotes (CPQ): `SELECT Name, SBQQ__Status__c, SBQQ__NetAmount__c, SBQQ__ExpirationDate__c, SBQQ__Primary__c FROM SBQQ__Quote__c WHERE SBQQ__Opportunity2__c='006...'`
    - Contracts: `SELECT ContractNumber, Status, StartDate, EndDate, ContractTerm, Account.Name FROM Contract WHERE AccountId='001...'`
    - Orders: `SELECT OrderNumber, Status, EffectiveDate, TotalAmount FROM Order WHERE AccountId='001...' ORDER BY EffectiveDate DESC`
    - Stale pipeline for a rep: `SELECT Name, StageName, Amount, CloseDate, LastModifiedDate FROM Opportunity WHERE Owner.Name='<rep>' AND IsClosed=false AND (CloseDate < TODAY OR LastModifiedDate < LAST_N_DAYS:30)`
    Field names vary by org: `salesforce.list-objects` and the org's data dictionary in knowledge win over this page.
    """})

sales_skills["deal-desk-quote-and-discount"] = skill(
    "deal-desk-quote-and-discount",
    "Use for pricing, discount, quote and approval questions: what discount a rep may give, who approves beyond it, why a CPQ quote is stuck, whether a non-standard term is allowed. Answers from the organisation's pricing and approval policy in knowledge (never from the model's idea of a fair price), reads the quote's state in CRM, and routes approvals to the named approver instead of promising a price.",
    COMMON + SALES_SF + ["hubspot.list-deals", "slack.list-users", "slack.post-message", "artifacts.write"],
    ["connector:salesforce", "tool:search_knowledge"],
    """
    # Deal desk — quote, discount, approval

    ## When to use
    A rep asks what they can offer, why a quote will not go out, who must approve, or whether a clause is acceptable.

    ## Rules that do not bend
    - The desk never sets a price. It reads the policy and the approval matrix from knowledge (`search_knowledge` "discount approval", "pricing policy", "non-standard terms") and says what the policy allows and who approves the rest. If knowledge has no policy, say so and route to the deal desk owner; never fill the gap with a plausible number.
    - Every figure quoted back to the rep carries where it came from: the quote line in CRM, the price book, or the policy page.
    - Non-standard legal terms (liability, indemnity, data residency, payment terms beyond the standard) go to legal through `sales-escalate-and-record`, with the clause quoted.

    ## Method
    1. **Read the quote and the deal.** `salesforce.soql-query` for the opportunity and its products, and the CPQ quote if the org uses CPQ (`references/cpq-and-pricing.md`). Note list price, proposed price, discount %, term, and the quote status (Draft, In Review, Approved, Rejected, Presented).
    2. **Read the policy.** `search_knowledge` for the discount matrix (by product, by term, by deal size), the approval chain, and the standard terms. Quote the line that applies.
    3. **Compare.** Discount within the rep's authority → say so with the policy line. Beyond → name the approver level and what the approval needs (business justification, competitive evidence, term length). A quote in "In Review" is waiting on that approver: say who, from the approval record if CRM exposes it.
    4. **Stuck quote diagnostics** (`references/cpq-and-pricing.md`): missing price book entry, an inactive product, a validation rule (term, currency), a quote not primary, an expired quote, a missing permission set.
    5. **Package the ask.** When an approval is needed, draft the approval request with `artifacts.write` (deal, amount, discount, term, justification, competitive context, what the customer will do if declined) and, if the org's deal desk channel is in knowledge, post a two-line summary there with `slack.post-message` only with the rep's yes.
    6. **Record** `dealdesk:<opportunity>` with the policy line applied and the approver named.
    """ + GAP + """
    ## What done looks like
    The policy line quoted, the quote's state read, an answer of "within authority" or "needs <approver> for <reason>", a packaged approval request when needed, and a note.
    """,
    {"references/cpq-and-pricing.md": """
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
    """})

sales_skills["pipeline-hygiene-and-forecast"] = skill(
    "pipeline-hygiene-and-forecast",
    "Use when a rep or manager asks about stale or wrong pipeline, what stage a deal should be in, forecast category and commit, next steps, or a stage that will not advance. Reads the pipeline with SOQL, applies the organisation's stage exit criteria from knowledge, reads the last customer call (Gong) and outreach state to ground next steps, and produces a concrete list of what to update — never updating the CRM itself.",
    COMMON + SALES_SF + ["hubspot.list-deals", "gong.list-calls", "gong.calls-extensive", "outreach.list-prospects", "outreach.list-sequence-states", "artifacts.write"],
    ["connector:salesforce", "connector:gong", "tool:search_knowledge"],
    """
    # Pipeline hygiene and forecast

    ## When to use
    Stale deals, "what stage is this really", forecast category and commit questions, next steps, a stage that will not advance, a manager's pipeline review prep.

    ## Method
    1. **Read the pipeline** with `salesforce.soql-query` (`references/soql.md` in sales-request-intake; the stale-pipeline query for a rep, or the single opportunity). Flag: CloseDate in the past; LastModifiedDate > 30 days; no NextStep; Amount empty; stage above what the activity supports.
    2. **Read the stage criteria** from knowledge (`search_knowledge` "stage exit criteria", "sales process", "MEDDIC" or the org's qualification). Each stage has exit evidence (champion identified, technical win, legal engaged, verbal commit). Compare the deal's recorded evidence to the criteria and say which criterion is missing for the stage it is in.
    3. **Ground the next step in the last conversation.** `gong.list-calls` for the account window, then `gong.calls-extensive` on the newest call: the points of interest (next steps, objections, questions) are the customer's words; quote them. Outreach: `list-sequence-states` shows whether follow-ups are running, replied or bounced.
    4. **Forecast category** follows the org's definitions in knowledge (Pipeline, Best Case, Commit, Closed). A Commit needs the evidence the definition names; say what is missing when it is not there.
    5. **Produce the hygiene list** with `artifacts.write`: one line per deal — what to change (stage, close date, amount, next step, forecast category) and the evidence for it. The rep or manager updates the CRM; the desk does not.
    6. **Record** `pipeline:<rep or opportunity>:<yyyy-mm-dd>` with the count of findings. A rep whose list repeats monthly is a coaching signal, not a data problem: say so to the manager if they asked.
    """ + GAP + """
    ## What done looks like
    Every flagged deal has a named criterion or a stale field with evidence, next steps come from the last call or sequence, the list is filed as an artifact, and nothing in the CRM was changed by the desk.
    """,
    {"references/stage-criteria-default.md": """
    # Default stage exit criteria (the organisation's process in knowledge wins)
    | Stage | Exit evidence |
    |---|---|
    | Qualification | Budget owner named, a business problem in the customer's words, a timeline, a next meeting booked |
    | Discovery | Pain quantified, decision process and criteria written, champion identified with their stake |
    | Solution / Demo | Technical fit confirmed by the customer's technical evaluator, competitors named |
    | Proposal | Proposal sent to the economic buyer, pricing acknowledged, procurement steps known |
    | Negotiation | Legal/security review started, redlines in progress, verbal agreement on price and term |
    | Commit | Signature date agreed, paperwork with the signer, no open legal items |
    Forecast: Commit = Negotiation or later with a signature date this period; Best Case = Proposal or later with a plausible path; Pipeline = the rest.
    """})

sales_skills["account-and-contact-research-brief"] = skill(
    "account-and-contact-research-brief",
    "Use before a customer meeting or when a rep asks 'what do we know about this account': builds a one-page brief from our own systems — CRM history, open deals and cases, contacts, the last calls (Gong), outreach engagement, support tickets, billing state, and the product and competitive knowledge on file — with every line sourced. Files it as an artifact the rep can open in the meeting.",
    COMMON + SALES_SF + ["hubspot.list-companies", "hubspot.list-contacts", "hubspot.list-deals", "gong.list-calls", "gong.calls-extensive", "outreach.list-prospects", "outreach.get-prospect", "zendesk.search-tickets", "stripe.list-customers", "web.fetch", "artifacts.write", "artifacts.read"],
    ["connector:salesforce", "connector:gong", "tool:search_knowledge"],
    """
    # Account and contact research brief

    ## When to use
    "Prep me for the call with <account>", "who is <contact>", "what happened last time with <account>", competitive or product questions ahead of a meeting.

    ## Method
    1. **Reuse before rebuilding.** `artifacts.read` for a brief on this account from the last 14 days; `memory.recall` for what the desk learned about them. Update rather than rewrite.
    2. **Read our systems**, each one a section with its source named:
       - CRM: account, contacts and titles, open and closed deals with amounts and outcomes, last activities (`salesforce.soql-query`; or `hubspot.list-companies`/`list-contacts`/`list-deals`).
       - Conversations: `gong.list-calls` for the account in the last 90 days, `gong.calls-extensive` on the newest two: topics, objections, next steps, who spoke.
       - Engagement: `outreach.list-prospects` by email → `get-prospect` (opened, replied, opted out).
       - Support health: `zendesk.search-tickets` for the organisation (open, escalated, satisfaction) when connected; otherwise Salesforce Cases.
       - Billing: `stripe.list-customers` by email/name for a live customer (delinquent, disputes) when connected.
       - Knowledge: `search_knowledge` for the account plan, the industry play, the competitive battlecard for competitors named in calls, and the product one-pager for what they own.
       - Public: `web.fetch` the company's newsroom or investor page only from hosts the egress policy allows; quote the headline and date.
    3. **Write the brief** with `artifacts.write`, name `brief-<account>-<yyyy-mm-dd>`, in this order: who they are (one line), where the relationship stands (deals, health, billing), who we know and who is missing (economic buyer? champion?), what they said last time (quotes), open risks and objections with the battlecard answer, three suggested questions for the meeting, sources.
    4. **Say what you could not read** (a system not connected → gap) so the rep knows the brief's blind spots.
    5. **Record** `account:<name>` with the two most important facts and the artifact name.
    """ + GAP + """
    ## What done looks like
    A brief filed as an artifact with every line sourced, blind spots named, and the memory note pointing at it.
    """,
    {"references/brief-template.md": """
    # Brief template
    **<Account>** — <industry>, <size>, <region>. Owner: <rep>. Source: CRM.
    **Where we stand.** <open deals: stage, amount, close>. <customer since / ARR / products> (CRM, billing). Support: <open tickets, escalations> (support system).
    **People.** <name — title — role in the deal (champion / economic buyer / evaluator)>; missing: <economic buyer not identified>.
    **Last conversations.** <date — call title>: "<customer's words on next step or objection>" (Gong). Outreach: <sequence state, last reply>.
    **Risks and objections.** <objection> → <battlecard answer> (knowledge: <doc>).
    **Questions for the meeting.** 1. … 2. … 3. …
    **Not read.** <system not connected / no data> — reported as a gap.
    """})

sales_skills["contract-and-signature-status"] = skill(
    "contract-and-signature-status",
    "Use when a rep asks where a contract is: sent, viewed, who has not signed, why it was declined, when it expires, whether to resend or void. Reads DocuSign envelopes, recipients and the audit trail, the contract and order records in CRM, and takes the nudge (resend) with the rep's yes; voiding and re-issuing go through the sender.",
    COMMON + SALES_SF + ["docusign.list-envelopes", "docusign.get-envelope", "docusign.list-recipients", "docusign.audit-events", "docusign.resend-envelope", "docusign.void-envelope"],
    ["connector:docusign", "connector:salesforce"],
    """
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
    """ + GAP + """
    ## What done looks like
    The envelope named, each signer's state and the next signer named, the action taken with the rep's yes, and the CRM contract state checked.
    """,
    {"references/envelope-states.md": """
    # Envelope and recipient states
    - **created**: drafted, not sent. **sent**: notification sent. **delivered**: a recipient opened it. **completed**: all signed. **declined**: a recipient declined (declinedReason). **voided**: cancelled by the sender (voidedReason). **signed** (recipient): signed but the envelope waits on others.
    - Routing order: signers in order 1 get it first; order 2 sees nothing until order 1 completes. "Waiting on the customer" is often waiting on our own counter-signer at order 2.
    - Expiry: envelopes expire after the account's default (often 120 days) unless set; an expired envelope must be re-issued.
    - Bounced email: the audit trail shows "Email bounced"; correct the recipient (sender) rather than resend.
    - Reminder settings are per envelope; resend sends the notification again now.
    """})

sales_skills["billing-renewal-and-order-status"] = skill(
    "billing-renewal-and-order-status",
    "Use for 'is the order booked', 'why is provisioning late', 'has the invoice been paid', 'when does this renew', 'is this customer at risk': reads orders and contracts in CRM, customer and payment state in billing (Stripe), support health, and the renewal opportunity, and hands finance or order management a complete case when the answer needs them.",
    COMMON + SALES_SF + ["stripe.list-customers", "stripe.get-charge", "stripe.list-disputes", "zendesk.search-tickets", "jira.search-issues", "jira.get-issue"],
    ["connector:salesforce", "connector:stripe"],
    """
    # Billing, renewal and order status

    ## When to use
    Order, invoice, payment, provisioning, renewal and churn-risk questions from sellers or CS.

    ## Method
    1. **Order and contract**: `salesforce.soql-query` Order (status: Draft, Activated; EffectiveDate), Contract (Status, StartDate, EndDate, ContractTerm) and the renewal Opportunity (Type = Renewal, CloseDate). An Activated order with no provisioning ticket is the usual "where is my order".
    2. **Provisioning**: `jira.search-issues` (JQL `project = <provisioning project> AND text ~ "<account>"`) when the org tracks provisioning in Jira; else the ServiceNow request in knowledge's process. Quote the ticket's status and assignee.
    3. **Billing**: `stripe.list-customers` by email or name → the customer's `delinquent` flag and default payment method; `stripe.get-charge` for a specific payment; `stripe.list-disputes` for chargebacks. Invoice and subscription reads are not yet in the Stripe connector: ask finance for the invoice state and file the gap.
    4. **Health**: `zendesk.search-tickets` for the organisation (open escalations, low satisfaction) when connected; Salesforce Cases otherwise.
    5. **Renewal**: contract EndDate minus today; the renewal opportunity's stage; usage or health if knowledge has an account plan. Say "renews in N days, renewal opportunity at <stage>, <risk signals>".
    6. **Hand off with a complete case** when finance or order management must act: account, order/contract numbers, what is expected, what the systems show, what the customer said. Through `sales-escalate-and-record`.
    7. **Record** `billing:<account>` with the state and the handoff.
    """ + GAP + """
    ## What done looks like
    Order, provisioning, payment and renewal each answered with the record quoted, gaps filed for what could not be read, and a complete handoff when one was needed.
    """,
    {})

sales_skills["sales-escalate-and-record"] = skill(
    "sales-escalate-and-record",
    "Use when a sales request needs sales ops, legal, finance, order management or product, when a CRM change is beyond the desk (merging accounts, reassigning ownership, fixing a price book), or when the rep asks for a ticket. Raises the request where the organisation tracks it (Jira for sales ops and product, the deal desk channel for approvals) with the facts already gathered, records the case in memory, and files a gap when the desk lacked a tool or connection.",
    COMMON + ["jira.search-issues", "jira.get-issue", "jira.list-projects", "slack.list-channels", "slack.post-message", "servicenow.create-record", "artifacts.write"],
    ["connector:jira", "connector:slack"],
    """
    # Sales escalate and record

    ## When to use
    Anything the desk cannot resolve: CRM structural changes, legal terms, finance exceptions, product questions with no knowledge answer, provisioning delays, a rep asking for a ticket.

    ## Method
    1. **Do not re-ask.** The deal facts from intake and the evidence the method skill read go into the request verbatim, with the record ids.
    2. **Where it goes** (from knowledge: `search_knowledge` "sales ops request process", "legal review request", "deal desk channel"):
       - Sales ops / CRM / product → Jira: `jira.search-issues` for an existing ticket first (JQL `project = <key> AND text ~ "<account>" AND statusCategory != Done`), then the create path the org uses (a Jira create operation is not in the connector yet: draft the ticket text with `artifacts.write` and post it to the channel, and file the gap).
       - Approvals and legal → the named Slack channel with `slack.post-message`, two lines and the artifact link, only with the rep's yes.
       - IT-style requests (a licence, a tool) → ServiceNow `create-record` on sc_request, or hand over to the IT desk.
    3. **Priority** from the deal: close date within 7 days and amount above the threshold in knowledge → urgent; say why in the request.
    4. **Tell the rep** the ticket or post, who owns it, and what to expect.
    5. **Record** `case:<account>:<ticket>` and update the `intake:` note. **File gaps** for each missing capability with the case as evidence.

    ## What done looks like
    The right place, a complete request with record ids, a priority with reasoning, the rep told, memory and gaps written.
    """,
    {})

# ═════════════════════════════ HR DESK ═════════════════════════════
hr_connectors = {}

workday_authz = {"authorize_url": "https://{auth_host}/{tenant}/authorize", "token_url": "https://{tenant_host}/ccx/oauth2/{tenant}/token",
                 "scopes": "Staffing Time_Off_and_Leave Personal_Data", "client_id": "{credentials.client_id}", "client_secret": "{credentials.client_secret}", "extra_params": {}}
hr_connectors["workday"] = manifest(
    "workday", "Workday HCM", "Workday REST API: a worker's record (job, manager, location, hire date, status), time-off balances and requests, job change history, and the time-off request a desk submits on a person's behalf. Written for the Workday REST services (common, absenceManagement, staffing); Workday SOAP (WWS) is out of scope.",
    "https://community.workday.com/sites/default/files/file-hosting/restapi/", "https://{tenant_host}/ccx/api",
    spec("Workday Connection", {"tenant_host": plain("Services host", 0, "The tenant's API host.", "wd2-impl-services1.workday.com"),
                                "auth_host": plain("Authorization host", 1, "The host people sign in to for the OAuth authorization step.", "impl.workday.com"),
                                "tenant": plain("Tenant", 2, "The tenant name.", "acme_preview"),
                                "credentials": creds_oauth_app("API client", "Register API Client for Integrations (Workday task) with the scopes Staffing, Time Off and Leave, Personal Data; the refresh token is issued when you authorize.")},
         ["tenant_host", "auth_host", "tenant", "credentials"]),
    [op("check-connection", "Verify the authorization is accepted.", "GET", "/common/v1/{tenant}/workers?limit=1", "read_only", auth=BEARER),
     op("search-workers", "Find workers by name or email.", "GET", "/common/v1/{tenant}/workers", "read_only", {"search": P("Name, email or employee id"), "limit": P("Rows", "integer", default=10)}, ["search"], auth=BEARER),
     op("get-worker", "One worker: primaryJob (position, manager, location, businessTitle), hireDate, status, workerType, primaryWorkEmail.", "GET", "/common/v1/{tenant}/workers/{id}", "read_only", {"id": P("Worker id")}, ["id"], auth=BEARER),
     op("get-worker-time-off-balances", "Time-off plan balances for a worker (vacation, sick, PTO) as of a date.", "GET", "/absenceManagement/v1/{tenant}/balances", "read_only", {"worker": P("Worker id"), "effective": P("As-of date YYYY-MM-DD")}, ["worker"], auth=BEARER),
     op("list-worker-time-off-requests", "A worker's time-off requests with status (Successfully Completed, In Progress, Denied) and the approver.", "GET", "/absenceManagement/v1/{tenant}/workers/{id}/requestTimeOff", "read_only", {"id": P("Worker id")}, ["id"], auth=BEARER),
     op("list-eligible-absence-types", "Which time-off types the worker may request (and their units).", "GET", "/absenceManagement/v1/{tenant}/workers/{id}/eligibleAbsenceTypes", "read_only", {"id": P("Worker id")}, ["id"], auth=BEARER),
     op("request-time-off", "Submit a time-off request for the worker (it routes to their approver like a self-service request). Only with the person's explicit yes in the conversation.", "POST", "/absenceManagement/v1/{tenant}/workers/{id}/requestTimeOff", "compensatable",
        {"id": P("Worker id"), "days": P('[{"date": "2026-10-03", "dailyQuantity": 8, "timeOffType": {"id": "<eligible type id>"}}]', "array")}, ["id", "days"], auth=BEARER),
     op("list-worker-job-changes", "Job change history (promotion, transfer, manager change) with effective dates.", "GET", "/staffing/v1/{tenant}/workers/{id}/jobChanges", "read_only", {"id": P("Worker id")}, ["id"], auth=BEARER)],
    "check-connection", workday_authz)

bamboo_auth = [{"style": "basic", "username": "{credentials.api_key}", "password": "x"}]
hr_connectors["bamboohr"] = manifest(
    "bamboohr", "BambooHR", "BambooHR API: the employee directory and one employee's record (job title, department, supervisor, hire date, location, status), time-off requests and balances, and who is out. For organisations on BambooHR rather than Workday.",
    "https://documentation.bamboohr.com/reference", "https://api.bamboohr.com/api/gateway.php/{company}/v1",
    spec("BambooHR Connection", {"company": plain("Company subdomain", 0, "The part before .bamboohr.com.", "acme"),
                                 "credentials": {"type": "object", "title": "API key", "description": "Generated by an HR admin under their avatar → API Keys; the key's access is that person's permissions.", "required": ["api_key"], "additionalProperties": False, "rusty_order": 1, "properties": {"api_key": secret("API key", 0)}}}, ["company", "credentials"]),
    [op("check-connection", "Verify the key is accepted.", "GET", "/employees/directory", "read_only", auth=bamboo_auth),
     op("employee-directory", "Everyone: id, displayName, jobTitle, department, location, supervisor, workEmail.", "GET", "/employees/directory", "read_only", auth=bamboo_auth),
     op("get-employee", "One employee's fields.", "GET", "/employees/{id}", "read_only", {"id": P("Employee id"), "fields": P("Comma-separated field names", default="firstName,lastName,jobTitle,department,division,location,supervisor,hireDate,status,workEmail,employmentHistoryStatus")}, ["id"], auth=bamboo_auth),
     op("list-time-off-requests", "Time-off requests in a window, optionally for one employee or one status (approved, denied, superceded, requested, canceled).", "GET", "/time_off/requests", "read_only", {"start": P("YYYY-MM-DD"), "end": P("YYYY-MM-DD"), "employeeId": P("Employee id"), "status": P("Status filter")}, ["start", "end"], auth=bamboo_auth),
     op("time-off-balance", "Projected time-off balances for an employee as of a date, per policy.", "GET", "/employees/{id}/time_off/calculator", "read_only", {"id": P("Employee id"), "end": P("As-of date YYYY-MM-DD")}, ["id", "end"], auth=bamboo_auth),
     op("whos-out", "Who is out (approved time off and holidays) in a window.", "GET", "/time_off/whos_out", "read_only", {"start": P("YYYY-MM-DD"), "end": P("YYYY-MM-DD")}, auth=bamboo_auth)],
    "check-connection")

gh_auth = [{"style": "basic", "username": "{credentials.api_key}", "password": ""}]
hr_connectors["greenhouse"] = manifest(
    "greenhouse", "Greenhouse Recruiting", "Greenhouse Harvest API: candidates and their applications, the stage each application is in, scheduled interviews, offers, and open jobs — so hiring managers, recruiters and candidates get a status that comes from the system, not from memory.",
    "https://developers.greenhouse.io/harvest.html", "https://harvest.greenhouse.io/v1",
    spec("Greenhouse Connection", {"credentials": {"type": "object", "title": "Harvest API key", "description": "Configure → Dev Center → API Credential Management → Harvest API key with Candidates, Applications, Jobs, Offers and Scheduled Interviews permissions (read).", "required": ["api_key"], "additionalProperties": False, "rusty_order": 0, "properties": {"api_key": secret("Harvest API key", 0)}}}, ["credentials"]),
    [op("check-connection", "Verify the key is accepted.", "GET", "/jobs?per_page=1", "read_only", auth=gh_auth),
     op("list-candidates", "Find candidates by email or by last update.", "GET", "/candidates", "read_only", {"email": P("Candidate email"), "updated_after": P("ISO-8601"), "per_page": P("Rows", "integer", default=10)}, auth=gh_auth),
     op("get-candidate", "One candidate with their application ids, recruiter and coordinator.", "GET", "/candidates/{id}", "read_only", {"id": P("Candidate id")}, ["id"], auth=gh_auth),
     op("get-application", "One application: job, status (active, rejected, hired), current_stage, source, applied_at, rejection reason.", "GET", "/applications/{id}", "read_only", {"id": P("Application id")}, ["id"], auth=gh_auth),
     op("list-scheduled-interviews", "Interviews scheduled for an application: interviewers, start/end, status, location.", "GET", "/applications/{id}/scheduled_interviews", "read_only", {"id": P("Application id")}, ["id"], auth=gh_auth),
     op("get-current-offer", "The application's current offer: status (unresolved, accepted, rejected, deprecated), starts_at, sent_at.", "GET", "/applications/{id}/offer/current_offer", "read_only", {"id": P("Application id")}, ["id"], auth=gh_auth),
     op("list-jobs", "Open jobs with hiring team and openings.", "GET", "/jobs", "read_only", {"status": P("open, closed, draft", default="open"), "per_page": P("Rows", "integer", default=20)}, auth=gh_auth)],
    "check-connection")

sf_auth = [{"style": "basic", "username": "{credentials.username}", "password": "{credentials.password}"}]
hr_connectors["sap-successfactors"] = manifest(
    "sap-successfactors", "SAP SuccessFactors", "SuccessFactors OData API (Employee Central): a person's user record and employment (job, manager, department, location, hire date), time accounts and balances, recorded absences, and personal data fields the HR desk may read. For organisations on SuccessFactors rather than Workday.",
    "https://help.sap.com/docs/SAP_SUCCESSFACTORS_PLATFORM/d599f15995d348a1b45ba5603e2aba9b", "https://{api_host}/odata/v2",
    spec("SuccessFactors Connection", {"api_host": plain("API host", 0, "The data centre API host.", "api4.successfactors.com"),
                                       "credentials": {"type": "object", "title": "API user", "description": "A technical user with the OData API permissions (username@companyId form) and its password, or configure OAuth SAML bearer outside this connector.", "required": ["username", "password"], "additionalProperties": False, "rusty_order": 1, "properties": {"username": secret("Username (user@companyId)", 0), "password": secret("Password", 1)}}}, ["api_host", "credentials"]),
    [op("check-connection", "Verify the API user is accepted.", "GET", "/User?$top=1&$format=json", "read_only", auth=sf_auth),
     op("search-users", "Find a person by name or email.", "GET", "/User", "read_only", {"$filter": P("e.g. email eq 'jdoe@acme.com' or lastName eq 'Doe'"), "$select": P("Fields", default="userId,username,firstName,lastName,email,status,department,division,location,title,managerId,hireDate"), "$format": P("json", default="json"), "$top": P("Rows", "integer", default=10)}, ["$filter"], auth=sf_auth),
     op("get-employment", "Employment and job (EmpJob): company, department, position, manager, location, event reason, effective dates.", "GET", "/EmpJob", "read_only", {"$filter": P("e.g. userId eq '1001'"), "$format": P("json", default="json"), "$top": P("Rows", "integer", default=5)}, ["$filter"], auth=sf_auth),
     op("get-time-accounts", "Time accounts (vacation, sick) with balances and accrual.", "GET", "/TimeAccount", "read_only", {"$filter": P("e.g. userId eq '1001'"), "$expand": P("Related", default="timeAccountDetails"), "$format": P("json", default="json")}, ["$filter"], auth=sf_auth),
     op("list-absences", "Recorded absences (EmployeeTime): type, start, end, approval status.", "GET", "/EmployeeTime", "read_only", {"$filter": P("e.g. userId eq '1001' and startDate ge datetime'2026-01-01T00:00:00'"), "$format": P("json", default="json"), "$top": P("Rows", "integer", default=20)}, ["$filter"], auth=sf_auth)],
    "check-connection")

HR_SYS = ["workday.search-workers", "workday.get-worker", "bamboohr.employee-directory", "bamboohr.get-employee", "sap-successfactors.search-users", "sap-successfactors.get-employment"]
CONFIDENTIAL = """
## Confidentiality that does not bend
- Answer only about the person you are talking to, or about their direct reports when the record shows they are the manager. Anyone else's pay, leave, performance, health or personal data is never disclosed, summarised or confirmed, even to a manager two levels up; route them to their HR partner.
- Never write pay figures, bank details, health information or ID numbers into memory, tickets or artifacts. Note that a topic was discussed, not the numbers.
- Policy answers come from the handbook in knowledge with the section cited; when the handbook is silent, say so and route. Never invent policy, eligibility or legal entitlement.
- Employee-relations matters (complaints, harassment, discipline, performance concerns, medical accommodation) are not diagnosed here: acknowledge, do not advise, hand to the HR partner through `hr-escalate-and-record` the same day, and tell the person that is what happened.
"""
hr_skills = {}

hr_skills["hr-question-intake"] = skill(
    "hr-question-intake",
    "Use at the start of every HR support conversation: identify who is asking and their relationship to the subject (self, manager, other), classify the request — policy question, time off and leave, pay and benefits, personal or job data change, onboarding or offboarding, recruiting status, employee relations — apply the confidentiality rules before anything is read, and route to the right method skill or straight to escalation.",
    COMMON + HR_SYS + ["okta.get-user", "microsoft-365.get-user"],
    ["connector:workday"],
    """
    # HR question intake

    ## When to use
    The first turn of any HR request, and again when the topic changes.
    """ + CONFIDENTIAL + """
    ## Method
    1. **Who is asking, about whom.** The person's identity comes from the run (the account they signed in with), never from what they type. Read their record (`workday.get-worker`, `bamboohr.get-employee` or `sap-successfactors.get-employment`) to know their job, manager and location; that decides which policies apply (country, entity, worker type) and whether they manage the subject.
    2. **Recall** `memory.recall` for open threads with this person.
    3. **Classify** into one class and name the skill:
       - "What is the policy on…", eligibility, how-to → `policy-question-with-citations`
       - Balance, request, cancel, parental/medical/sabbatical leave → `time-off-and-leave`
       - Pay slip, deductions, tax forms, benefits enrolment, comp questions → `benefits-payroll-and-compensation-questions`
       - Address, name, emergency contact, bank, manager, title, transfer, employment verification → `employee-data-and-job-changes`
       - New hire, first day, equipment, leaver, last day, access → `onboarding-and-offboarding-coordination`
       - Candidate, interview, offer status → `recruiting-and-interview-status`
       - Complaint, conflict, performance, discipline, accommodation, anything distressing → `hr-escalate-and-record` now, with care.
    4. **Read the taking-over skill** with `skills.read` and continue in the same conversation.
    5. **Record** `hr-intake:<person>:<yyyy-mm-dd>` with the class only (no details for pay, health or relations topics).
    """ + GAP + """
    ## What done looks like
    The asker and the subject established from the record, one class, confidentiality applied, the taking-over skill started, a minimal note.
    """,
    {})

hr_skills["policy-question-with-citations"] = skill(
    "policy-question-with-citations",
    "Use for any 'what is the policy', 'am I eligible', 'how do I' question about employment: answers only from the handbook and policy documents in knowledge, quotes the section, applies the person's country, entity and worker type from their record, and says plainly when the documents do not answer — routing to the HR partner instead of guessing.",
    COMMON + HR_SYS,
    ["tool:search_knowledge", "connector:workday"],
    """
    # Policy question with citations

    ## When to use
    Any question whose answer should be in the handbook, a policy, a benefits guide or a country supplement.
    """ + CONFIDENTIAL + """
    ## Method
    1. **Scope the person**: country/entity, worker type (employee, contractor, intern), tenure and level from their record. Policies differ by these; say which scope you answered for.
    2. **Search** `search_knowledge` with the question's nouns and the scope (e.g. "parental leave Germany", "remote work policy contractor"). Read the top passages; prefer the newest version of a document when versions conflict, and say the version and date.
    3. **Answer in this shape**: the rule in one or two sentences; the quoted sentence from the document with its title and section; the condition that applies to this person (eligibility, notice period, approvals); the next step (the self-service task, the form, who approves).
    4. **When the documents are silent or contradict**: say exactly that, quote both if two conflict, and route the question to the HR partner through `hr-escalate-and-record`; also file a gap so the handbook owner sees the missing answer.
    5. **Record** `policy:<topic>:<scope>` with the document and section that answered, so the next person gets it in one turn; nothing about the asker beyond the class.
    """ + GAP + """
    ## What done looks like
    An answer with a quoted section and document title, scoped to the person's country and worker type, or an honest "not in our documents" with a routed question and a gap.
    """,
    {"references/answer-shape.md": """
    # Answer shape
    **Rule.** Employees in <country> accrue <n> days of annual leave per year, pro-rated in the first year.
    **From the handbook.** "<quoted sentence>" — <Document title>, section <x.y>, version <date>.
    **For you.** As a full-time employee hired <date>, your first-year entitlement is pro-rated; the record shows <balance> as of <date>.
    **Next step.** Request it in <system> → <task>; your manager approves within <n> days per the policy.
    **Not covered.** <what the documents do not say>; I have asked your HR partner and will pass on the answer.
    """})

hr_skills["time-off-and-leave"] = skill(
    "time-off-and-leave",
    "Use for balances, requests, cancellations and questions about vacation, sick time, public holidays and formal leaves (parental, medical, bereavement, sabbatical, jury). Reads the person's balances and requests from the HR system, applies eligibility and notice rules from the handbook with citations, submits a simple time-off request only with the person's explicit yes, and hands formal leaves to the leave administrator with a complete case.",
    COMMON + HR_SYS + ["workday.get-worker-time-off-balances", "workday.list-worker-time-off-requests", "workday.list-eligible-absence-types", "workday.request-time-off", "workday.list-worker-job-changes", "bamboohr.list-time-off-requests", "bamboohr.time-off-balance", "bamboohr.whos-out", "sap-successfactors.get-time-accounts", "sap-successfactors.list-absences"],
    ["connector:workday", "tool:search_knowledge"],
    """
    # Time off and leave

    ## When to use
    "How much vacation do I have", "did my request go through", "who approves", "I need parental/medical leave", "can I cancel", "is my team out next week" (managers, for their reports only).
    """ + CONFIDENTIAL + """
    ## Method
    1. **Read the record**: balances as of today (`workday.get-worker-time-off-balances`, `bamboohr.time-off-balance` or `sap-successfactors.get-time-accounts`) and the requests with their status and approver (`list-worker-time-off-requests`, `bamboohr.list-time-off-requests`, `list-absences`). Quote the balance with its as-of date and the unit (days or hours).
    2. **Read the rule** from knowledge for the type asked: accrual, carry-over and expiry, notice period, minimum increments, blackout periods, public holidays by location, who approves. Cite the section.
    3. **Simple time off** (vacation, sick day, personal day):
       - The person prefers self-service: tell them the task. If they ask the desk to submit, confirm the exact dates, type and quantity back to them and submit with `workday.request-time-off` only after their explicit yes in the conversation; report the request id and the approver it went to.
       - A request "stuck": the approver from the record; if the approver is out (`bamboohr.whos-out` or the manager's own requests), say so and route to the delegate the policy names.
    4. **Formal leaves** (parental, medical, disability, bereavement, sabbatical, military, jury): the desk explains eligibility and the steps from the handbook, gathers the facts the leave administrator needs (type, expected dates, whether a medical certificate is required per policy — never ask for the diagnosis), and opens the case through `hr-escalate-and-record`. It does not decide eligibility for statutory leave.
    5. **Managers** asking about their team: only their direct reports per the record, only approved time off, never the reason for a sick absence.
    6. **Record** `leave:<person>` with the type and the request id or the case number; no medical detail.
    """ + GAP + """
    ## What done looks like
    Balance and requests quoted from the system with dates, the rule cited, a request submitted only with an explicit yes, formal leaves handed over as a complete case, no medical detail anywhere.
    """,
    {"references/leave-types.md": """
    # Leave types — what the desk handles and what it hands over
    | Type | Desk handles | Hands over |
    |---|---|---|
    | Vacation / PTO | Balance, rules, submit with yes, stuck approvals | Disputes about accrual |
    | Sick day (short) | Rules, how to record, certificate threshold (days) per policy | Anything beyond the short-term threshold → medical leave |
    | Public holidays | The calendar by location from knowledge | — |
    | Parental | Eligibility summary with citation, steps, timeline | The case: leave administrator confirms statutory entitlement, pay, dates |
    | Medical / disability | Steps, forms, who to contact | The case; never the diagnosis |
    | Bereavement | Rule and days per policy, how to record | Extensions |
    | Sabbatical / unpaid | Eligibility and approval chain | The request itself (manager and HR partner) |
    | Jury / military | Rule, how to record, pay continuation per policy | Documentation |
    """})

hr_skills["benefits-payroll-and-compensation-questions"] = skill(
    "benefits-payroll-and-compensation-questions",
    "Use for pay slips, deductions, tax forms, pay dates, benefits enrolment and changes, pension and equity questions, and compensation review timing. Explains the general rules from the benefits guide and payroll calendar with citations, points to self-service for personal figures, and opens a payroll or benefits case with the right team for anything specific to the person's pay — never quoting, estimating or correcting pay in chat.",
    COMMON + HR_SYS,
    ["tool:search_knowledge", "connector:workday"],
    """
    # Benefits, payroll and compensation questions

    ## When to use
    Anything about money the person earns or the benefits they hold.
    """ + CONFIDENTIAL + """
    ## Method
    1. **General or personal?** "When is payday", "how does the pension match work", "what is the enrolment window" are general: answer from knowledge with the citation and the person's scope (country, entity). "Why is my pay lower this month", "my deduction is wrong", "I was not paid overtime" are personal: the desk does not read or compute pay; it opens a payroll case with the facts (pay period, what they expected, what they see) through `hr-escalate-and-record` and tells them where to see their pay slip in self-service.
    2. **Benefits enrolment and life events**: the windows and the qualifying events (marriage, birth, address move) from the guide with citations; the self-service task; the deadline in days from today. A missed window is a case for the benefits team, not a promise.
    3. **Equity, bonus, compensation review**: the plan rules and the calendar from knowledge; individual amounts and decisions are with the manager and the compensation team; route the person there, do not speculate.
    4. **Tax forms and year-end**: where the forms are in self-service and when they are issued (from the payroll calendar); corrections are a payroll case.
    5. **Record** the class and the case number only.
    """ + GAP + """
    ## What done looks like
    General questions answered with citations and scope; personal pay matters opened as a case with the facts and no figures in the note; the person told where to see their own data.
    """,
    {})

hr_skills["employee-data-and-job-changes"] = skill(
    "employee-data-and-job-changes",
    "Use for changes to a person's own data (address, name, emergency contact, bank details, work location), manager-initiated job changes (title, manager, transfer, hours, termination) and employment verification requests. Reads the current record, explains the self-service task and the approval chain from the handbook, never takes bank or ID details in chat, and packages manager-initiated changes and verifications for the HR operations team.",
    COMMON + HR_SYS + ["workday.list-worker-job-changes", "artifacts.write"],
    ["connector:workday", "tool:search_knowledge"],
    """
    # Employee data and job changes

    ## When to use
    "Update my address", "I got married and changed my name", "change my bank account", "I need an employment letter", and from managers: "promote / transfer / change the manager of <report>", "reduce hours", "end a contract".
    """ + CONFIDENTIAL + """
    ## Method
    1. **Own data** (address, phone, emergency contact, preferred name): read the current value from the record, tell the person the self-service task, and what else it triggers (tax, benefits, payroll location) per knowledge. **Bank details, national ID, passport, legal name changes**: never accepted in chat; self-service only or the secure form the policy names, with the documents it requires; say why (fraud prevention).
    2. **Job changes from a manager**: confirm they manage the person (`workday.get-worker` primaryJob.manager). Explain the business process (Workday: Change Job / Request Compensation Change; SuccessFactors: Job Information change) and the approvals it will route through (from knowledge). Gather the facts the process needs — effective date, new title/position, new manager, location, hours, reason — and package them with `artifacts.write` for the HR operations team through `hr-escalate-and-record`. Compensation changes: the amount goes into the system by the manager, not through the desk.
    3. **Terminations and end of contract**: the desk explains the process and the notice rules from knowledge; the action is HR's. Voluntary resignations from the person: acknowledge, explain notice and the leaver steps, hand to `onboarding-and-offboarding-coordination`.
    4. **Employment verification letters**: the template and the fields the policy allows (employment dates, title; salary only with the person's written consent per policy); the request goes to HR operations as a case; a third party asking is routed to the verification service the policy names.
    5. **Record** `data-change:<person>` with the class and the case number; never the values.
    """ + GAP + """
    ## What done looks like
    The current record read, the right self-service task or business process named with approvals, sensitive values kept out of chat, manager changes packaged as a case with effective date and reason.
    """,
    {})

hr_skills["onboarding-and-offboarding-coordination"] = skill(
    "onboarding-and-offboarding-coordination",
    "Use for new hires (before day one, first week) and leavers (notice given, last day, after): runs the organisation's checklist from knowledge, reads the person's state in HR and identity systems (record created, accounts provisioned, equipment ordered, access removed), raises the IT requests the checklist needs, and reports what is done, pending and blocked with owners — coordinating, never performing the HR or security actions itself.",
    COMMON + HR_SYS + ["workday.list-worker-job-changes", "okta.get-user", "okta.list-user-groups", "okta.list-user-app-links", "microsoft-365.get-user", "google-workspace-admin.get-user", "servicenow.list-records", "servicenow.list-catalog-items", "servicenow.order-catalog-item", "servicenow.create-record", "artifacts.write"],
    ["connector:workday", "connector:okta", "connector:servicenow", "tool:search_knowledge"],
    """
    # Onboarding and offboarding coordination

    ## When to use
    A hiring manager or HR asks "is <new hire> ready for Monday"; a new hire asks "what happens on day one"; a manager or HR says "<person> is leaving on <date>"; a leaver asks about their last day.
    """ + CONFIDENTIAL + """
    ## Method
    1. **Read the checklist** from knowledge (`search_knowledge` "onboarding checklist", "offboarding checklist", by country and worker type). It names the steps, owners and lead times; the desk follows it, it does not invent steps.
    2. **Read the state**, one system at a time, quoting each:
       - HR record exists with start/end date and manager (`workday.get-worker` / `bamboohr.get-employee` / `sap-successfactors.get-employment`).
       - Identity: account created or deactivated (`okta.get-user` status, `microsoft-365.get-user` accountEnabled, `google-workspace-admin.get-user` suspended); groups and app links match the role (`okta.list-user-groups`, `list-user-app-links`).
       - Equipment and access requests: `servicenow.list-records` on sc_request / sc_req_item for the person (laptop, badge, software).
    3. **Raise what the checklist says the desk raises** — typically the IT equipment and access catalog items (`servicenow.list-catalog-items` then `order-catalog-item` on the manager's behalf with their yes), with the start date and the role's standard bundle from knowledge. Security actions (deactivating accounts, revoking access, wiping devices) are not the desk's: they are the identity and security teams', triggered by the HR termination process; the desk confirms they happened and chases when they did not.
    4. **Report** with `artifacts.write` (`onboarding-<person>` / `offboarding-<person>`): each step done / pending / blocked, the owner, the date, what the manager must still do (buddy, first-week plan, equipment return for leavers). Update the same artifact as things change.
    5. **Leaver specifics**: notice and last-day rules from knowledge, the exit interview scheduling, benefits continuation information from the guide with citations, the equipment return process; the person's questions about final pay go to `benefits-payroll-and-compensation-questions`.
    6. **Record** `lifecycle:<person>` with the phase and the artifact name.
    """ + GAP + """
    ## What done looks like
    The organisation's checklist applied, every step's state read from a system or filed as a gap, the desk's own requests raised with the manager's yes, an artifact that shows done / pending / blocked with owners.
    """,
    {"references/checklist-default.md": """
    # Default checklist (the organisation's own in knowledge wins)
    ## Onboarding
    | When | Step | Owner |
    |---|---|---|
    | Offer accepted | HR record created with start date, manager, location | HR ops |
    | T-10 days | Identity account created; groups for the role | Identity/IT |
    | T-10 days | Laptop and peripherals ordered (standard bundle by role) | Manager via IT catalog |
    | T-5 days | Badge, desk, parking; welcome email with day-one logistics | Facilities, HR |
    | T-3 days | Software and app access per role; MFA enrolment instructions | IT |
    | Day 1 | Device handed over, MFA enrolled, orientation, buddy assigned | IT, HR, manager |
    | Week 1 | Payroll and benefits enrolment completed; first-week plan | Person, manager |
    ## Offboarding
    | When | Step | Owner |
    |---|---|---|
    | Notice given | HR record updated with last day; manager notified; knowledge transfer plan | HR ops, manager |
    | Last day −5 | Equipment return arranged; exit interview scheduled | IT, HR |
    | Last day | Accounts disabled at the agreed time; badge returned; devices returned | Identity, facilities, IT |
    | Last day +1 | Mailbox and files handled per policy (delegation/retention); final pay per payroll calendar | IT, payroll |
    | +30 days | Accounts deleted per retention; benefits continuation information sent | Identity, benefits |
    """})

hr_skills["recruiting-and-interview-status"] = skill(
    "recruiting-and-interview-status",
    "Use when a hiring manager, interviewer, recruiter or candidate asks about a requisition or an application: where it stands, what interviews are scheduled, whether an offer went out and its state, what is blocking. Reads the applicant tracking system (Greenhouse) and answers only what the asker is entitled to see — a candidate about themselves, a hiring team about their own job — and routes scheduling changes and offer decisions to the recruiter.",
    COMMON + ["greenhouse.list-candidates", "greenhouse.get-candidate", "greenhouse.get-application", "greenhouse.list-scheduled-interviews", "greenhouse.get-current-offer", "greenhouse.list-jobs", "workday.search-workers", "bamboohr.employee-directory"],
    ["connector:greenhouse"],
    """
    # Recruiting and interview status

    ## When to use
    "Where is my application", "when is my next interview", "did the offer go out", "how many candidates are in stage X for my job", "who is on the panel".

    ## Who may see what
    - A candidate: only their own applications, by the email they applied with; stage and next scheduled interview, never interviewer feedback, scorecards or other candidates.
    - The hiring team of a job (hiring manager, recruiter, coordinator, listed interviewers per `greenhouse.list-jobs` hiring_team): that job's pipeline and applications.
    - Anyone else: nothing about candidates; route to the recruiter.
    Interview feedback and rejection reasons are never relayed to candidates; the recruiter communicates decisions.

    ## Method
    1. **Establish the asker's role** from the HR record (employee → hiring team member of which jobs; external email → candidate).
    2. **Read** `greenhouse.list-candidates` by email → `get-candidate` → `get-application` (status, current_stage, applied_at, source) → `list-scheduled-interviews` (interviewers, times, status) → `get-current-offer` (status, sent_at, starts_at) as the question needs.
    3. **Answer** with dates and stage names as the system has them; "in review" is not an answer, "Onsite stage, panel scheduled 24 Sep 10:00–12:00 with three interviewers, debrief pending" is.
    4. **Changes** (reschedule, add an interviewer, extend an offer, reject) are the recruiter's: package the request with the application id and route through `hr-escalate-and-record`; the desk does not write to the ATS.
    5. **Record** `recruiting:<job or application id>` with the state; nothing about candidate assessments.
    """ + GAP + """
    ## What done looks like
    The asker's entitlement checked, the state read from the ATS and quoted with dates, changes routed to the recruiter, nothing evaluative disclosed.
    """,
    {})

hr_skills["hr-escalate-and-record"] = skill(
    "hr-escalate-and-record",
    "Use when an HR conversation needs the HR partner, payroll, benefits, leave administration, HR operations, recruiting or legal; for any employee-relations, accommodation or wellbeing matter; when the documents do not answer; or when the person asks for a case. Opens the HR case in the case system with the facts already gathered and the confidentiality the topic needs, tells the person what happens next, records the minimum in memory, and files gaps when the desk lacked a tool or connection.",
    COMMON + ["servicenow.create-record", "servicenow.list-records", "servicenow.update-record", "servicenow.aggregate", "slack.post-message", "artifacts.write"],
    ["connector:servicenow"],
    """
    # HR escalate and record

    ## When to use
    Any matter the method skills hand over, any employee-relations or wellbeing matter, any unanswered policy question, any request for a case.
    """ + CONFIDENTIAL + """
    ## Method
    1. **Do not re-ask**; carry the facts the method skill gathered. For relations and wellbeing matters carry only what the person chose to say, in their words, and nothing you inferred.
    2. **Where**: the HR case system from knowledge (`search_knowledge` "HR case management"): ServiceNow HR Service Delivery (`servicenow.create-record` on the HR case table the org uses, typically sn_hr_core_case, with hr_service and the topic) or, when there is none, the HR partner's queue named in knowledge. Payroll and benefits cases go to their own service in the same system; leave cases to leave administration; recruiting to the recruiter; legal via the HR partner, never directly.
    3. **Sensitivity**: mark the case with the confidentiality the system supports (a restricted HR case type for relations, medical, investigation); never post such cases to Slack. Ordinary operations cases (address change, letter request) may be summarised to the HR ops channel with `slack.post-message` if knowledge names one.
    4. **Urgency**: safety, harassment, medical emergency, a resignation with a same-week last day, a payroll error before a pay run → same day; say so in the case and to the person. Everything else follows the service's SLA from knowledge.
    5. **Tell the person** the case number, who will contact them, by when, and how to reach the HR partner or the employee assistance programme directly if the matter is distressing (from knowledge).
    6. **Record** `hr-case:<person>:<case>` with the class and the case number only. **File gaps** for missing tools or connections with the case as evidence.

    ## What done looks like
    The right service, a case with the facts in the person's words, the right confidentiality, urgency stated, the person told the number and the next contact, a minimal note, gaps filed.
    """,
    {})

write_plugin(f"{CATALOG}/sales-desk", "sales-desk", "Sales Desk",
             "What sales support does for sellers and their managers — request intake, deal desk (pricing, discount, approvals), pipeline hygiene and forecast, account and contact briefs, contract and signature status, billing and renewal, escalation — as method skills with references, and the connectors they read from (DocuSign, Gong, Outreach) written from the vendors' API documentation, alongside the Salesforce, HubSpot, Stripe, Jira and Slack connectors already in the catalog. Connect what you have; each skill files a gap for what it could not read.",
             sales_connectors, sales_skills)
write_plugin(f"{CATALOG}/hr-desk", "hr-desk", "HR Desk",
             "What an HR help desk does for employees, managers and candidates — intake with confidentiality rules, policy answers with citations, time off and leave, benefits and payroll questions, employee data and job changes, onboarding and offboarding coordination, recruiting status, escalation to the HR partner — as method skills with references, and the connectors they read from (Workday, BambooHR, Greenhouse, SAP SuccessFactors) written from the vendors' API documentation, alongside Okta, Microsoft 365 and ServiceNow. Connect what you have; each skill files a gap for what it could not read.",
             hr_connectors, hr_skills)
