# athleto-app-rs — task runner. Run `just` to see everything.
#
# Environment secrets live in env/enc/*.env.enc, encrypted with sops + age and
# committed to this repo. See env/README.md for the workflow.

set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load := false

# Show available recipes.
default:
    @just --list

# ─── encrypted environment files (sops + age) ───────────────────────────────
#
# Secrets live in env/enc/{dev,prod}.env.enc, encrypted with sops + age and
# committed to this repo; plaintext is decrypted to env/dec/*.env (gitignored,
# mode 0600) and the active profile is symlinked to ./.env. See env/README.md.
#
#   just env-keygen            once per machine
#   just env-decrypt dev       env/enc/dev.env.enc -> env/dec/dev.env
#   just env-use dev           .env -> env/dec/dev.env (relative, managed)
#   just env-run prod <cmd…>   no plaintext ever touches disk
#   just env-check             fail-closed audit — runs in CI (secrets-audit)

import '.just/env.just'

# Activate <name>: decrypt it and point ./.env at env/dec/<name>.env.
# The link is relative and is only ever replaced when it already points into
# env/dec/ — an unmanaged .env file or foreign symlink is never clobbered.
[group('env')]
env-use name: (env-decrypt name)
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ justfile_directory() }}"
    target="env/dec/{{ name }}.env"
    [[ -f $target ]] || { echo "missing $target" >&2; exit 1; }
    if [[ -e .env || -L .env ]]; then
      if [[ -L .env ]] && [[ $(readlink .env) == env/dec/*.env ]]; then
        rm -f .env
      else
        echo "refusing to replace unmanaged .env (not a symlink into env/dec/)" >&2
        exit 1
      fi
    fi
    ln -s "$target" .env
    echo ".env -> $target"

# Deactivate: remove the managed ./.env symlink (never an unmanaged file).
[group('env')]
env-unuse:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ justfile_directory() }}"
    if [[ -L .env ]] && [[ $(readlink .env) == env/dec/*.env ]]; then
      rm -f .env; echo "removed .env symlink"
    elif [[ -e .env ]]; then
      echo "refusing to remove unmanaged .env" >&2; exit 1
    else
      echo "no .env to remove"
    fi
