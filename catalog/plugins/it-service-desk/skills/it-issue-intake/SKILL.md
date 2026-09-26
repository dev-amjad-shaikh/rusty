---
name: it-issue-intake
description: Use at the start of every IT support conversation: turn 'my laptop is broken' into a classified issue with the facts a fix needs — who, which device, since when, what changed, what exactly fails — and route it to the right method skill (laptop, network, app, sign-in, SaaS access) or straight to escalation.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.list-records, servicenow.get-record, servicenow.aggregate, okta.get-user, microsoft-365.get-user, microsoft-intune.search-devices, jamf-pro.search-computers, google-workspace-admin.get-user
dependencies: connector:servicenow
license: MIT
---
    # IT issue intake

    ## When to use
    The first turn of any IT support request, before any fix is suggested. Also when a conversation drifts into a second problem.

    ## Method
    1. **Recall.** `memory.recall` with the person's name or email and with the device name if given. An open note from a previous conversation changes the first question: continue it rather than restart.
    2. **Gather the six facts.** Ask only for what is missing; one message, not six.
       - Who: the person and how they can be reached if the session drops.
       - Device: make, OS and version, managed or personal, hostname or serial if visible (Mac: Apple menu → About This Mac; Windows: Settings → System → About; ChromeOS: chrome://version; Linux: `hostnamectl`).
       - Since when, and whether it ever worked.
       - What changed just before: update, new network, new password, travel, new app, new peripheral.
       - The exact failure: the error text verbatim, a screenshot's words, what they clicked and what happened instead.
       - Blast radius: only them, their team, everyone (a quick `servicenow.aggregate` on incident by short_description over the last hour tells you if it is an outage; say so and stop diagnosing the individual).
    3. **Check the record.** If a management or identity connector is present, read the device (`microsoft-intune.search-devices`, `jamf-pro.search-computers`, `google-workspace-admin.get-user`) and the account (`okta.get-user` or `microsoft-365.get-user`) before asking the person for facts the system already holds. Quote what you read and its timestamp.
    4. **Classify** into exactly one primary class and name the skill that takes over:
       - Device (will not boot, slow, battery, disk full, peripheral, OS update) → `laptop-troubleshooting`
       - Network (no internet, Wi-Fi drops, VPN, DNS, a site unreachable) → `network-connectivity-troubleshooting`
       - Application (Zoom, Slack, Teams, Outlook, browser, Office) → `workplace-app-troubleshooting`
       - Sign-in (password, MFA, SSO, locked out) → `identity-and-sign-in-troubleshooting`
       - Access to a business system (ServiceNow, Salesforce, Workday, SAP: cannot see, cannot do, wrong role) → `saas-access-troubleshooting`
       - Not diagnosable remotely, hardware damage, security incident, data loss → `escalate-and-record` now.
    5. **Read the taking-over skill** with `skills.read` (name only) and follow its method in the same conversation. Do not restate the six facts to the person.
    6. **Record** the classified intake with `memory.remember` under the key `intake:<person>:<yyyy-mm-dd>` — one sentence: class, device, since when, the change that preceded it.

    ## Questions that save a round trip
    - "Does it happen on another network (phone hotspot)?" separates device from network.
    - "Does it happen in a private browser window / another app?" separates account from client.
    - "Did you change your password recently?" explains most sudden sign-in and mail failures.
    - "Is anyone near you seeing the same?" separates outage from individual.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    One class, the six facts, the record read (or the gap filed), the taking-over skill named and its method started, and one memory note written.
