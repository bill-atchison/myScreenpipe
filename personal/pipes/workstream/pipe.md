---
schedule: 5 * * * *
enabled: true
effort: medium
preset:
- codex
- claude code
- screenpipe-cloud
timeout: 300
icon: 🧵
title: Workstream (hourly)
description: Hourly per-project summary of the previous clock hour, saved as memories tagged workstream
template: true
---

# Job

Summarise what William worked on during the **previous full clock hour** (for a run at 14:05, the window is 13:00 to 14:00 local time), **grouped by project**, and save one memory per project. Agents and the daily rollup read these later, so be factual and specific.

Use only screenpipe's recorded data via the local API at `http://localhost:3030`, sending `Authorization: Bearer $SCREENPIPE_LOCAL_API_KEY`. Read the `screenpipe-api` skill first. Do not read this machine's files or other apps' data.

# Steps

1. Work out the window: `start` = start of the previous clock hour, `end` = start of this hour, both as local ISO 8601 with offset. `window` = the start formatted `YYYY-MM-DDTHH` (local).
2. **Skip if already done:** `GET /memories?source=workstream&tags=workstream,window:<window>&limit=1`. If anything comes back, stop: this hour was already summarised.
3. `GET /meetings?start_time=<start>&end_time=<end>`. **Meetings are never skipped.** For each meeting that overlaps the window (Teams or any other app), read `GET /meetings/<id>/transcript` and keep its part inside the window.
4. `GET /activity-summary?start_time=<start>&end_time=<end>`. If there was no meeting and it shows under 5 minutes of activity in total, stop and write nothing.
5. If you need more detail, run at most 4 more searches (`/search` or `/search/semantic`, limit 10, within the window).
6. **Find the projects.** A project is a repo, product or ticket the work belongs to. Infer it from window titles, repo and branch names, VS Code / terminal workspace names, MultiTerminal ticket titles, document names, and meeting titles and what was discussed. Use short stable lowercase names (`myscreenpipe`, `dtpos`, `multiterminal`). Ignore anything under 3 minutes.
   - **Skip work that belongs to no project** (browsing, reading news or email): write nothing for it.
   - **Meetings:** put a meeting under the project it was about. If it was about no single project, use the project `meetings`. Never drop a meeting.
7. For each project, write one memory with `POST /memories`:
   ```json
   {"content": "...", "source": "workstream", "importance": 0.4,
    "tags": ["workstream", "hourly", "project:<name>", "window:<window>"]}
   ```
   `content` uses this shape, under 120 words:
   ```
   [<project>] <HH:00>–<HH:00>, ~<minutes> min
   Did: <what was worked on, naming files, tickets, apps>
   Decided: <decisions made, or "none seen">
   Open: <what was left unfinished or blocked, or "none seen">
   ```
   **When the hour included a meeting, the memory MUST have a meeting block** (one per meeting,
   before `Did:`). Read the whole transcript for the window before writing it. With a meeting the
   content may run to 250 words:
   ```
   Meeting: <title>, <HH:MM>–<HH:MM> (<app>)
   Discussed: <the main topics, one short clause each>
   Decisions: <each decision, or "none">
   Actions:
   - <owner>: <action> (<due, if said>)
   ```
   - Speaker labels ("speaker 1", "?") are anonymous and can be wrong. Name an owner only when the same exchange shows who it is: someone is addressed by name just before they commit ("Hey, Don… Yes, sir, will do" → Don), or they name themselves. William is called "Bill" by others; give William an action only when he is addressed or clearly speaking as the one who will do it ("I'll get something set up for today" in his exchange with Don). Otherwise use the speaker's role as it shows in the meeting ("offshore lead", "tester", "support"), and "owner unclear" only when even that is unknown.
   - Include every follow-up someone commits to out loud ("I'll follow up with him", "I'll send you the reports", "email us if you need a replacement"), not only formal decisions. Walk the transcript to the very end: commitments often come in the last minutes after the main meeting ends.
   - When several defects or tickets are discussed, keep each outcome attached to the right one; if you cannot tell which outcome belongs to which, say so instead of guessing.
   - Name any blocker raised in the meeting under `Open:`, with who is waiting on whom.
   - Still fill `Did:` with the non-meeting work of the hour.

# Rules

- Only report what the data shows. No guesses about intent, no advice.
- Transcripts mis-hear names. Correct a mis-hearing only when the meeting itself shows the right word (for example "Nordstar" or "all star" when the meeting is about Northstar); never copy a garbled phrase into a summary. If unsure, leave it out.
- Never put passwords, tokens, API keys or other secrets in a memory, even if they appear on screen.
- Write nothing else: no files, no notifications, no messages.
