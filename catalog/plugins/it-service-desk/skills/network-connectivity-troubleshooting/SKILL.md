---
name: network-connectivity-troubleshooting
description: Use for no internet, Wi-Fi that drops, VPN that will not connect or stays connected without access, DNS and proxy failures, and 'this one site does not load'. A layered method (link, address, DNS, path, application) that isolates the failing layer with the person's own commands, reads the device record when a management connector is present, and checks vendor service health before blaming the laptop.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.list-records, servicenow.get-record, microsoft-intune.search-devices, microsoft-intune.get-device, jamf-pro.search-computers, jamf-pro.get-computer, google-workspace-admin.get-chromeos-device, microsoft-365.list-service-health, servicenow.aggregate, web.fetch
dependencies: connector:microsoft-intune, connector:jamf-pro, tool:search_knowledge
license: MIT
---
    # Network and connectivity troubleshooting

    ## When to use
    After intake classified the issue as network, or when the failure is "cannot reach" anything.

    ## Method — isolate the layer, top down is slower, bottom up is right
    1. **Blast radius first.** `servicenow.aggregate` incidents by short_description containing "network" or "VPN" in the last hour; `microsoft-365.list-service-health` if the unreachable thing is Microsoft. If many people are affected, say so, point them to the outage, and stop individual diagnosis.
    2. **Link.** Is the laptop connected to a network at all? Wi-Fi icon, Ethernet light. Wi-Fi name matches the intended one (not a neighbour, not a guest network). Signal: 1–2 bars is a link problem.
    3. **Address.** Did it get an address? An address starting 169.254 means DHCP failed. Mac: `ifconfig en0`; Windows: `ipconfig`; Linux: `ip a`; ChromeOS: chrome://network#state.
    4. **Gateway and DNS.** Ping the gateway (`ipconfig`/`netstat -nr` shows it). Then resolve a name: `nslookup example.com`. Then reach an IP without DNS: `ping 1.1.1.1`. IP works but names do not → DNS. Neither → path (captive portal, firewall, VPN).
    5. **Path.** `traceroute` / `tracert` to the destination; where does it stop? A captive portal answers everything with a redirect: open a plain http page (neverssl.com) to get the portal.
    6. **VPN.** Read `references/vpn.md`. The question "which VPN client and what does its status line say" comes before anything else.
    7. **Application.** One site or service fails while the rest work: it is the site, a proxy rule, a certificate (clock skew makes every certificate invalid: check the date), or an allow-list. Read `references/dns-proxy-and-one-site.md`.
    8. **Device record.** If a management connector is present, read the device: OS version and last sync (a Wi-Fi driver or VPN client two versions behind is a finding). Quote what you read.
    9. **Fix** at the failing layer only, least disruptive first: forget and rejoin Wi-Fi, renew DHCP, flush DNS, toggle VPN, restart, then driver/client update, then escalate to the network team with the traceroute.
    10. **Verify** the exact original failure, and **record** `network:<person>:<yyyy-mm-dd>` with the layer and the fix. If the same person reports the same layer twice in a month, say so and file a gap: the fix was a workaround.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The failing layer named with the command output that shows it, the fix applied at that layer, verified, and recorded.
