---
name: recruiting-and-interview-status
description: Use when a hiring manager, interviewer, recruiter or candidate asks about a requisition or an application: where it stands, what interviews are scheduled, whether an offer went out and its state, what is blocking. Reads the applicant tracking system (Greenhouse) and answers only what the asker is entitled to see — a candidate about themselves, a hiring team about their own job — and routes scheduling changes and offer decisions to the recruiter.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, greenhouse.list-candidates, greenhouse.get-candidate, greenhouse.get-application, greenhouse.list-scheduled-interviews, greenhouse.get-current-offer, greenhouse.list-jobs, workday.search-workers, bamboohr.employee-directory
dependencies: connector:greenhouse
license: MIT
---
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

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The asker's entitlement checked, the state read from the ATS and quoted with dates, changes routed to the recruiter, nothing evaluative disclosed.
