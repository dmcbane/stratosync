-- Migration 0006: Dedupe file_index rows that share (mount_id, parent_inode, name).
--
-- Background: Microsoft Graph returns parentReference.path in two
-- equivalent forms — `/drive/root:/...` and `/drives/{drive_id}/root:/...`.
-- The OneDrive delta provider only stripped the first; items from the
-- second leaked into the DB with paths like
-- `drives/4139BC84D0721EB5/root:/test`. The rclone poller meanwhile
-- reported the same item with its clean relative path (`test`), so the
-- (mount_id, remote_path) UNIQUE didn't catch the collision and the DB
-- ended up with two rows per affected entry. Symptom: each such folder
-- showed up twice in Dolphin / `ls`. (Pre-v0.12.1 the kernel readdir
-- cache happened to hide the dupes; v0.12.1's post-mutation
-- inval_inode exposed them.)
--
-- The path resolver is fixed in v0.12.1; this migration cleans up rows
-- already written by the broken resolver. We keep the lowest-inode row
-- in each (mount_id, parent_inode, name) group — that one matches what
-- `get_by_parent_name` (and therefore lookup) was already returning, so
-- existing references stay valid.

-- Step 1: reparent any children of soon-to-be-deleted duplicate rows
-- onto the keeper. Phantoms typically don't have children — the
-- broken-path duplicate was the leaf-most one — but we don't want to
-- gamble against ON DELETE blocking the cleanup.
UPDATE file_index
SET parent_inode = (
    SELECT MIN(keeper.inode)
    FROM file_index keeper, file_index dupe
    WHERE dupe.inode = file_index.parent_inode
      AND keeper.mount_id     = dupe.mount_id
      AND keeper.parent_inode = dupe.parent_inode
      AND keeper.name         = dupe.name
)
WHERE parent_inode IN (
    SELECT f.inode FROM file_index f
    WHERE f.parent_inode IS NOT NULL
      AND f.inode > (
          SELECT MIN(g.inode) FROM file_index g
          WHERE g.mount_id     = f.mount_id
            AND g.parent_inode = f.parent_inode
            AND g.name         = f.name
      )
);

-- Step 2: delete the duplicates themselves (keep MIN inode per group).
DELETE FROM file_index
WHERE parent_inode IS NOT NULL
  AND inode > (
      SELECT MIN(g.inode) FROM file_index g
      WHERE g.mount_id     = file_index.mount_id
        AND g.parent_inode = file_index.parent_inode
        AND g.name         = file_index.name
  );

-- Step 3: prevent future (parent_inode, name) duplicates at the schema
-- level. SQLite doesn't allow ALTER TABLE ADD CONSTRAINT, so we use a
-- partial unique index. The FUSE root inode (parent_inode IS NULL) is
-- excluded — it's a single row per mount and the predicate never
-- matches a NULL.
CREATE UNIQUE INDEX idx_file_index_parent_name_unique
    ON file_index(mount_id, parent_inode, name)
    WHERE parent_inode IS NOT NULL;
