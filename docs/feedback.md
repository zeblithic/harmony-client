# Submitting feedback

This page explains what happens when you click **(?) → Submit Feedback** in Harmony alpha, what's included in your report, what isn't, and what to expect after submitting.

## The flow

1. Click the **(?)** icon in the top-right corner of Harmony.
2. Click **Submit Feedback** in the dropdown menu.
3. A modal opens with a description field. Type what happened, what you expected, and what you saw — at least 10 characters.
4. Optional: toggle **"Attach network diagnostics"** to include a redacted snapshot of your Network Health panel.
5. Click **Submit**. Your default browser opens a pre-filled GitHub new-issue page.
6. Review the body, edit if you'd like, and click **Submit new issue** on GitHub.

The Harmony app never sends your feedback anywhere on its own. It only opens a pre-filled GitHub URL in your browser; you submit (or don't) from there.

## What's auto-included

Every feedback submission includes:

- **`## Description`** — exactly what you typed, verbatim.
- **`## Environment`** — four lines:
  - App version (e.g., `0.1.2`)
  - Platform (`macos` for macOS, `windows` for Windows, `linux` for Linux)
  - OS version (e.g., `15.0`)
  - Timestamp (ISO-8601 UTC of when you submitted)

If you toggled **"Attach network diagnostics"** ON:

- **`## Network diagnostics`** — the same redacted markdown produced by the **Export diagnostics** button in your Network Health panel. Identifiers are server-side redacted (no full Ed25519 hex). You can preview the exact text in the modal before submitting.

If the diagnostic toggle is OFF, the `## Network diagnostics` section is omitted entirely from the report.

## What's NOT included

- **No automatic telemetry.** Harmony never sends usage data, logs, or crash reports anywhere by itself. Feedback is opt-in, manual, and routed through your browser.
- **No identity material.** Your Ed25519 secret keys, pkarr secrets, and ALPN tokens never flow through the feedback path. Diagnostic snapshots use the same redactor as the in-app Export diagnostics button.
- **No content.** Messages you've sent, files you've stored, communities you're in — none of this is included unless you paste it into the description yourself.
- **No persistent draft.** If you dismiss the modal, your typed description is discarded. Submit when you're ready.

## URL-length budget

GitHub URLs have a practical limit around 8000 characters. If you attach a large diagnostic snapshot, the snapshot section may be truncated with a `…[truncated for URL length]` marker. Your description, environment info, and the beginning of the diagnostics are always preserved intact.

If you need the full diagnostic, use the **Export diagnostics** button in the Network Health panel and attach the resulting `.txt` file to the GitHub issue manually after submitting.

## What if my browser doesn't open?

If the Tauri shell plugin can't launch your default browser (e.g., on a Linux desktop without `xdg-open`), the app falls back to copying the GitHub URL to your clipboard with a toast notification. Paste it into your browser of choice manually.

## Where reports go

All feedback flows to the public [`zeblithic/harmony-client` GitHub issue tracker](https://github.com/zeblithic/harmony-client/issues). Other alpha testers can see and comment on your issues, which helps build a shared knowledge base.

Issues are reviewed by the development team on a rolling basis. There's no formal SLA during alpha — bugs blocking the validation flow get priority. Feel free to comment, edit, or close issues yourself.

## Privacy expectations

- You review the GitHub URL body **before** clicking Submit on GitHub. Edit out anything you'd rather not share.
- Diagnostic snapshots are redacted by Harmony before you see them — you can verify by inspecting the modal preview before clicking Submit.
- GitHub itself is a public forum. Don't include anything sensitive in your description if you wouldn't put it in a public forum.

## When in doubt

Submit a feedback report anyway. We'd rather know.
