// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com
import { expect, it, vi } from "vitest";
import { WORKSTREAM_SUMMARIES_TOOL, workstreamSummaries } from "./workstream-summaries";

it("reads workstream memories for a range and project", async () => {
  const api = vi.fn().mockResolvedValue(Response.json({ data: [
    { content: "Did: built zPOS_Prism", tags: ["workstream", "hourly", "project:dtpos", "window:2026-11-27T14:00"] },
  ] }));
  const r = await workstreamSummaries({ start_time: "2026-11-27T00:00:00Z", end_time: "2026-11-28T00:00:00Z", project: "DTPOS" }, api);
  const p = new URL(`http://h${api.mock.calls[0][0]}`);
  expect(p.pathname).toBe("/memories");
  expect(p.searchParams.get("source")).toBe("workstream");
  expect(p.searchParams.get("tags")).toBe("workstream,hourly,project:dtpos");
  expect(p.searchParams.get("start_time")).toBeTruthy();
  expect(r.content[0].text).toContain("[project:dtpos window:2026-11-27T14:00]");
  expect(r.content[0].text).toContain("built zPOS_Prism");
  expect(WORKSTREAM_SUMMARIES_TOOL.annotations?.readOnlyHint).toBe(true);
});

it("uses daily scope and accepts a bare array response", async () => {
  const api = vi.fn().mockResolvedValue(Response.json([]));
  const r = await workstreamSummaries({ start_time: "2026-11-27T00:00:00Z", scope: "daily" }, api);
  expect(new URL(`http://h${api.mock.calls[0][0]}`).searchParams.get("tags")).toBe("workstream,daily");
  expect(r.content[0].text).toContain("No workstream summaries");
});

it("rejects bad input before calling the API", async () => {
  const api = vi.fn();
  await expect(workstreamSummaries({ start_time: "today", project: "a,b" }, api)).rejects.toThrow("project");
  await expect(workstreamSummaries({ start_time: "today", scope: "weekly" }, api)).rejects.toThrow("scope");
  await expect(workstreamSummaries({ end_time: "today" }, api)).rejects.toThrow("start_time");
  await expect(workstreamSummaries({ start_time: "today", extra: 1 }, api)).rejects.toThrow("Unknown");
  expect(api).not.toHaveBeenCalled();
});
