-- Implemented subset of replica/schema.sql. Ids are text (uuid strings) so the
-- application Id type maps 1:1. All times are timestamptz (UTC).
-- Access rule: handlers authorise every request (tf-core::can) and scope by org;
-- the database enforces integrity (unique, check, foreign keys, one executing run).

create table users (
  id text primary key default gen_random_uuid()::text,
  email text not null,
  password_hash text not null,
  display_name text not null,
  email_verified boolean not null default false,
  created_at timestamptz not null default now()
);
create unique index users_email_lower on users (lower(email));

create table orgs (
  id text primary key default gen_random_uuid()::text,
  name text not null unique check (name ~ '^[A-Za-z0-9_-]{1,90}$'),
  created_at timestamptz not null default now()
);

create table org_members (
  org_id text not null references orgs(id) on delete cascade,
  user_id text not null references users(id) on delete cascade,
  role text not null check (role in ('owner','admin','member')),
  created_at timestamptz not null default now(),
  primary key (org_id, user_id)
);
create index org_members_user on org_members (user_id);

create table sessions (
  token_hash bytea primary key,
  user_id text not null references users(id) on delete cascade,
  expires_at timestamptz not null,
  created_at timestamptz not null default now()
);
create index sessions_user on sessions (user_id);

create table api_tokens (
  id text primary key default gen_random_uuid()::text,
  kind text not null check (kind in ('user','org')),
  user_id text references users(id) on delete cascade,
  org_id text not null references orgs(id) on delete cascade,
  description text not null default '',
  token_hash bytea not null unique,
  expires_at timestamptz,
  created_at timestamptz not null default now(),
  check (kind = 'org' or user_id is not null)
);
create index api_tokens_org on api_tokens (org_id);

create table email_tokens (
  token_hash bytea primary key,
  user_id text not null references users(id) on delete cascade,
  purpose text not null check (purpose in ('verify','reset')),
  expires_at timestamptz not null,
  used_at timestamptz
);

create table workspaces (
  id text primary key default gen_random_uuid()::text,
  org_id text not null references orgs(id) on delete cascade,
  name text not null check (name ~ '^[A-Za-z0-9_-]{1,90}$'),
  lock_owner text,                -- 'user' or 'run:<id>'
  lock_reason text,
  lock_since timestamptz,
  current_run text,
  current_state_version text,
  resource_count integer not null default 0,
  created_at timestamptz not null default now(),
  unique (org_id, name)
);

create table configuration_versions (
  id text primary key default gen_random_uuid()::text,
  workspace_id text not null references workspaces(id) on delete cascade,
  source text not null,
  text text not null,
  created_at timestamptz not null default now()
);
create index config_versions_ws on configuration_versions (workspace_id, created_at desc);

create table runs (
  id text primary key default gen_random_uuid()::text,
  seq bigserial not null unique,   -- queue order
  workspace_id text not null references workspaces(id) on delete cascade,
  config_version_id text not null references configuration_versions(id) on delete cascade,
  status text not null check (status in (
    'pending','planning','needs_confirmation','apply_queued','applying',
    'applied','planned_and_finished','errored','discarded','canceled')),
  operation text not null check (operation in ('plan_and_apply','plan_only')),
  source text not null,
  message text not null default '',
  auto_apply boolean not null default false,
  summary jsonb,
  changes jsonb not null default '[]',
  input_state_serial bigint,
  status_timestamps jsonb not null default '{}',
  plan_blob text,                  -- sealed (enc:v1:...)
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);
create index runs_ws on runs (workspace_id, seq desc);
create index runs_queue on runs (status, seq) where status in ('pending','apply_queued');

-- The queue rule, enforced by the database: at most one lock-holding run per workspace.
create unique index one_executing_run_per_workspace on runs (workspace_id)
  where status in ('planning','needs_confirmation','apply_queued','applying')
    and operation <> 'plan_only';

create table run_logs (
  id bigserial primary key,
  run_id text not null references runs(id) on delete cascade,
  line text not null,
  created_at timestamptz not null default now()
);
create index run_logs_run on run_logs (run_id, id);

create table state_versions (
  id text primary key default gen_random_uuid()::text,
  workspace_id text not null references workspaces(id) on delete cascade,
  run_id text,
  serial bigint not null,
  status text not null default 'finalized',
  content text not null,           -- sealed (enc:v1:...)
  resource_count integer not null default 0,
  created_at timestamptz not null default now(),
  unique (workspace_id, serial)
);

create table variables (
  id text primary key default gen_random_uuid()::text,
  workspace_id text not null references workspaces(id) on delete cascade,
  key text not null,
  category text not null check (category in ('terraform','env')),
  hcl boolean not null default false,
  sensitive boolean not null default false,
  value text not null,             -- sealed when sensitive
  description text not null default '',
  created_at timestamptz not null default now(),
  unique (workspace_id, category, key),
  check (not sensitive or value like 'enc:v1:%')
);
