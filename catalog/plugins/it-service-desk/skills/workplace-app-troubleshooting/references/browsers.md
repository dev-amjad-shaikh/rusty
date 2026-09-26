# Browsers — Chrome, Edge, Safari, Firefox
- Works in a private/incognito window → an extension or cached data. Disable extensions; clear site data for that site only, not everything.
- Works in another browser → that browser's proxy or secure DNS setting, or an enterprise policy (chrome://policy, edge://policy) blocking the site or feature.
- Certificate error everywhere → clock; on one site behind a gateway → the gateway root is not in that browser's trust (Firefox keeps its own store: about:config `security.enterprise_roots.enabled` true).
- Managed browsers reset settings (homepage, extensions) on purpose: say it is policy.
- Sign-in to a Google or Microsoft account in the browser is separate from the OS account; a profile signed into a personal account explains "wrong Drive/OneDrive".
- Memory/slow: dozens of tabs, a heavy extension; Chrome → More tools → Task manager shows which.
- Downloads blocked → gateway or policy; the block page says which.
