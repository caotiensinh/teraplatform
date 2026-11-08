-- Phase 1 schema for the Rust IaC run platform (working name TBD).
-- Postgres 15+. Times are timestamptz (UTC). Not yet executed against a database.
-- Access rule: one authorisation check per query in the data layer (no RLS),
-- always scoped by org_id. Forbidden and missing both return 404.

create extension if not exists pgcrypto;

create table users (
  id uuid primary key default gen_random_uuid(),
  email text not null unique,
  password_hash text,                      -- argon2id; null for SSO-only users
  display_name text not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table orgs (
  id uuid primary key default gen_random_uuid(),
  name text not null unique check (name ~ '^[A-Za-z0-9_-]{2,64}$'),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table org_members (
  org_id uuid not null references orgs(id) on delete cascade,
  user_id uuid not null references users(id) on delete cascade,
  role text not null default 'member' check (role in ('owner','member')),
  created_at timestamptz not null default now(),
  primary key (org_id, user_id)
);
create index on org_members (user_id);

create table teams (
  id uuid primary key default gen_random_uuid(),
  org_id uuid not null references orgs(id) on delete cascade,
  name text not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (org_id, name)
);

create table team_members (
  team_id uuid not null references teams(id) on delete cascade,
  user_id uuid not null references users(id) on delete cascade,
  primary key (team_id, user_id)
);
create index on team_members (user_id);

create table projects (
  id uuid primary key default gen_random_uuid(),
  org_id uuid not null references orgs(id) on delete cascade,
  name text not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (org_id, name)
);

-- Team access to a project or a single workspace.
create table access_grants (
  id uuid primary key default gen_random_uuid(),
  team_id uuid not null references teams(id) on delete cascade,
  project_id uuid references projects(id) on delete cascade,
  workspace_id uuid,                       -- fk added after workspaces
  permission text not null check (permission in ('read','plan','write','admin')),
  created_at timestamptz not null default now(),
  check ((project_id is null) <> (workspace_id is null))
);
create index on access_grants (team_id);

create table agent_pools (
  id uuid primary key default gen_random_uuid(),
  org_id uuid not null references orgs(id) on delete cascade,
  name text not null,
  created_at timestamptz not null default now(),
  unique (org_id, name)
);

create table workspaces (
  id uuid primary key default gen_random_uuid(),
  org_id uuid not null references orgs(id) on delete cascade,
  project_id uuid not null references projects(id) on delete restrict,
  name text not null check (name ~ '^[A-Za-z0-9_-]{1,90}$'),
  description text not null default '',
  execution_mode text not null default 'remote' check (execution_mode in ('remote','local','agent')),
  agent_pool_id uuid references agent_pools(id) on delete set null,
  engine text not null default 'tofu' check (engine in ('tofu')),   -- phase 1: the tofu binary
  engine_version text not null default 'latest',                    -- exact or constraint
  working_directory text not null default '',
  auto_apply boolean not null default false,
  allow_destroy_plan boolean not null default true,
  speculative_enabled boolean not null default true,
  queue_all_runs boolean not null default true,
  global_remote_state boolean not null default false,
  vcs_repo jsonb,                          -- {identifier, branch, ingress_submodules, tags_regex}
  trigger_prefixes text[] not null default '{}',
  trigger_patterns text[] not null default '{}',
  tags text[] not null default '{}',
  -- lock: owner + lease. Lease expiry lets a crashed run release itself (gap G12).
  locked_by_user uuid references users(id) on delete set null,
  locked_by_run uuid,                      -- fk added after runs
  lock_reason text,
  lock_id text,                            -- opaque id used by the CLI backend protocol
  lock_info jsonb,
  lock_lease_until timestamptz,
  current_state_version_id uuid,           -- fk added after state_versions
  current_run_id uuid,                     -- fk added after runs
  resource_count integer not null default 0,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (org_id, name),
  check (execution_mode <> 'agent' or agent_pool_id is not null)
);
create index on workspaces (project_id);
create index on workspaces (org_id, updated_at desc);
alter table access_grants add constraint access_grants_workspace_fk
  foreign key (workspace_id) references workspaces(id) on delete cascade;

create table tokens (
  id uuid primary key default gen_random_uuid(),
  kind text not null check (kind in ('user','team','org','audit')),
  user_id uuid references users(id) on delete cascade,
  team_id uuid references teams(id) on delete cascade,
  org_id uuid references orgs(id) on delete cascade,
  description text not null default '',
  token_hash bytea not null unique,        -- sha256 of the secret; secret shown once
  expires_at timestamptz,
  last_used_at timestamptz,
  created_at timestamptz not null default now(),
  check (
    (kind = 'user' and user_id is not null) or
    (kind = 'team' and team_id is not null) or
    (kind in ('org','audit') and org_id is not null)
  )
);

create table configuration_versions (
  id uuid primary key default gen_random_uuid(),
  workspace_id uuid not null references workspaces(id) on delete cascade,
  source text not null check (source in ('api','ui','vcs','cli')),
  status text not null default 'pending' check (status in ('pending','uploaded','errored')),
  speculative boolean not null default false,
  auto_queue_runs boolean not null default true,
  blob_key text,                           -- object store key of the archive
  vcs_commit_sha text,
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now()
);
create index on configuration_versions (workspace_id, created_at desc);

create table runs (
  id uuid primary key default gen_random_uuid(),
  workspace_id uuid not null references workspaces(id) on delete cascade,
  org_id uuid not null references orgs(id) on delete cascade,
  configuration_version_id uuid not null references configuration_versions(id) on delete restrict,
  input_state_version_id uuid,             -- fk added after state_versions
  status text not null default 'pending' check (status in (
    'pending','fetching','pre_plan_running','planning','planned','needs_confirmation',
    'policy_checking','policy_override','policy_soft_failed','policy_checked',
    'cost_estimating','cost_estimated','post_plan_running','confirmed',
    'apply_queued','pre_apply_running','applying','post_apply_running',
    'applied','planned_and_finished','errored','discarded','canceled','force_canceled')),
  operation text not null default 'plan_and_apply' check (operation in (
    'plan_only','plan_and_apply','save_plan','refresh_only','destroy','empty_apply')),
  source text not null check (source in ('ui','api','vcs','cli')),
  is_destroy boolean not null default false,
  refresh boolean not null default true,
  auto_apply boolean not null default false,
  allow_empty_apply boolean not null default false,
  target_addrs text[] not null default '{}',
  replace_addrs text[] not null default '{}',
  variables jsonb not null default '{}',   -- run-level overrides; sensitive never stored here
  message text not null default '',
  debugging_mode boolean not null default false,
  has_changes boolean,
  plan_summary jsonb,                      -- {add, change, destroy}
  plan_blob_key text,                      -- saved plan file (encrypted)
  status_timestamps jsonb not null default '{}',
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);
create index on runs (workspace_id, created_at desc);
create index on runs (org_id, created_at desc);
create index on runs (status) where status in ('pending','planning','applying','apply_queued','confirmed');

-- The queue rule: at most ONE run per workspace is executing at a time.
create unique index one_active_run_per_workspace on runs (workspace_id)
  where status in ('fetching','pre_plan_running','planning','policy_checking','cost_estimating',
                   'post_plan_running','apply_queued','pre_apply_running','applying','post_apply_running')
    and operation <> 'plan_only';

alter table workspaces add constraint workspaces_current_run_fk
  foreign key (current_run_id) references runs(id) on delete set null;
alter table workspaces add constraint workspaces_locked_by_run_fk
  foreign key (locked_by_run) references runs(id) on delete set null;

create table run_events (
  id bigserial primary key,
  run_id uuid not null references runs(id) on delete cascade,
  kind text not null,                      -- status_change, comment, apply_requested, override, cancel...
  actor_user_id uuid references users(id) on delete set null,
  from_status text,
  to_status text,
  body text,
  created_at timestamptz not null default now()
);
create index on run_events (run_id, id);

-- Streamed plan/apply output, stored as ordered chunks for replay and SSE resume.
create table run_logs (
  run_id uuid not null references runs(id) on delete cascade,
  phase text not null check (phase in ('plan','apply','policy','task')),
  seq integer not null,
  chunk text not null,
  created_at timestamptz not null default now(),
  primary key (run_id, phase, seq)
);

create table state_versions (
  id uuid primary key default gen_random_uuid(),
  workspace_id uuid not null references workspaces(id) on delete cascade,
  run_id uuid references runs(id) on delete set null,
  serial bigint not null,
  lineage text,
  format_version integer,
  status text not null default 'pending' check (status in ('pending','finalized','discarded')),
  intermediate boolean not null default false,
  md5 text not null,
  blob_key text,                           -- encrypted state blob
  engine_version text,
  resources_processed boolean not null default false,
  resources jsonb,
  outputs jsonb,
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  unique (workspace_id, serial)
);
create index on state_versions (workspace_id, created_at desc);

alter table workspaces add constraint workspaces_current_state_fk
  foreign key (current_state_version_id) references state_versions(id) on delete set null;
alter table runs add constraint runs_input_state_fk
  foreign key (input_state_version_id) references state_versions(id) on delete set null;

create table variables (
  id uuid primary key default gen_random_uuid(),
  workspace_id uuid not null references workspaces(id) on delete cascade,
  key text not null,
  category text not null check (category in ('terraform','env')),
  hcl boolean not null default false,
  sensitive boolean not null default false,
  value_plain text,                        -- null when sensitive
  value_enc bytea,                         -- AES-256-GCM with app key when sensitive; write once
  description text not null default '',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (workspace_id, category, key),
  check ((sensitive and value_enc is not null and value_plain is null)
      or (not sensitive and value_enc is null))
);

create table policy_sets (
  id uuid primary key default gen_random_uuid(),
  org_id uuid not null references orgs(id) on delete cascade,
  name text not null,
  scope text not null default 'workspaces' check (scope in ('global','projects','workspaces')),
  enforcement text not null default 'advisory' check (enforcement in ('advisory','soft_mandatory','hard_mandatory')),
  source text not null default 'inline' check (source in ('inline','vcs')),
  body text,                               -- own rule language, see architecture.md
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (org_id, name)
);

create table policy_set_targets (
  policy_set_id uuid not null references policy_sets(id) on delete cascade,
  project_id uuid references projects(id) on delete cascade,
  workspace_id uuid references workspaces(id) on delete cascade,
  check ((project_id is null) <> (workspace_id is null))
);
create index on policy_set_targets (policy_set_id);

create table policy_results (
  id uuid primary key default gen_random_uuid(),
  run_id uuid not null references runs(id) on delete cascade,
  policy_set_id uuid not null references policy_sets(id) on delete cascade,
  outcome text not null check (outcome in ('passed','advisory_failed','soft_failed','hard_failed','overridden')),
  detail jsonb not null default '{}',
  overridden_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now()
);
create index on policy_results (run_id);

create table notifications (
  id uuid primary key default gen_random_uuid(),
  workspace_id uuid not null references workspaces(id) on delete cascade,
  destination text not null check (destination in ('webhook','email','slack')),
  url text,
  email text,
  triggers text[] not null default '{}',
  secret_enc bytea,
  enabled boolean not null default true,
  created_at timestamptz not null default now()
);
create index on notifications (workspace_id);

create table audit_log (
  id bigserial primary key,
  org_id uuid not null references orgs(id) on delete cascade,
  actor_user_id uuid references users(id) on delete set null,
  token_id uuid references tokens(id) on delete set null,
  action text not null,                    -- e.g. workspace.lock, run.apply, state.rollback
  target_type text not null,
  target_id uuid,
  detail jsonb not null default '{}',
  created_at timestamptz not null default now()
);
create index on audit_log (org_id, id desc);

-- Postgres-backed job queue for the runner and notifications (SELECT ... FOR UPDATE SKIP LOCKED).
create table jobs (
  id uuid primary key default gen_random_uuid(),
  kind text not null,                      -- run_step, notify, drift_check, state_cleanup
  payload jsonb not null,
  run_after timestamptz not null default now(),
  attempts integer not null default 0,
  max_attempts integer not null default 5,
  locked_by text,
  locked_until timestamptz,
  done_at timestamptz,
  last_error text,
  created_at timestamptz not null default now()
);
create index on jobs (run_after) where done_at is null;
