# Supabase runtime role and RLS rollout

## Why this is an operational change

The app's migrations enable RLS for the Supabase/PostgREST surface, but a
database table owner and any role with `BYPASSRLS` bypass those policies. A
backend connected with the Supabase owner credential therefore needs a
separate, non-owner runtime role before RLS can provide defence in depth for
server-side queries.

Do **not** solve this by adding `USING (true)` policies for the application
role. That changes the role name but preserves the same IDOR blast radius.

## Provision the role

Run these commands through the Supabase SQL editor or an owner-only migration
connection after choosing a long random password in the deployment secret
store. Role creation is intentionally not an embedded app migration: it is
cluster-global, is often unavailable to a pooled Supabase connection, and
must not rotate a production password at app boot.

```sql
CREATE ROLE athleto_runtime LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS
  PASSWORD '<store-this-only-in-your-secret-manager>';

GRANT USAGE ON SCHEMA public, auth TO athleto_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO athleto_runtime;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO athleto_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO athleto_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public
  GRANT USAGE, SELECT ON SEQUENCES TO athleto_runtime;
```

Use this role's pooled connection string for `DATABASE_URL`; never give the
application the `postgres` owner password. Confirm the resulting session is
not an owner or bypass role:

```sql
SELECT current_user, rolbypassrls, pg_has_role(current_user, 'postgres', 'member')
FROM pg_roles WHERE rolname = current_user;
```

The expected result is `rolbypassrls = false` and no membership in `postgres`.

## Enforce a request subject

The current SeaORM layer performs trusted server-side work and does not yet
set a verified Supabase subject on each transaction. Before switching customer
tables to strict runtime-role RLS, add a transaction boundary that receives a
`Uuid` only after `auth::fetch_user` has verified the session with GoTrue and
executes:

```sql
SELECT set_config('request.jwt.claim.sub', '<verified-user-uuid>', true);
```

Every customer-scoped policy must then use the scoped setting, never an
untrusted request field:

```sql
USING (user_id = NULLIF(current_setting('request.jwt.claim.sub', true), '')::uuid)
WITH CHECK (user_id = NULLIF(current_setting('request.jwt.claim.sub', true), '')::uuid)
```

Keep privileged operations in narrowly scoped `SECURITY DEFINER` functions or
a distinct ops role, and revoke direct table grants from roles that do not
need them. Test every policy with `SET ROLE athleto_runtime`, a matching
subject, a different subject, and an unset subject. The last two must return
zero rows or be rejected.

## Rollout gate

Do not change `DATABASE_URL` to `athleto_runtime` until the transaction
scoping and policies above cover every customer query. A non-owner role with
missing policies fails closed (correct but unavailable); a permissive policy
would be available but does not improve authorization. Track that code
migration as a release gate and verify it in a staging project before rotating
the production connection string.
