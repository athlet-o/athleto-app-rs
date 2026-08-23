# ─── encrypted environment files (sops + age) ────────────────────────────────
#
# SHARED MODULE — keep byte-identical across repos; fix it in one place and
# propagate, do not fork it per repo.
#
#   env/enc/<name>.env.enc   ciphertext — COMMITTED to git (the source of truth)
#   env/dec/<name>.env       plaintext  — GITIGNORED, mode 0600, disposable
#   .env -> env/dec/<name>.env          relative symlink, gitignored, managed
#
# The files are dotenv under the hood: variable NAMES stay readable in the
# ciphertext so a diff shows exactly which values changed. `.enc` is not a
# format sops auto-detects, so every direct sops call states the types.
#
#   just env-key                     this machine's age recipient → .sops.yaml
#   just env-use dev                 decrypt env/enc/dev.env.enc → env/dec/dev.env → ./.env
#   just env-edit prod               change a value in place (no plaintext on disk)
#   just env-run dev cargo run       run a command with the variables exported
#   just env-audit                   fail-closed, keyless audit — wired into CI
#
# The mechanics are delegated to `ores-sops` (github.com/ORESoftware/ores-sops),
# the single implementation shared across orgs. It comes from the Nix devShell;
# without it on PATH the recipes fall back to `nix run`.

_ores_sops := if `command -v ores-sops >/dev/null 2>&1 && echo yes || echo no` == "yes" { "ores-sops" } else { "nix run github:ORESoftware/ores-sops --" }
_enc := justfile_directory() / "env" / "enc"
_dec := justfile_directory() / "env" / "dec"
_sops_fmt := "--input-type dotenv --output-type dotenv"
_age_key := env_var_or_default("SOPS_AGE_KEY_FILE", env_var_or_default("HOME", "") / ".config/sops/age/keys.txt")

# env/dec is runtime-only and deliberately absent from git; an empty ignored
# directory does not survive a clone, so every recipe recreates the 0700
# plaintext boundary first.
[private]
_env-dec:
    #!/usr/bin/env bash
    set -euo pipefail
    path="{{ _dec }}"
    [[ ! -L "{{ justfile_directory() }}/env" && ! -L "$path" ]] || { echo "env/ and env/dec must not be symlinks" >&2; exit 1; }
    [[ ! -e "$path" || -d "$path" ]] || { echo "env/dec must be a directory" >&2; exit 1; }
    umask 077; mkdir -p "$path"; chmod 700 "$path"

# Decrypt env/enc/<name>.env.enc → env/dec/<name>.env and point ./.env at it.
[group('env')]
env-use name: _env-dec
    @{{ _ores_sops }} use {{ name }}

# Per-environment state; * marks the one ./.env points at.
[group('env')]
env-status: _env-dec
    @{{ _ores_sops }} status

# Edit an encrypted environment in $EDITOR; plaintext never lands on disk.
[group('env')]
env-edit name: _env-dec
    @{{ _ores_sops }} edit {{ name }}

# Fold env/dec/<name>.env edits back into env/enc/<name>.env.enc (minimal diff).
[group('env')]
env-encrypt name: _env-dec
    @{{ _ores_sops }} encrypt {{ name }}

# Variable names that differ between env/dec/<name>.env and the ciphertext.
[group('env')]
env-diff name: _env-dec
    @{{ _ores_sops }} diff {{ name }}

# Re-decrypt the active environment if its ciphertext changed (git hooks call this).
[group('env')]
env-refresh: _env-dec
    @{{ _ores_sops }} refresh

# Remove decrypted plaintext and the ./.env symlink.
[group('env')]
env-lock: _env-dec
    @{{ _ores_sops }} lock

# Install the post-merge/post-checkout refresh hooks and the pre-commit guard.
[group('env')]
env-hooks:
    @{{ _ores_sops }} install-hooks

# List every environment and the variable NAMES it defines (never values).
[group('env')]
env-list:
    #!/usr/bin/env bash
    set -euo pipefail
    shopt -s nullglob
    files=({{ _enc }}/*.env.enc)
    [[ ${#files[@]} -gt 0 ]] || { echo "no environments in env/enc/"; exit 0; }
    for f in "${files[@]}"; do
      n=$(basename "$f" .env.enc)
      keys=$( { grep -oE '^[A-Za-z_][A-Za-z0-9_]*=' "$f" || true; } | tr -d '=' | { grep -v '^sops_' || true; } | tr '\n' ' ')
      printf '%-8s %s\n' "$n" "${keys:-(empty — populate with: just env-edit $n)}"
    done

# Run a command with <name>'s variables exported. Plaintext never touches disk.
[group('env')]
env-run name +cmd: _env-dec
    #!/usr/bin/env bash
    set -euo pipefail
    src="{{ _enc }}/{{ name }}.env.enc"
    [[ -f $src ]] || { echo "no such environment: {{ name }}" >&2; exit 1; }
    # Parsed with read+export, never eval: a value containing $(...) must stay a string.
    while IFS='=' read -r k v; do
      case "$k" in ''|'#'*|sops_*) continue ;; esac
      export "$k=$v"
    done < <(sops decrypt {{ _sops_fmt }} "$src")
    exec {{ cmd }}

# Run a container with <name>'s variables injected at RUN time (image needs no key).
# Everything after <name> goes to `docker run` verbatim:
#   just env-docker-run prod -p 8080:8080 ghcr.io/org/app:dev
[group('docker')]
env-docker-run name +args: _env-dec
    #!/usr/bin/env bash
    set -euo pipefail
    src="{{ _enc }}/{{ name }}.env.enc"
    [[ -f $src ]] || { echo "no such environment: {{ name }}" >&2; exit 1; }
    command -v docker >/dev/null || { echo "docker not found" >&2; exit 1; }
    # Materialise BEFORE invoking docker so a failed decrypt is a hard stop, not
    # a container that starts with half its secrets.
    envdata=$(sops decrypt {{ _sops_fmt }} "$src" | grep -vE '^(sops_|#)' || true)
    exec docker run --env-file <(printf '%s\n' "$envdata") {{ args }}

# Print this machine's age recipient (public; safe to share and commit).
[group('env')]
env-key:
    #!/usr/bin/env bash
    set -euo pipefail
    for p in "{{ _age_key }}" "$HOME/Library/Application Support/sops/age/keys.txt"; do
      [[ -f $p ]] && { age-keygen -y "$p"; exit 0; }
    done
    echo "no age key found — create one: mkdir -p ~/.config/sops/age && age-keygen -o ~/.config/sops/age/keys.txt" >&2
    exit 1

# Re-encrypt every environment to match .sops.yaml after adding/removing a recipient.
[group('env')]
env-rekey:
    #!/usr/bin/env bash
    set -euo pipefail
    shopt -s nullglob
    for f in {{ _enc }}/*.env.enc; do sops updatekeys --yes --input-type dotenv "$f"; done
    echo "recipients synced — commit env/enc/"

# Fail-closed, KEYLESS audit: no plaintext tracked, .gitignore intact, ciphertext valid. CI-safe.
[group('env')]
env-audit:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ justfile_directory() }}"
    fail=0; note() { echo "  ✗ $1"; fail=1; }
    echo "tracked files:"
    bad=$(git ls-files -z | tr '\0' '\n' | grep -E '(^|/)\.env$|\.env$|(^|/)\.env\.[^/]*$|(^|/)env/dec/' | grep -vE '\.(example|sample|template)$' || true)
    [[ -z $bad ]] && echo "  ✓ no plaintext dotenv tracked" || { note "plaintext env files are tracked:"; echo "$bad" | sed 's/^/      /'; }
    keys=$(git grep -l -E 'AGE-SECRET-KEY-1' -- . ':!env/README.md' ':!.just/env.just' 2>/dev/null || true)
    [[ -z $keys ]] && echo "  ✓ no age private keys tracked" || { note "possible private keys tracked:"; echo "$keys" | sed 's/^/      /'; }
    echo ".gitignore:"
    for rule in '*.env' '**/*.env' '**/**/*.env' 'env/dec'; do
      grep -qxF -- "$rule" .gitignore && echo "  ✓ $rule" || note "missing .gitignore rule: $rule"
    done
    echo "ciphertext:"
    shopt -s nullglob; found=0
    for f in env/enc/*.env.enc; do
      found=1
      grep -q 'ENC\[AES256_GCM' "$f" || { note "$f is NOT sops-encrypted"; continue; }
      grep -q '^sops_mac=' "$f" || note "$f has no sops MAC (truncated or hand-edited?)"
      r=$(grep -c 'map_recipient' "$f" || true)
      [[ $r -ge 2 ]] && echo "  ✓ $f encrypted, $r recipients" || note "$f has $r recipient(s); needs >= 2 so a lost key is survivable"
    done
    [[ $found -eq 1 ]] || note "no env/enc/*.env.enc present"
    [[ $fail -eq 0 ]] && echo $'\nenv-audit PASSED' || { echo $'\nenv-audit FAILED'; exit 1; }

# Report which env tools are present on this host.
[group('env')]
env-doctor:
    #!/usr/bin/env bash
    set -uo pipefail
    for t in sops age-keygen just git ores-sops nix docker; do
      if command -v "$t" >/dev/null 2>&1; then printf '  ✓ %-10s %s\n' "$t" "$(command -v "$t")"; else printf '  · %-10s missing\n' "$t"; fi
    done
    k=""; for p in "{{ _age_key }}" "$HOME/Library/Application Support/sops/age/keys.txt"; do [[ -f $p ]] && { k=$p; break; }; done
    [[ -n ${SOPS_AGE_KEY:-} ]] && echo "  ✓ SOPS_AGE_KEY set" || { [[ -n $k ]] && echo "  ✓ age key $k" || echo "  · no age key (keyless checks still work; this is normal in CI)"; }
    [[ -L .env ]] && echo "  .env -> $(readlink .env)" || echo "  .env: none active — run: just env-use dev"
