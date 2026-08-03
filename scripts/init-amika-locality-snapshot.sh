#!/usr/bin/env bash
set -euo pipefail
set +a
AZURE_OPENAI_API_KEY_CAPTURE="${AZURE_OPENAI_API_KEY:-}"
unset PROFILE_KEY LOCALITY_PROFILE_KEY AMIKA_SECRET_LINE \
  AZURE_OPENAI_API_KEY AZURE_OPENAI_API_KEY_VALUE
AZURE_OPENAI_API_KEY_VALUE="$AZURE_OPENAI_API_KEY_CAPTURE"
unset AZURE_OPENAI_API_KEY_CAPTURE
export -n AZURE_OPENAI_API_KEY_VALUE

usage() {
  cat <<'EOF'
Usage: init-amika-locality-snapshot.sh --api-url <origin> [options]

Creates a fresh remote Amika sandbox, installs the verified Locality v0.3.7
CLI, and materializes a scoped workspace snapshot. It then runs one inline
Notion-only scenario and prints both the prompt and generated report. The
Workspace Profile key created in Admin is read from standard input and is never
passed in a command-line argument.

Options:
  --api-url <origin>       Locality backend API origin (required).
  --name <name>            Amika sandbox name. Default: locality-snapshot-<UTC>.
  --reuse                   Refused: credential-bearing sandboxes must be fresh.
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

ACTIVE_CHILD_PID=""
PENDING_CHILD_SIGNAL=""
TERMINAL_STATE=""
TERMINAL_STATE_ACTIVE=false
ACTIVE_DELIVERY_DIR=""

restore_terminal_state() {
  local saved_state="${TERMINAL_STATE:-}"

  if [ "${TERMINAL_STATE_ACTIVE:-false}" != true ]; then
    return
  fi
  TERMINAL_STATE_ACTIVE=false
  TERMINAL_STATE=""
  stty "$saved_state" < /dev/tty 2>/dev/null || stty echo < /dev/tty 2>/dev/null || true
}

cleanup_delivery_marker() {
  local delivery_dir="${ACTIVE_DELIVERY_DIR:-}"

  if [ -z "$delivery_dir" ]; then
    return
  fi
  ACTIVE_DELIVERY_DIR=""
  rm -f -- "$delivery_dir/delivered" 2>/dev/null || true
  rmdir "$delivery_dir" 2>/dev/null || true
}

forward_and_reap_active_child() {
  local signal="$1"
  local child_pid="${ACTIVE_CHILD_PID:-}"

  if [ -z "$child_pid" ]; then
    return
  fi
  ACTIVE_CHILD_PID=""
  kill -s "$signal" "$child_pid" 2>/dev/null || true
  if wait_for_child_exit "$child_pid" 50; then
    return
  fi
  if [ "$signal" != TERM ]; then
    kill -TERM "$child_pid" 2>/dev/null || true
    if wait_for_child_exit "$child_pid" 25; then
      return
    fi
  fi
  kill -KILL "$child_pid" 2>/dev/null || true
  if ! wait_for_child_exit "$child_pid" 50; then
    printf 'init Amika Locality snapshot: could not reap child %s after SIGKILL\n' "$child_pid" >&2
  fi
}

wait_for_child_exit() {
  local child_pid="$1"
  local attempts="$2"
  local state

  while [ "$attempts" -gt 0 ]; do
    if ! kill -0 "$child_pid" 2>/dev/null; then
      wait "$child_pid" 2>/dev/null || true
      return 0
    fi
    state="$(ps -o stat= -p "$child_pid" 2>/dev/null || true)"
    case "$state" in
      ''|*Z*)
        wait "$child_pid" 2>/dev/null || true
        return 0
        ;;
    esac
    attempts=$((attempts - 1))
    sleep 0.01
  done
  return 1
}

cleanup_on_exit() {
  local status="$1"

  trap - EXIT
  trap '' HUP INT TERM
  restore_terminal_state
  forward_and_reap_active_child TERM
  cleanup_delivery_marker
  exit "$status"
}

terminate_for_signal() {
  local signal="$1"
  local signal_number="$2"

  trap '' HUP INT TERM
  PROFILE_KEY=""
  AZURE_OPENAI_API_KEY_VALUE=""
  restore_terminal_state
  forward_and_reap_active_child "$signal"
  cleanup_delivery_marker
  exit $((128 + signal_number))
}

trap 'cleanup_on_exit $?' EXIT
trap 'terminate_for_signal HUP 1' HUP
trap 'terminate_for_signal INT 2' INT
trap 'terminate_for_signal TERM 15' TERM

prepare_child_launch() {
  PENDING_CHILD_SIGNAL=""
  trap 'PENDING_CHILD_SIGNAL="HUP 1"' HUP
  trap 'PENDING_CHILD_SIGNAL="INT 2"' INT
  trap 'PENDING_CHILD_SIGNAL="TERM 15"' TERM
}

activate_child() {
  ACTIVE_CHILD_PID="$1"
  trap 'terminate_for_signal HUP 1' HUP
  trap 'terminate_for_signal INT 2' INT
  trap 'terminate_for_signal TERM 15' TERM
  case "$PENDING_CHILD_SIGNAL" in
    "HUP 1") terminate_for_signal HUP 1 ;;
    "INT 2") terminate_for_signal INT 2 ;;
    "TERM 15") terminate_for_signal TERM 15 ;;
  esac
}

wait_for_active_child() {
  local child_pid="$1"
  local status

  if wait "$child_pid"; then
    status=0
  else
    status=$?
  fi
  if [ "${ACTIVE_CHILD_PID:-}" = "$child_pid" ]; then
    ACTIVE_CHILD_PID=""
  fi
  return "$status"
}

encode_remote_argv() {
  local payload
  payload="$(printf '%s\0' "$@" | base64 | tr -d '\n')"
  printf 'python3 -c '\''import base64, os, sys; argv = [os.fsdecode(item) for item in base64.b64decode(sys.argv[1]).split(b"\\0")[:-1]]; os.execvp(argv[0], argv)'\'' %s' "$payload"
}

EXPECT_TRANSPORT_PROCS='
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

  proc restore_terminal {} {
    global terminal_state
    if {$terminal_state ne ""} {
      catch {exec stty $terminal_state < /dev/tty}
    } else {
      catch {exec stty sane < /dev/tty}
    }
  }

  proc reap_child {{signal ""}} {
    global child_active spawn_id
    if {!$child_active} {
      return
    }
    set child_pid ""
    catch {set child_pid [exp_pid -i $spawn_id]}
    if {$signal ne "" && [string is integer -strict $child_pid]} {
      catch {exec kill -$signal -- -$child_pid}
      after 1000
      catch {exec kill -KILL -- -$child_pid}
    }
    catch {close -i $spawn_id}
    catch {wait -i $spawn_id}
    set child_active 0
  }

  proc terminate_for_signal {signal number} {
    catch {set ::secret ""}
    reap_child $signal
    restore_terminal
    exit [expr {128 + $number}]
  }

  proc initialize_transport {} {
    global child_active terminal_state
    set child_active 0
    set terminal_state ""
    catch {set terminal_state [exec stty -g < /dev/tty]}
    trap {terminate_for_signal SIGHUP 1} SIGHUP
    trap {terminate_for_signal SIGINT 2} SIGINT
    trap {terminate_for_signal SIGTERM 15} SIGTERM
  }

  proc wait_for_child {} {
    global child_active spawn_id
    set result [wait -i $spawn_id]
    set child_active 0
    return $result
  }
'

amika_ssh() {
  local sandbox="$1"
  local remote_command
  shift
  if [ "${1:-}" = "--" ]; then
    shift
  fi
  remote_command="$(encode_remote_argv "$@")"
  command -v expect >/dev/null 2>&1 || fail "expect is required for Amika PTY transport"
  prepare_child_launch
  (
    trap - HUP INT TERM
    AMIKA_EXPECT_COMMON="$EXPECT_TRANSPORT_PROCS" \
      AMIKA_SANDBOX_NAME="$sandbox" AMIKA_REMOTE_COMMAND="$remote_command" \
      exec expect -c '
      eval $env(AMIKA_EXPECT_COMMON)
      initialize_transport
      set timeout 1800
      spawn -noecho amika sandbox ssh -t $env(AMIKA_SANDBOX_NAME) -- $env(AMIKA_REMOTE_COMMAND)
      set child_active 1
      expect {
        eof {
          set result [wait_for_child]
          restore_terminal
          exit [child_status $result]
        }
        timeout {
          reap_child SIGTERM
          restore_terminal
          puts stderr "Amika operation did not finish within 30 minutes"
          exit 124
        }
      }
    '
  ) &
  activate_child "$!"
  wait_for_active_child "$ACTIVE_CHILD_PID"
}

amika_ssh_secret_line() {
  local sandbox="$1"
  local secret="$2"
  local attempt=1
  local delivered
  local delivery_marker
  local status
  local remote_command
  shift 2
  if [ "${1:-}" = "--" ]; then
    shift
  fi
  command -v expect >/dev/null 2>&1 || fail "expect is required for Amika credential transfer"
  remote_command="$(encode_remote_argv "$@")"
  ACTIVE_DELIVERY_DIR="$(mktemp -d "${TMPDIR:-/tmp}/amika-secret-delivery.XXXXXX")" || \
    fail "could not create credential delivery state directory"
  delivery_marker="$ACTIVE_DELIVERY_DIR/delivered"

  while [ "$attempt" -le 3 ]; do
    prepare_child_launch
    (
      trap - HUP INT TERM
      AMIKA_EXPECT_COMMON="$EXPECT_TRANSPORT_PROCS" \
        AMIKA_SANDBOX_NAME="$sandbox" AMIKA_REMOTE_COMMAND="$remote_command" \
        AMIKA_DELIVERY_MARKER="$delivery_marker" \
        exec expect -c '
        eval $env(AMIKA_EXPECT_COMMON)
        initialize_transport
        set timeout 30
        if {[gets stdin secret] < 0} {
          restore_terminal
          puts stderr "credential input closed before a secret was read"
          exit 65
        }
        spawn -noecho amika sandbox ssh -t $env(AMIKA_SANDBOX_NAME) -- $env(AMIKA_REMOTE_COMMAND)
        set child_active 1
        expect {
          "__LOCALITY_STDIN_READY__" {}
          eof {
            catch {wait_for_child}
            restore_terminal
            puts stderr "Amika SSH closed before requesting credential input"
            exit 75
          }
          timeout {
            reap_child SIGTERM
            restore_terminal
            puts stderr "Amika SSH did not request credential input within 30 seconds"
            exit 75
          }
        }
        send -- "$secret\n"
        if {[catch {
          set marker [open $env(AMIKA_DELIVERY_MARKER) {WRONLY CREAT EXCL}]
          close $marker
        }]} {
          set secret ""
          reap_child SIGTERM
          restore_terminal
          puts stderr "could not record credential delivery; it was not retried"
          exit 125
        }
        set secret ""
        set timeout 1800
        expect {
          eof {
            set result [wait_for_child]
            restore_terminal
            exit [child_status $result]
          }
          timeout {
            set secret ""
            reap_child SIGTERM
            restore_terminal
            puts stderr "Amika credential operation did not finish within 30 minutes; it was not retried"
            exit 124
          }
        }
      '
    ) <<<"$secret" &
    activate_child "$!"
    if wait_for_active_child "$ACTIVE_CHILD_PID"; then
      secret=""
      cleanup_delivery_marker
      return 0
    else
      status=$?
    fi

    delivered=false
    if [ -f "$delivery_marker" ]; then
      delivered=true
    fi
    if [ "$status" -ne 75 ] || [ "$delivered" = true ] || [ "$attempt" -eq 3 ]; then
      secret=""
      cleanup_delivery_marker
      return "$status"
    fi
    printf 'Amika credential transport closed before secret delivery; retrying (%s/3)...\n' "$attempt" >&2
    attempt=$((attempt + 1))
    sleep 1
  done
}

create_amika_sandbox() {
  local sandbox="$1"

  prepare_child_launch
  (
    trap - HUP INT TERM
    cd "$REPO_ROOT"
    exec amika sandbox create \
      --remote \
      --name "$sandbox" \
      --no-git \
      --yes
  ) >/dev/null &
  activate_child "$!"
  wait_for_active_child "$ACTIVE_CHILD_PID"
}

API_URL=""
SANDBOX_NAME="locality-snapshot-$(date -u +%Y%m%d-%H%M%S)"
REUSE_SANDBOX=false
REMOTE_ROOT="/home/amika/locality-snapshot"
LOC_RELEASE_VERSION="0.3.7"
LOC_RELEASE_DEB_SHA256="692b05460839ba44b85cd1e6b3b6969ad4a3f62f3e81f420c4651159ad7ef195"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
AZURE_OPENAI_BASE_URL="${AZURE_OPENAI_BASE_URL:-https://aseem-mp32maxp-eastus2.openai.azure.com/openai/v1}"
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
[ "$REUSE_SANDBOX" != true ] || \
  fail "--reuse is refused because an existing sandbox is not a trusted credential boundary; choose a fresh --name"
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
  TERMINAL_STATE="$(stty -g < /dev/tty)" || fail "could not read terminal state"
  TERMINAL_STATE_ACTIVE=true
  stty -echo < /dev/tty || fail "could not disable terminal echo"
  IFS= read -r PROFILE_KEY || {
    restore_terminal_state
    fail "read the Workspace Profile key from standard input"
  }
  restore_terminal_state
  printf '\n'
else
  IFS= read -r PROFILE_KEY || fail "read the Workspace Profile key from standard input"
fi
export -n PROFILE_KEY
[ "${#PROFILE_KEY}" -eq 64 ] || fail "Workspace Profile key must be 64 lowercase hexadecimal characters"
case "$PROFILE_KEY" in
  *[!0-9a-f]*) fail "Workspace Profile key must be 64 lowercase hexadecimal characters" ;;
esac

printf 'Creating fresh Amika sandbox %s (existing sandboxes are never reused or replaced)...\n' "$SANDBOX_NAME"
create_amika_sandbox "$SANDBOX_NAME" || \
  fail "could not create fresh sandbox ${SANDBOX_NAME}; no credentials were transferred (choose a new name or explicitly delete the old sandbox)"

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
