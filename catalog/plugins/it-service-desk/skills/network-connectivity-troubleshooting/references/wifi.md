# Wi-Fi — known causes ranked
1. **Wrong network or captive portal.** Guest networks need a portal; corporate 802.1X needs a valid certificate or credentials. Open http://neverssl.com to surface a portal.
2. **Roaming between access points** drops calls hourly: the laptop sticks to a far AP. Forget and rejoin; on Mac, Wi-Fi Diagnostics (option-click Wi-Fi icon → Wireless Diagnostics) shows RSSI (< -70 dBm is poor) and noise. On Windows, `netsh wlan show interfaces` shows Signal.
3. **Power management** turns the adapter off (Windows: adapter properties → Power Management). Docked laptops with the lid closed and Wi-Fi power saving are a classic.
4. **Band steering / 2.4 GHz congestion.** Prefer 5 GHz/6 GHz; on Windows adapter properties → Preferred Band.
5. **Driver or firmware.** Intel Wi-Fi driver updates fix more drops than anything else; on Mac, the OS update carries it.
6. **802.1X certificate expired**: the device connects and then disconnects, or "authentication failed". Management renews the SCEP/PKCS profile: Intune `get-device-configuration-states`, Jamf profiles.
7. **Random MAC address** (iOS/Android/macOS private address, Windows random hardware address) breaks MAC-allow-listed networks: turn it off for that network only.
8. **DHCP exhaustion** in a busy office: 169.254 address for many people at once → network team.
Commands: Mac `networksetup -listallhardwareports`, `sudo wdutil info`; Windows `netsh wlan show interfaces`, `netsh wlan show wlanreport`; Linux `nmcli device wifi list`, `iw dev wlan0 link`.
