# Pinned certificates

## `supabase-prod-ca-2021.crt`

The **Supabase Root 2021 CA**. Supabase's connection pooler
(`*.pooler.supabase.com`) presents a certificate chain anchored at this root,
which is a **private Supabase CA** — it is *not* in the Mozilla webpki-roots
bundle that `sqlx` (rustls) trusts by default. `sslmode=verify-full` therefore
requires it to be pinned explicitly, which `db::enforce_db_tls` does for public
hosts.

Provenance (verified 2026-07-20):

- Downloaded from Supabase's published bundle:
  `https://supabase-downloads.s3-ap-southeast-1.amazonaws.com/prod/ssl/prod-ca-2021.crt`
- SHA-256 fingerprint:
  `80:70:25:AD:50:D4:ED:21:9D:2C:9C:7D:29:9C:00:4F:82:4E:B0:0C:F7:F6:5A:FE:F6:07:D0:7B:72:E6:CA:FA`
- Confirmed **byte-identical** to the root the live pooler
  (`aws-0-ca-central-1.pooler.supabase.com:5432`) serves as the top of its
  chain (`openssl s_client -starttls postgres -showcerts`).
- Validity: `Apr 28 2021` → `Apr 26 2031`.

### Rotation

If Supabase rotates this CA, `verify-full` connections will fail with a cert
error. To recover without a code change, an operator can point
`ATHLETO_DB_SSLROOTCERT` at the new CA (or set `ATHLETO_DB_SSLMODE=require` as a
temporary encrypted-but-unverified stopgap). The permanent fix is to replace
this file with the new published CA and re-verify the fingerprint above.

### Verifying this file

```sh
openssl x509 -in supabase-prod-ca-2021.crt -noout -sha256 -fingerprint -subject -dates
```
