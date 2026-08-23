# Environment files

Secrets for this repo are **committed, encrypted**, with [sops] + [age].

```
env/enc/<name>.env.enc   ciphertext — committed. This is the source of truth.
env/dec/<name>.env       plaintext  — gitignored, mode 0600, disposable.
.env -> env/dec/<name>.env   relative symlink, gitignored; `just env-use` manages it.
```

`env/dec` and `.env` are build artifacts. Delete them whenever you like and
regenerate with `just env-use <name>`; nothing there is authoritative.

Two environments are tracked: `dev` and `prod`. That is the whole contract —
the `.gitignore` allow-list admits exactly `env/enc/dev.env.enc` and
`env/enc/prod.env.enc`, so a stray `env/enc/staging.env.enc` cannot be
committed by accident.

## First run on a new machine

```sh
mkdir -p ~/.config/sops/age && age-keygen -o ~/.config/sops/age/keys.txt   # once per machine
just env-key        # prints your public recipient — send it to a maintainer
```

A maintainer adds your recipient to `.sops.yaml`, runs `just env-rekey`, and
commits. Until then you cannot decrypt anything. After that:

```sh
just env-use dev    # env/enc/dev.env.enc -> env/dec/dev.env, and .env -> env/dec/dev.env
just env-audit      # confirms nothing plaintext is tracked and the ciphertext is sound
```

Inside the Nix dev shell (`nix develop`) the tooling — `sops`, `age`, `just`,
`ores-sops` — is already on `PATH`, and the shell hook installs the git hooks
that keep `.env` current after a merge or checkout.

## Day to day

| Command | What it does |
|---|---|
| `just env-use <name>` | decrypt and activate: `env/dec/<name>.env`, `.env -> env/dec/<name>.env` |
| `just env-status` | per-environment state; `*` marks the active one |
| `just env-edit <name>` | edit the ciphertext in `$EDITOR`; plaintext never hits disk |
| `just env-encrypt <name>` | fold hand edits to `env/dec/<name>.env` back into the ciphertext |
| `just env-diff <name>` | variable *names* that differ between `env/dec` and the ciphertext |
| `just env-list` | environments and the variable names in each (never values) |
| `just env-run <name> <cmd…>` | run `cmd` with those variables exported, no file involved |
| `just env-docker-run <name> <docker run args…>` | inject at `docker run` time, image needs no key |
| `just env-refresh` | re-decrypt the active env if its ciphertext changed |
| `just env-lock` | wipe `env/dec` and the `.env` symlink |
| `just env-rekey` | re-sync recipients after editing `.sops.yaml` |
| `just env-audit` | fail-closed, keyless audit — runs in CI |
| `just env-doctor` | which tools and keys this host has |

Prefer `just env-edit` over decrypt-edit-encrypt. Both work, but `env-edit`
re-encrypts only the values you actually changed, so the diff names them:

```
-DATABASE_URL=ENC[AES256_GCM,data:OG3trz…]
+DATABASE_URL=ENC[AES256_GCM,data:9fKq2a…]
```

A bare `sops encrypt` is not — it gives every line a fresh IV and rewrites the
whole file, which makes review useless and guarantees merge conflicts. Don't
call sops directly; use the recipes.

## What is and isn't hidden

Variable **names are plaintext** in `env/enc/*.env.enc`; only values are
encrypted. That is the point — it makes diffs reviewable and lets `env-list`
work without a key. Never encode a secret in a variable *name*. Comments are
encrypted, so anything explanatory belongs in this file or `.env.example`.

Two format limits, inherited from sops' dotenv parser:

- **No multi-line values.** A PEM must be a single line with `\n` escapes.
- **Blank lines are dropped** on round-trip. Cosmetic only.

## Containers

Decryption happens at **`docker run`**, never at `docker build`. A secret
decrypted during a build is written into an image layer and stays there — a
later `RUN rm` does not remove it, and `--build-arg` is worse still because it
lands in `docker history`.

The Dockerfile therefore ships only *ciphertext* plus the `sops` binary, and
`scripts/sops-entrypoint.sh` decrypts into the process environment at start-up
before `exec`-ing the real command (so the app is PID 1 and still receives
`SIGTERM`). The key arrives at run time and is never in the image:

```sh
docker run --rm -e SOPS_AGE_KEY="$(cat ~/.config/sops/age/keys.txt)" ghcr.io/org/app
# or, with a platform secret store mounted on tmpfs:
docker run --rm -e SOPS_AGE_KEY_FILE=/run/secrets/age.key -v …:/run/secrets/age.key:ro ghcr.io/org/app
# which environment is baked in (ciphertext only) is a build arg, default prod:
docker build --build-arg SOPS_ENV=dev -t app:dev .
```

Without a key the entrypoint runs the command unchanged (so `--help`, tests and
platform-injected configuration keep working); set `SOPS_REQUIRE_KEY=1` to make
a missing key a hard failure. For images with no shell (distroless, scratch),
inject host-side instead: `just env-docker-run prod <image>`.

## Rules

- Never commit anything from `env/dec/` or a plain `.env`. `.gitignore` and
  `just env-audit` both block it; don't defeat them with `git add -f`.
- Never commit a private age key. It belongs only in
  `~/.config/sops/age/keys.txt`, mode 0600.
- Removing a recipient does not un-leak anything. Rotate the credentials too.
- Files ending in `.env` are gitignored repo-wide. If a repo has a legitimate
  non-secret `*.env`, allow it with an explicit `!` rule — deny by default,
  permit narrowly.

[sops]: https://github.com/getsops/sops
[age]: https://github.com/FiloSottile/age
