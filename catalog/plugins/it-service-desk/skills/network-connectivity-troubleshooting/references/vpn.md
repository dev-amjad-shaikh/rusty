# VPN — known causes ranked
Ask: which client (Cisco Secure Client/AnyConnect, GlobalProtect, Zscaler ZPA, Tailscale, WireGuard, FortiClient, OpenVPN), and the exact status text.
1. **Wrong password / MFA not completed**: the client says "Login failed" or waits for a push. Check the identity side (`identity-and-sign-in-troubleshooting`).
2. **Connected but nothing works**: split-tunnel DNS. The tunnel is up but names resolve on the local network. Mac: `scutil --dns`; Windows: `Get-DnsClientNrptPolicy`; Linux: `resolvectl status`. Fix: reconnect; the client's DNS settings; on Linux set the tunnel's DNS domains.
3. **Connects then drops every N minutes**: rekey/session lifetime, or Wi-Fi roaming underneath (see wifi.md). Note the interval; a fixed interval is a policy, a random one is the link.
4. **Blocked network**: hotel/airport blocks UDP 443/500/4500 or IKE. Try the client's TCP fallback or a phone hotspot to prove it.
5. **Posture/compliance check failed**: the client says the device is non-compliant (EDR not running, disk not encrypted, OS old). Read the management record; fix compliance; `sync-device`.
6. **Certificate expired**: device certificate for the VPN profile; management renews it.
7. **Client version too old** after a gateway upgrade: update the client from the approved source.
8. **Local network conflict**: home router on 10.0.0.0/24 or 192.168.1.0/24 overlapping the corporate range; the client cannot route. Change the home subnet or use the client's "allow local LAN" toggle if policy permits.
9. **Always-on VPN with no captive portal exception**: cannot reach the portal to get online. The client usually has a "captive portal remediation" window; otherwise use a hotspot briefly.
Logs: Cisco `DART` bundle; GlobalProtect → Settings → Troubleshooting → Collect Logs; Zscaler → More → Export Logs. Ask for the last 20 lines around the failure, not the whole bundle.
