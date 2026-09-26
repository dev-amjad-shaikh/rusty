---
name: escalate-and-record
description: Use when a support conversation cannot be resolved remotely, needs a team with different access (network, security, HR, SAP basis, hardware service), is a security incident, or when two rounds of fixes failed. Raises the right ticket in ServiceNow with the facts already gathered (never asking the person twice), sets priority from impact and urgency, records the case in memory, and files a gap when the desk lacked a tool or a connection.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.list-records, servicenow.get-record, servicenow.create-incident, servicenow.create-record, servicenow.update-record, servicenow.list-catalog-items, servicenow.order-catalog-item, servicenow.aggregate
dependencies: connector:servicenow
license: MIT
---
# Escalate and record

## When to use
Any of: hardware damage or no power; data loss; a security incident (compromise, forwarding rule, phishing clicked, lost device); a fix that needs access the desk does not have; an outage affecting many; two rounds of fixes that failed; the person asks for a ticket.

## Method
1. **Do not re-ask.** Everything intake and the method skill gathered goes into the ticket: who, device (hostname/serial), OS and version, since when, what changed, exact error, what was tried and what each attempt showed, the evidence read from systems (quote with timestamps).
2. **Incident or request?** Something broken → `servicenow.create-incident`. Something wanted (access, licence, a replacement device, a new printer) → the catalog: `list-catalog-items`, then `order-catalog-item` with the person's yes; `create-record` on sc_request only when no item fits. A security matter → an incident with category Security and the security assignment group from knowledge (`search_knowledge` "security incident assignment").
3. **Priority** from impact × urgency, and say the words:
   - Impact: one person / a team / a site or everyone.
   - Urgency: work stopped with no workaround / degraded with a workaround / inconvenience.
   One person, work stopped, no workaround → P3 with a note; a team → P2; site-wide or security → P1. Follow the organisation's matrix if knowledge has one.
4. **Check for duplicates**: `servicenow.aggregate` incidents with the same short description in the last day; if an outage ticket exists, link the person to it (a child incident or a note) rather than a new P1.
5. **Write the ticket** so the next engineer starts where you stopped: short description in the form "<system> — <symptom> — <person/site>"; description with the facts in the order above; work notes with the evidence quoted.
6. **Tell the person** the ticket number, who it went to, the priority in plain words, and what to expect next. Offer a workaround if one exists.
7. **Record** `case:<person>:<ticket>` in memory with the class, cause so far, and the ticket number. Update the earlier `intake:` note with the ticket.
8. **File gaps** with `gaps.file` for anything the desk could not do because a tool or connection was missing, so the platform learns what to add. One gap per missing capability, with the ticket as evidence.

## What done looks like
The right record type raised with every fact already gathered, a priority with the reasoning, no duplicate of an outage, the person told the number and the next step, memory and gaps written.
