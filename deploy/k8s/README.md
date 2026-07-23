# deploy/k8s — prebuilt-image deployment

Canonical family pattern (quaestor `deploy/k8s`, daedalus `deploy/k8s`):
a prebuilt distroless image + in-repo manifests + an ArgoCD `Application`,
replacing the legacy compile-on-boot manifest that currently lives in
`k8s-cluster/remote/argocd/dd-next-runtime/`.

- **Image:** build `../Dockerfile` (multi-stage, non-root `10001`) →
  `ghcr.io/athlet-o/athleto-app-rs:main` in CI (add `.github/workflows/image.yml`).
- **Secrets:** `athleto-app-rs-secrets` via an ESO `ExternalSecret`
  (`ClusterSecretStore/dd-cluster-secrets` → AWS Secrets Manager). The rendered
  secret must include `SUPABASE_URL`, `SUPABASE_ANON_KEY`, and
  `SHARED_AUTH_BASE_URL` for login; auth stays fail-closed when that stack is
  incomplete.
- **Register:** apply `argocd-application.yaml` once; ArgoCD syncs this dir on
  `main`. The `athleto` AppProject must exist (tenant boundary).

This is a foundation: wire the CI image build + ESO ExternalSecret + the
`athleto` AppProject in `athleto-infra`, then cut over from the in-pod-build
manifest.
