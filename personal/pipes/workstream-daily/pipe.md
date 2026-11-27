---
schedule: 30 18 * * *
enabled: true
effort: low
preset:
- codex
- claude code
- screenpipe-cloud
timeout: 300
icon: 🧵
title: Workstream (daily)
description: Evening per-project rollup of the day's hourly workstream summaries, saved as memories
template: true
---

# Job

Roll up today's hourly workstream summaries into **one daily summary per project**. Use only the local API at `http://localhost:3030` with `Authorization: Bearer $SCREENPIPE_LOCAL_API_KEY` (read the `screenpipe-api` skill first).

# Steps

1. `date` = today's local date, `YYYY-MM-DD`.
2. **Skip if already done:** `GET /memories?source=workstream&tags=workstream,daily,date:<date>&limit=1`. If anything comes back, stop.
3. `GET /memories?source=workstream&tags=workstream,hourly&start_time=<today 00:00 local>&limit=200&order_by=created_at&order_dir=asc`.
   If there are none, stop and write nothing.
4. Group them by their `project:` tag. For each project, write one memory with `POST /memories`:
   ```json
   {"content": "...", "source": "workstream", "importance": 0.6,
    "tags": ["workstream", "daily", "project:<name>", "date:<date>"]}
   ```
   `content`, under 200 words:
   ```
   [<project>] <date>, ~<total minutes> min across <n> hours
   Done: <main outcomes of the day>
   Decided: <decisions, or "none seen">
   Open: <what is still unfinished or blocked at end of day, or "none seen">
   ```
   Merge repeats; an "Open" item that a later hour shows as done is not open.

# Rules

- Build only from the hourly summaries. If something looks wrong in them, leave it out rather than guess.
- Never put secrets in a memory. Write nothing else: no files, no notifications.
