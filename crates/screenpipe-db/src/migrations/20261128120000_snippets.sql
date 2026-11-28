-- screenpipe — AI that knows everything you've seen, said, or heard
-- https://screenpipe.com

-- Code the user copied, kept by the opt-in snippet worker
-- (enableSnippetCapture). Each row comes from one clipboard ui_event; the
-- same text copied again is stored once (content_hash).
CREATE TABLE IF NOT EXISTS snippets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL UNIQUE,
    language TEXT,
    title TEXT,
    description TEXT,
    tags TEXT NOT NULL DEFAULT '[]',
    app_name TEXT,
    window_title TEXT,
    browser_url TEXT,
    document_path TEXT,
    frame_id INTEGER,
    ui_event_id INTEGER,
    enriched_at TEXT,
    -- Text privacy receipt key the copy was released under when saved; NULL
    -- when release was off. While release is on, the API serves a snippet
    -- only if this matches the current key or its event is released now.
    release_key INTEGER
);

CREATE INDEX IF NOT EXISTS idx_snippets_created_at ON snippets(created_at);
CREATE INDEX IF NOT EXISTS idx_snippets_language ON snippets(language);
CREATE INDEX IF NOT EXISTS idx_snippets_enriched_at ON snippets(enriched_at);
CREATE INDEX IF NOT EXISTS idx_snippets_ui_event_id ON snippets(ui_event_id);

-- Saved snippets outlive their clipboard event: retention (and any other
-- ui_events delete) only unlinks them. A user's time-range delete removes
-- them explicitly (delete_time_range); DELETE /snippets/:id removes one.
CREATE TRIGGER IF NOT EXISTS snippets_ui_events_ad AFTER DELETE ON ui_events BEGIN
    UPDATE snippets SET ui_event_id = NULL WHERE ui_event_id = OLD.id;
END;
