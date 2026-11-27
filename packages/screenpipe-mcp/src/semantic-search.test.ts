// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com
import { expect, it, vi } from "vitest";
import { SEMANTIC_SEARCH_TOOL, semanticSearch } from "./semantic-search";

const item = (o: Record<string, unknown>) => ({ timestamp: "2026-11-26T22:00:00-05:00", score: 0.03, matched_by: "vector", ...o });

it("calls /search/semantic with bounded params and formats frame and audio hits", async () => {
  const api = vi.fn().mockResolvedValue(Response.json({ data: [
    item({ source_type: "frame", id: 7, app_name: "Code", window_name: "plan.md", text: "retire the old reporting tool", matched_by: "both" }),
    item({ source_type: "audio", id: 3, device: "MacBook Mic", speaker: "Bill", text: "x".repeat(400) }),
  ] }));
  const r = await semanticSearch({ q: "drop legacy reports", content_type: "frame", limit: 5 }, api);
  const url = new URL(`http://h${api.mock.calls[0][0]}`);
  expect(url.pathname).toBe("/search/semantic");
  expect(Object.fromEntries(url.searchParams)).toEqual({ q: "drop legacy reports", content_type: "frame", limit: "5" });
  const text = r.content[0].text;
  expect(text).toContain("[frame:7] Code | plan.md");
  expect(text).toContain("both");
  expect(text).toContain("[audio:3] MacBook Mic (Bill)");
  expect(text).toContain(`${"x".repeat(300)}…`);
  expect(text).not.toContain("x".repeat(301));
  expect(SEMANTIC_SEARCH_TOOL.annotations?.readOnlyHint).toBe(true);
});

it("passes normalized time bounds", async () => {
  const api = vi.fn().mockResolvedValue(Response.json({ data: [] }));
  await semanticSearch({ q: "x", start_time: "2026-11-01T00:00:00Z", end_time: "2026-11-02T00:00:00Z" }, api);
  const p = new URL(`http://h${api.mock.calls[0][0]}`).searchParams;
  expect(p.get("start_time")).toBeTruthy();
  expect(p.get("end_time")).toBeTruthy();
});

it("explains how to turn it on when the engine returns 503", async () => {
  const api = vi.fn().mockRejectedValue(Object.assign(new Error("HTTP 503"), { status: 503 }));
  const r = await semanticSearch({ q: "x" }, api);
  expect(r.content[0].text).toContain("Settings > Screen > System");
});

it.each([401, 404, 500])("does not turn HTTP %s into an empty result", async (status) => {
  const api = vi.fn().mockRejectedValue(Object.assign(new Error(`HTTP ${status}`), { status }));
  await expect(semanticSearch({ q: "x" }, api)).rejects.toThrow(`HTTP ${status}`);
});

it("rejects a response without a data array", async () => {
  await expect(semanticSearch({ q: "x" }, async () => Response.json({}))).rejects.toThrow("coverage is unknown");
});

it.each([{}, { q: "" }, { q: "  " }, { q: "x".repeat(1001) }, { q: "x", limit: 0 }, { q: "x", limit: 101 }, { q: "x", content_type: "video" }, { q: "x", app_name: "Code" }])(
  "rejects invalid input before any request: %j",
  async (args) => {
    const api = vi.fn();
    await expect(semanticSearch(args, api)).rejects.toThrow();
    expect(api).not.toHaveBeenCalled();
  },
);
