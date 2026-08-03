#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: init-amika-locality-snapshot.sh --api-url <origin> [options]

Creates or reuses a remote Amika sandbox, installs the verified Locality v0.3.7
CLI, and materializes a scoped workspace snapshot. It then runs one inline
Notion-only scenario and prints both the prompt and generated report. The
Workspace Profile key created in Admin is read from standard input and is never
passed in a command-line argument.

Options:
  --api-url <origin>       Locality backend API origin (required).
  --name <name>            Amika sandbox name. Default: locality-snapshot-<UTC>.
  --reuse                   Reuse the explicitly named, already-started sandbox.
  --model <model>          Model passed to codex exec.
                           Default: CODEX_MODEL or gpt-5.6-sol.
  --reasoning <effort>     Reasoning effort passed to codex exec.
                           Default: CODEX_REASONING_EFFORT or low.
  --help, -h               Show this help.

Example:
  printf '%s\n' "$LOCALITY_PROFILE_KEY" | \
    scripts/init-amika-locality-snapshot.sh \
      --api-url https://api.dev.locality.dev
EOF
}

fail() {
  printf 'init Amika Locality snapshot: %s\n' "$*" >&2
  exit 2
}

encode_remote_argv() {
  local payload
  payload="$(printf '%s\0' "$@" | base64 | tr -d '\n')"
  printf 'python3 -c '\''import base64, os, sys; argv = [os.fsdecode(item) for item in base64.b64decode(sys.argv[1]).split(b"\\0")[:-1]]; os.execvp(argv[0], argv)'\'' %s' "$payload"
}

amika_ssh() {
  local sandbox="$1"
  local remote_command
  shift
  if [ "${1:-}" = "--" ]; then
    shift
  fi
  remote_command="$(encode_remote_argv "$@")"
  command -v expect >/dev/null 2>&1 || fail "expect is required for Amika PTY transport"
  AMIKA_SANDBOX_NAME="$sandbox" AMIKA_REMOTE_COMMAND="$remote_command" \
    expect -c '
      proc child_status {result} {
        if {[llength $result] >= 6 && [lindex $result 4] eq "CHILDKILLED"} {
          array set signal_number {
            SIGHUP 1 SIGINT 2 SIGQUIT 3 SIGKILL 9 SIGPIPE 13 SIGTERM 15
          }
          set signal [lindex $result 5]
          if {[info exists signal_number($signal)]} {
            return [expr {128 + $signal_number($signal)}]
          }
          return 125
        }
        if {[lindex $result 2] != 0} {
          return 125
        }
        return [lindex $result 3]
      }
      set timeout 1800
      spawn -noecho amika sandbox ssh -t $env(AMIKA_SANDBOX_NAME) -- $env(AMIKA_REMOTE_COMMAND)
      expect {
        eof {
          set result [wait]
          exit [child_status $result]
        }
        timeout {
          catch {close}
          catch {wait}
          puts stderr "Amika operation did not finish within 30 minutes"
          exit 124
        }
      }
    '
}

amika_ssh_secret_line() {
  local sandbox="$1"
  local secret="$2"
  local attempt=1
  local status
  local remote_command
  shift 2
  if [ "${1:-}" = "--" ]; then
    shift
  fi
  command -v expect >/dev/null 2>&1 || fail "expect is required for Amika credential transfer"
  remote_command="$(encode_remote_argv "$@")"

  while [ "$attempt" -le 3 ]; do
    if printf '%s\n' "$secret" | \
      AMIKA_SANDBOX_NAME="$sandbox" AMIKA_REMOTE_COMMAND="$remote_command" \
      expect -c '
        proc child_status {result} {
          if {[llength $result] >= 6 && [lindex $result 4] eq "CHILDKILLED"} {
            array set signal_number {
              SIGHUP 1 SIGINT 2 SIGQUIT 3 SIGKILL 9 SIGPIPE 13 SIGTERM 15
            }
            set signal [lindex $result 5]
            if {[info exists signal_number($signal)]} {
              return [expr {128 + $signal_number($signal)}]
            }
            return 125
          }
          if {[lindex $result 2] != 0} {
            return 125
          }
          return [lindex $result 3]
        }
        set timeout 30
        if {[gets stdin secret] < 0} {
          puts stderr "credential input closed before a secret was read"
          exit 65
        }
        spawn -noecho amika sandbox ssh -t $env(AMIKA_SANDBOX_NAME) -- $env(AMIKA_REMOTE_COMMAND)
        expect {
          "__LOCALITY_STDIN_READY__" {}
          eof {
            catch {wait}
            puts stderr "Amika SSH closed before requesting credential input"
            exit 75
          }
          timeout {
            catch {close}
            catch {wait}
            puts stderr "Amika SSH did not request credential input within 30 seconds"
            exit 75
          }
        }
        send -- "$secret\n"
        set secret ""
        set timeout 1800
        expect {
          eof {
            set result [wait]
            exit [child_status $result]
          }
          timeout {
            catch {close}
            catch {wait}
            puts stderr "Amika credential operation did not finish within 30 minutes; it was not retried"
            exit 124
          }
        }
      '; then
      secret=""
      return 0
    else
      status=$?
    fi

    if [ "$status" -ne 75 ] || [ "$attempt" -eq 3 ]; then
      secret=""
      return "$status"
    fi
    printf 'Amika credential transport closed before secret delivery; retrying (%s/3)...\n' "$attempt" >&2
    attempt=$((attempt + 1))
    sleep 1
  done
}

API_URL=""
SANDBOX_NAME="locality-snapshot-$(date -u +%Y%m%d-%H%M%S)"
SANDBOX_NAME_EXPLICIT=false
REUSE_SANDBOX=false
REMOTE_ROOT="/home/amika/locality-snapshot"
LOC_RELEASE_VERSION="0.3.7"
LOC_RELEASE_DEB_SHA256="692b05460839ba44b85cd1e6b3b6969ad4a3f62f3e81f420c4651159ad7ef195"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
AZURE_OPENAI_BASE_URL="${AZURE_OPENAI_BASE_URL:-https://aseem-mp32maxp-eastus2.openai.azure.com/openai/v1}"
AZURE_OPENAI_API_KEY_VALUE="${AZURE_OPENAI_API_KEY:-}"
unset AZURE_OPENAI_API_KEY
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --api-url)
      [ "$#" -ge 2 ] || fail "--api-url requires a value"
      API_URL="$2"
      shift 2
      ;;
    --name)
      [ "$#" -ge 2 ] || fail "--name requires a value"
      SANDBOX_NAME="$2"
      SANDBOX_NAME_EXPLICIT=true
      shift 2
      ;;
    --reuse)
      REUSE_SANDBOX=true
      shift
      ;;
    --model)
      [ "$#" -ge 2 ] || fail "--model requires a value"
      CODEX_MODEL="$2"
      shift 2
      ;;
    --reasoning)
      [ "$#" -ge 2 ] || fail "--reasoning requires a value"
      CODEX_REASONING_EFFORT="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[ -n "$API_URL" ] || fail "--api-url is required"
case "$API_URL" in
  https://*) ;;
  *) fail "--api-url must use https" ;;
esac
case "$SANDBOX_NAME" in
  *[!a-zA-Z0-9._-]*|'') fail "--name contains unsupported characters" ;;
esac
if [ "$REUSE_SANDBOX" = true ] && [ "$SANDBOX_NAME_EXPLICIT" != true ]; then
  fail "--reuse requires an explicit --name"
fi
[ -n "$CODEX_MODEL" ] || fail "--model must not be empty"
[ -n "$AZURE_OPENAI_API_KEY_VALUE" ] || fail "AZURE_OPENAI_API_KEY is required"
case "$AZURE_OPENAI_BASE_URL" in
  https://*) ;;
  *) fail "AZURE_OPENAI_BASE_URL must use https" ;;
esac
case "$CODEX_REASONING_EFFORT" in
  low|medium|high|xhigh|max|ultra) ;;
  *) fail "--reasoning must be low, medium, high, xhigh, max, or ultra" ;;
esac

scenario_prompt() {
  cat <<'EOF'
Use the scoped Notion workspace snapshot at `/home/amika/locality-snapshot` for all Notion context.
Use the repository checkout at `/home/amika/workspace/locality`.
Write the final report to `/home/amika/final_report.md`.

You are preparing a launch gate memo for Locality. Find the relevant Notion
context under `/home/amika/locality-snapshot` and recent code changes from
`/home/amika/workspace/locality`, decide what is actually proven, what is still
unverified, and what should block launch. Produce a concise Markdown memo.

Do not use direct Notion API tools in this run. Do not create a new Notion page
or modify existing Notion pages.

Write the final Markdown report to `/home/amika/final_report.md`.

Report format:

# Locality Launch Gate Memo

## Recommendation

## Evidence Reviewed

## Proven

## Unverified

## Launch Blockers

## Required Validation

The memo should be concise, specific, and grounded in evidence. If a claim
cannot be verified from git, gh, or Locality context, say so.
EOF
}

EFFECTIVE_PROMPT="$(scenario_prompt)"

command -v amika >/dev/null 2>&1 || fail "amika is not available on PATH"
command -v git >/dev/null 2>&1 || fail "git is not available on PATH"
SOURCE_REVISION="$(git -C "$REPO_ROOT" rev-parse HEAD)" || fail "could not resolve source revision"

if [ -t 0 ]; then
  stty -echo
  IFS= read -r PROFILE_KEY || {
    stty echo
    fail "read the Workspace Profile key from standard input"
  }
  stty echo
  printf '\n'
else
  IFS= read -r PROFILE_KEY || fail "read the Workspace Profile key from standard input"
fi
[ "${#PROFILE_KEY}" -eq 64 ] || fail "Workspace Profile key must be 64 lowercase hexadecimal characters"
case "$PROFILE_KEY" in
  *[!0-9a-f]*) fail "Workspace Profile key must be 64 lowercase hexadecimal characters" ;;
esac

if [ "$REUSE_SANDBOX" = true ]; then
  printf 'Reusing Amika sandbox %s...\n' "$SANDBOX_NAME"
else
  printf 'Creating Amika sandbox %s...\n' "$SANDBOX_NAME"
  (cd "$REPO_ROOT" && amika sandbox create \
    --remote \
    --name "$SANDBOX_NAME" \
    --no-git \
    --yes >/dev/null)
fi

printf 'Installing released loc CLI v%s in %s...\n' "$LOC_RELEASE_VERSION" "$SANDBOX_NAME"
amika_ssh "$SANDBOX_NAME" -- sh -c '
  set -eu
  loc_version=$1
  expected_sha256=$2
  work_dir=$(mktemp -d)
  trap '\''rm -rf "$work_dir"'\'' EXIT
  package="$work_dir/Locality_Linux_v${loc_version}.deb"
  curl -fsSL \
    -o "$package" \
    "https://github.com/codeflash-ai/locality/releases/download/v${loc_version}/Locality_Linux_v${loc_version}.deb"
  printf "%s  %s\n" "$expected_sha256" "$package" | sha256sum -c -
  dpkg-deb -x "$package" "$work_dir/package"
  test -x "$work_dir/package/usr/bin/loc"
  mkdir -p "$HOME/.local/bin"
  install -m 0755 "$work_dir/package/usr/bin/loc" "$HOME/.local/bin/loc"
  "$HOME/.local/bin/loc" sandbox init --help >/dev/null
' sh "$LOC_RELEASE_VERSION" "$LOC_RELEASE_DEB_SHA256"

if [ "$REUSE_SANDBOX" = true ]; then
  printf 'Checking reused sandbox evidence boundary before authorization...\n'
  amika_ssh "$SANDBOX_NAME" -- sh -c '
    set -eu
    repo_dir=/home/amika/workspace/locality
    if [ -e "$repo_dir" ]; then
      test "$(git -C "$repo_dir" rev-parse --is-inside-work-tree)" = true
      origin_url=$(git -C "$repo_dir" remote get-url origin)
      case "$origin_url" in
        https://github.com/codeflash-ai/locality|https://github.com/codeflash-ai/locality.git|git@github.com:codeflash-ai/locality.git) ;;
        *) printf "unexpected Locality repository origin: %s\n" "$origin_url" >&2; exit 65 ;;
      esac
      test -z "$(git -C "$repo_dir" status --porcelain --untracked-files=all)" || {
        printf "Locality evidence checkout is dirty; use a clean sandbox or remove the changes explicitly\n" >&2
        exit 65
      }
    fi
  ' sh
fi

printf 'Materializing scoped workspace at %s:%s...\n' "$SANDBOX_NAME" "$REMOTE_ROOT"
amika_ssh_secret_line "$SANDBOX_NAME" "$PROFILE_KEY" -- sh -c '
  set -eu
  if [ -t 0 ]; then
    stty -echo
  fi
  printf "__LOCALITY_STDIN_READY__\n"
  IFS= read -r profile_key
  if [ -t 0 ]; then
    stty echo
  fi
  printf "%s\n" "$profile_key" | "$HOME/.local/bin/loc" sandbox init "$@"
' sh \
    --api-url "$API_URL" \
    --root "$REMOTE_ROOT" \
    --profile-key-stdin \
    --profile \
    --json
unset PROFILE_KEY

printf 'Snapshot ready in Amika sandbox %s at %s\n' "$SANDBOX_NAME" "$REMOTE_ROOT"

printf 'Preparing clean Locality evidence checkout at revision %s...\n' "$SOURCE_REVISION"
amika_ssh "$SANDBOX_NAME" -- sh -c '
  set -eu
  revision=$1
  repo_dir=/home/amika/workspace/locality
  mkdir -p /home/amika/workspace
  if [ ! -e "$repo_dir" ]; then
    git clone https://github.com/codeflash-ai/locality.git "$repo_dir"
  fi
  test "$(git -C "$repo_dir" rev-parse --is-inside-work-tree)" = true
  origin_url=$(git -C "$repo_dir" remote get-url origin)
  case "$origin_url" in
    https://github.com/codeflash-ai/locality|https://github.com/codeflash-ai/locality.git|git@github.com:codeflash-ai/locality.git) ;;
    *) printf "unexpected Locality repository origin: %s\n" "$origin_url" >&2; exit 65 ;;
  esac
  test -z "$(git -C "$repo_dir" status --porcelain --untracked-files=all)" || {
    printf "Locality evidence checkout is dirty; use a clean sandbox or remove the changes explicitly\n" >&2
    exit 65
  }
  git -C "$repo_dir" fetch origin --prune
  git -C "$repo_dir" cat-file -e "$revision^{commit}"
  git -C "$repo_dir" checkout --detach "$revision"
' sh "$SOURCE_REVISION"

amika_ssh "$SANDBOX_NAME" -- sh -c '
  set -eu
  azure_base_url=$1
  model=$2
  reasoning=$3
  AZURE_OPENAI_BASE_URL="$azure_base_url" \
    CODEX_MODEL="$model" \
    CODEX_REASONING_EFFORT="$reasoning" \
    AMIKA_AGENT_CWD="$HOME" \
    bash /home/amika/workspace/locality/experiment/locality-mcp-comparison/setup-codex-azure.sh
  test -z "$(git -C /home/amika/workspace/locality status --porcelain --untracked-files=all)" || {
    printf "Codex setup dirtied the Locality evidence checkout\n" >&2
    exit 65
  }
' sh "$AZURE_OPENAI_BASE_URL" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT"

PROMPT_BASE64="$(printf '%s\n' "$EFFECTIVE_PROMPT" | base64 | tr -d '\n')"
amika_ssh "$SANDBOX_NAME" -- sh -c '
  printf "%s" "$1" | base64 -d > /home/amika/scenario-prompt.md
' sh "$PROMPT_BASE64"
unset PROMPT_BASE64

printf '\n===== Inline scenario prompt =====\n%s\n' "$EFFECTIVE_PROMPT"
printf '\n===== Running scenario in %s =====\n' "$SANDBOX_NAME"

amika_ssh_secret_line "$SANDBOX_NAME" "$AZURE_OPENAI_API_KEY_VALUE" -- sh -c '
  set -eu
  model=$1
  reasoning=$2
  snapshot_root=$3
  if [ -t 0 ]; then
    stty -echo
  fi
  printf "__LOCALITY_STDIN_READY__\n"
  IFS= read -r azure_api_key
  if [ -t 0 ]; then
    stty echo
  fi
  test -n "$azure_api_key"
  command -v codex >/dev/null 2>&1 || {
    printf "codex is not installed in the Amika sandbox\n" >&2
    exit 127
  }
  rm -f /home/amika/final_report.md /home/amika/scenario-codex.jsonl
  prompt=$(cat /home/amika/scenario-prompt.md)
  AZURE_OPENAI_API_KEY="$azure_api_key" codex exec \
    --json \
    --model "$model" \
    -c "model_reasoning_effort=\"$reasoning\"" \
    --dangerously-bypass-approvals-and-sandbox \
    --disable hooks \
    -C /home/amika/workspace/locality \
    --add-dir "$snapshot_root" \
    "$prompt" < /dev/null > /home/amika/scenario-codex.jsonl
  unset azure_api_key
  test -s /home/amika/final_report.md || {
    printf "scenario did not create /home/amika/final_report.md\n" >&2
    exit 1
  }
' sh "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" "$REMOTE_ROOT"
AZURE_OPENAI_API_KEY_VALUE=""
unset AZURE_OPENAI_API_KEY_VALUE

amika_ssh "$SANDBOX_NAME" -- sh -c '
  test -z "$(git -C /home/amika/workspace/locality status --porcelain --untracked-files=all)" || {
    printf "scenario modified the Locality evidence checkout\n" >&2
    exit 65
  }
' sh

printf '\n===== /home/amika/final_report.md =====\n'
amika_ssh "$SANDBOX_NAME" -- cat /home/amika/final_report.md
