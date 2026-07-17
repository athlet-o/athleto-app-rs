{ pkgs }:
let
  shellPackages = with pkgs; [
    bacon
    cargo
    clippy
    curl
    git
    jq
    postgresql_16
    rust-analyzer
    rustc
    rustfmt
    sqlx-cli
  ];
in
pkgs.mkShell {
  packages = shellPackages;

  LANG = if pkgs.stdenv.hostPlatform.isDarwin then "en_US.UTF-8" else "C.UTF-8";
  LC_ALL = if pkgs.stdenv.hostPlatform.isDarwin then "en_US.UTF-8" else "C.UTF-8";

  shellHook = ''
    export NIX_DEV_SHELL=athleto-app-rs

    # Local Supabase credentials (never committed): maps ATHLETO_* to the env
    # vars the app reads, so `cargo run` works out of the box.
    if [ -f "$HOME/.config/athlet-o/secrets.env" ]; then
      . "$HOME/.config/athlet-o/secrets.env"
      export SUPABASE_URL="''${SUPABASE_URL:-$ATHLETO_SUPABASE_URL}"
      export SUPABASE_ANON_KEY="''${SUPABASE_ANON_KEY:-$ATHLETO_SUPABASE_ANON_KEY}"
      export DATABASE_URL="''${DATABASE_URL:-$ATHLETO_DATABASE_URL}"
    fi
  '';
}
