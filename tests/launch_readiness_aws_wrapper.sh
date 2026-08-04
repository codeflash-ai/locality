#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WRAPPER="${ROOT}/experiment/locality-mcp-comparison/run-repeated-aws.sh"

fail() {
  printf 'launch readiness AWS wrapper test: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local path="$1"
  local needle="$2"
  grep -F -q -- "$needle" "$path" || fail "missing ${needle} in ${path}"
}

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-launch-readiness-aws-wrapper-test.XXXXXX")"
run_id="aws-wrapper-default-output"
repo_default_out="$ROOT/experiment/launch-readiness-aws/$run_id"
repo_old_default_out="$ROOT/target/launch-readiness-aws/$run_id"
repo_wrong_plural_out="$ROOT/experiments/launch-readiness-aws/$run_id"
cleanup() {
  rm -rf "$tmp_root" "$repo_default_out" "$repo_old_default_out" "$repo_wrong_plural_out"
}
trap cleanup EXIT

rm -rf "$repo_default_out" "$repo_old_default_out" "$repo_wrong_plural_out"

fake_bin="${tmp_root}/bin"
fake_aws_log="${tmp_root}/aws.log"
fake_ssh_log="${tmp_root}/ssh.log"
key_file="${tmp_root}/aws_key.pem"
mkdir -p "$fake_bin"
touch "$key_file"
chmod 600 "$key_file"

cat > "${fake_bin}/aws" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

printf 'aws' >> "${FAKE_AWS_LOG:?}"
for arg in "$@"; do
  printf ' %q' "$arg" >> "$FAKE_AWS_LOG"
done
printf '\n' >> "$FAKE_AWS_LOG"

if [ "${1:-}" != "ec2" ]; then
  printf 'unexpected fake aws command: %s\n' "$*" >&2
  exit 2
fi
shift

command="${1:-}"
shift || true
args=" $* "

case "$command" in
  describe-instances)
    case "$args" in
      *"tag:Name,Values=locality-benchmark-vm"*) printf 'i-source-locality\n' ;;
      *"tag:Name,Values=mcp-benchmark-vm"*) printf 'i-source-mcp\n' ;;
      *"InstanceType"*) printf 't3.large\n' ;;
      *"SubnetId"*) printf 'subnet-test\n' ;;
      *"KeyName"*) printf 'test-key\n' ;;
      *"SecurityGroups[].GroupId"*) printf 'sg-test\n' ;;
      *"IamInstanceProfile.Arn"*) printf 'None\n' ;;
      *"PublicIpAddress"*)
        case "$args" in
          *"i-locality-trial-1"*) printf '203.0.113.10\n' ;;
          *"i-mcp-trial-1"*) printf '203.0.113.20\n' ;;
          *) printf 'unexpected public ip lookup: %s\n' "$args" >&2; exit 2 ;;
        esac
        ;;
      *) printf 'unexpected describe-instances args: %s\n' "$args" >&2; exit 2 ;;
    esac
    ;;
  describe-images)
    case "$args" in
      *"LaunchReadinessRole,Values=locality"*) printf 'ami-locality available 2026-01-01T00:00:00.000Z lr-locality\n' ;;
      *"LaunchReadinessRole,Values=mcp"*) printf 'ami-mcp available 2026-01-01T00:00:00.000Z lr-mcp\n' ;;
      *) printf 'unexpected describe-images args: %s\n' "$args" >&2; exit 2 ;;
    esac
    ;;
  wait)
    ;;
  run-instances)
    case "$args" in
      *"LaunchReadinessRole,Value=locality"*) printf 'i-locality-trial-1\n' ;;
      *"LaunchReadinessRole,Value=mcp"*) printf 'i-mcp-trial-1\n' ;;
      *) printf 'unexpected run-instances args: %s\n' "$args" >&2; exit 2 ;;
    esac
    ;;
  terminate-instances)
    ;;
  *)
    printf 'unexpected fake aws ec2 command: %s\n' "$command" >&2
    exit 2
    ;;
esac
SH
chmod +x "${fake_bin}/aws"

cat > "${fake_bin}/ssh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

printf 'ssh' >> "${FAKE_SSH_LOG:?}"
for arg in "$@"; do
  printf ' %q' "$arg" >> "$FAKE_SSH_LOG"
done
printf '\n' >> "$FAKE_SSH_LOG"
SH
chmod +x "${fake_bin}/ssh"

PATH="${fake_bin}:$PATH" \
  FAKE_AWS_LOG="$fake_aws_log" \
  FAKE_SSH_LOG="$fake_ssh_log" \
  RUN_ID="$run_id" \
  TRIALS=1 \
  AWS_SSH_KEY_FILE="$key_file" \
  AWS_POLL_SECONDS=0 \
  SYNC_ARTIFACTS=0 \
  SYNC_LOCAL_EXPERIMENT=0 \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  CODEX_EXEC_TIMEOUT_SECONDS=12 \
  "$WRAPPER" --scenario scenario2 >/dev/null

test -f "$repo_default_out/aws-run.env" || fail "default BASE_OUT_DIR should write to experiment"
test ! -e "$repo_old_default_out/aws-run.env" || fail "default BASE_OUT_DIR should not write to target"
test ! -e "$repo_wrong_plural_out/aws-run.env" || fail "default BASE_OUT_DIR should not write to experiments"
assert_contains "$repo_default_out/aws-run.env" "base_out_dir=$repo_default_out"
assert_contains "$repo_default_out/trial-1/run.env" "locality_remote_out_dir=/home/ubuntu/workspace/locality-launch-readiness-$run_id-trial-1/experiment/launch-readiness-$run_id-trial-1-locality"
assert_contains "$repo_default_out/trial-1/run.env" "mcp_remote_out_dir=/home/ubuntu/workspace/locality-launch-readiness-$run_id-trial-1/experiment/launch-readiness-$run_id-trial-1-mcp"

printf 'launch readiness AWS wrapper tests passed\n'
