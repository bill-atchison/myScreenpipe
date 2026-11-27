---
schedule: 5 * * * *
enabled: true
effort: low
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
3. `GET /activity-summary?start_time=<start>&end_time=<end>`. If it shows under 5 minutes of activity in total, stop and write nothing.
4. If you need more detail, run at most 4 more searches (`/search` or `/search/semantic`, limit 10, within the window).
5. **Find the projects.** A project is a repo, product or ticket the work belongs to. Infer it from window titles, repo and branch names, VS Code / terminal workspace names, MultiTerminal ticket titles, document names, and meeting titles. Use short stable lowercase names (`myscreenpipe`, `dtpos`, `multiterminal`). Put work you cannot place under `other`. Ignore anything under 3 minutes.
6. For each project, write one memory with `POST /memories`:
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

# Rules

- Only report what the data shows. No guesses about intent, no advice.
- Never put passwords, tokens, API keys or other secrets in a memory, even if they appear on screen.
- Write nothing else: no files, no notifications, no messages.
