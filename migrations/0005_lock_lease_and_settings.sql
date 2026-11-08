-- Lock leases (a lock can lapse unless its holder keeps renewing it) and workspace settings.
alter table workspaces
  add column lock_expires_at timestamptz,
  add column description text not null default '' check (char_length(description) <= 500),
  add column auto_apply boolean not null default false;
