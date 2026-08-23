# athleto-app-rs — task runner. Run `just` to see everything.
#
# Environment secrets live in env/enc/*.env.enc, encrypted with sops + age and
# committed to this repo. `just env-use <name>` decrypts to env/dec/<name>.env
# and points ./.env at it. See env/README.md for the workflow.

import '.just/env.just'

# Show available recipes.
default:
    @just --list

alias use := env-use
alias edit := env-edit
alias audit := env-audit
