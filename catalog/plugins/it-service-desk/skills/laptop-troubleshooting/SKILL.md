---
name: laptop-troubleshooting
description: Use for any problem on a person's computer — Mac, Windows, ChromeOS or Linux laptop: slow, will not boot, battery, disk full, updates, peripherals, printing. Reads the management record (Intune, Jamf, Google Admin) when connected, otherwise works from what the person reports, and follows the OS-specific reference for the exact checks and commands.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.list-records, servicenow.get-record, microsoft-intune.search-devices, microsoft-intune.get-device, microsoft-intune.get-device-compliance, microsoft-intune.sync-device, microsoft-intune.restart-device, jamf-pro.search-computers, jamf-pro.get-computer, jamf-pro.get-computer-mdm-capability, jamf-pro.update-inventory, jamf-pro.send-mdm-command, google-workspace-admin.search-chromeos-devices, google-workspace-admin.get-chromeos-device, web.fetch
dependencies: connector:microsoft-intune, connector:jamf-pro, connector:google-workspace-admin, tool:search_knowledge
license: MIT
---
    # Laptop troubleshooting

    ## When to use
    After intake classified the issue as a device problem, or when a person names their laptop as the thing that fails.

    ## Method
    1. **Identify the OS**, then read that reference with `skills.read` (reference `references/macos.md`, `references/windows.md`, `references/chromeos.md`, `references/linux.md`). It holds the checks, the commands to ask the person to run, and the known causes ranked by how often they are the answer.
    2. **Read the device record** before asking the person anything the record holds:
       - Windows or managed macOS/iOS in Intune: `microsoft-intune.search-devices` by user or name, then `get-device` and `get-device-compliance`. Note lastSyncDateTime (a device that has not synced in days is not getting policy or updates), osVersion, freeStorageSpaceInBytes, complianceState and the failing setting.
       - Mac in Jamf: `jamf-pro.search-computers` by user, then `get-computer`. Note lastContactTime, OS version, available disk, FileVault state, the pending or failed profiles, installed apps and their versions.
       - ChromeOS: `google-workspace-admin.search-chromeos-devices` by user, then `get-chromeos-device`. Note lastSync, osVersion, status, diskVolumeReports, systemRamFreeReports.
       Quote every value you use with its timestamp. A stale record is a finding in itself (the device is not talking to management).
    3. **Rank causes** for the reported symptom using the reference's table and the record: disk under 10% free, OS two majors behind, a pending restart, a failed profile, a battery at end of life, a recent driver or agent update.
    4. **Fix in order of least disruption**: setting or cache first, then restart, then a management action (`sync-device`, `update-inventory`, `restart-device`, `send-mdm-command` RestartDevice), then reinstall or re-enrol, then hardware service. Any management action that interrupts the person needs their yes in the conversation first.
    5. **Verify** with the person: the exact thing that failed, tried again. Not "does it seem better".
    6. **Record** with `memory.remember`: `device:<hostname or serial>` — the cause found and the fix that worked, one sentence with the date. Next time the same device comes back, step 1 starts from it.
    7. If two rounds of fixes fail, or the reference says "hardware", hand over to `escalate-and-record` with the facts gathered.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The OS reference consulted, the record read (or gap filed), the cause named with evidence, the fix verified by the person, and the device note written.
