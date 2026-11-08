-- Refresh options on runs, and drift checks on workspaces.
alter table runs
  add column skip_refresh boolean not null default false,
  add column refresh_only boolean not null default false,
  add constraint refresh_only_is_speculative check (not refresh_only or (operation = 'plan_only' and not skip_refresh));
alter table workspaces
  add column drift_interval bigint check (drift_interval is null or drift_interval > 0),
  add column drift_checked_at timestamptz,
  add column drift_run text,
  add column drift_status text check (drift_status is null or drift_status in ('clean','drifted','error')),
  add column drift_changes integer;
