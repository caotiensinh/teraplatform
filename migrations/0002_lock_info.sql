-- The OpenTofu http backend describes its lock as JSON; other clients get it back on conflict.
alter table workspaces add column lock_info text;
