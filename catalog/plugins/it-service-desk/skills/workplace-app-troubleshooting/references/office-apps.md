# Microsoft 365 desktop apps (Word, Excel, PowerPoint, OneDrive)
- "Unlicensed product": the account signed into Office is not the licensed one, or licence removed. `microsoft-365.get-user` assignedLicenses. File → Account → sign out, sign in with the work account.
- Activation errors 0x8004FC12, 0x80070005: Office Licensing service; run Office repair (Apps → Microsoft 365 → Modify → Quick Repair, then Online Repair).
- OneDrive not syncing: a file name with invalid characters or a path > 400 chars, a locked file, "files on demand" state; OneDrive icon → Help & Settings → View sync problems names the file. Reset: `%LOCALAPPDATA%\Microsoft\OneDrive\onedrive.exe /reset`.
- Autosave off: the file is not in OneDrive/SharePoint or is .xls/.doc; save as .xlsx/.docx in a synced location.
- Excel slow: volatile formulas, external links, a 200 MB workbook; not the laptop.
- Crashes on open: add-ins (start with /safe); a corrupt normal.dotm (rename it).
- Mac Office: sign-in issues clear with `~/Library/Group Containers/UBF8T346G9.Office` licence removal via the Microsoft License Removal Tool, then sign in again.
