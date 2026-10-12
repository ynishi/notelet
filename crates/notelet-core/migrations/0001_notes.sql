-- notelet: two tables, nothing else.
--
-- `notes` is the live store: one row per path, the whole Markdown body, and a
-- version that `note_put` compares against (optimistic locking). `revisions`
-- is an append-only copy of every body that `note_put` replaced, so a bad
-- write can be read back. Appends do not create revisions: an append only
-- adds text, and the previous content is a prefix of the new one.

CREATE TABLE notes (
    path        text        PRIMARY KEY,
    body        text        NOT NULL,
    version     bigint      NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text        NOT NULL
);

-- prefix listing (`path LIKE 'projects/x/%'`) and keyset paging by path
CREATE INDEX notes_path_prefix ON notes (path text_pattern_ops);
CREATE INDEX notes_updated_at  ON notes (updated_at);

CREATE TABLE revisions (
    path        text        NOT NULL,
    version     bigint      NOT NULL,
    body        text        NOT NULL,
    updated_at  timestamptz NOT NULL,
    updated_by  text        NOT NULL,
    PRIMARY KEY (path, version)
);
