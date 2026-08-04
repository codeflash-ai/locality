#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-repeated-aws.sh [benchmark args...]

Reuses prior benchmark AMIs when available, creates missing AMIs from the source
EC2 instances, launches benchmark instances from those AMIs, then runs paired
Locality/MCP trials concurrently.

Defaults:
  AWS_LOCALITY_SOURCE_NAME=locality-benchmark-vm
  AWS_MCP_SOURCE_NAME=mcp-benchmark-vm
  TRIALS=3
  CODEX_MODEL=gpt-5.6-sol
  CODEX_REASONING_EFFORT=low
  AWS_SSH_KEY_FILE=$HOME/.ssh/aws_key.pem
  AWS_AMI_NO_REBOOT=1
  AWS_REUSE_AMIS=1
  AWS_TERMINATE_SUCCESS_INSTANCES=1
  AWS_DELETE_AMIS_ON_SUCCESS=0

Optional AMI overrides:
  AWS_LOCALITY_AMI_ID=<ami-id>
  AWS_MCP_AMI_ID=<ami-id>

Requires AZURE_OPENAI_API_KEY in the local environment or login shell; the
comparison runner forwards it to the benchmark instances at run time.

Any remaining arguments are passed to run-agent-comparison.sh, for example:
  --scenario scenario6
EOF
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
  usage
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-aws}"
TRIALS="${TRIALS:-3}"
AWS_LOCALITY_SOURCE_NAME="${AWS_LOCALITY_SOURCE_NAME:-locality-benchmark-vm}"
AWS_MCP_SOURCE_NAME="${AWS_MCP_SOURCE_NAME:-mcp-benchmark-vm}"
AWS_SSH_KEY_FILE="${AWS_SSH_KEY_FILE:-$HOME/.ssh/aws_key.pem}"
AWS_AMI_NO_REBOOT="${AWS_AMI_NO_REBOOT:-1}"
AWS_REUSE_AMIS="${AWS_REUSE_AMIS:-1}"
AWS_LOCALITY_AMI_ID="${AWS_LOCALITY_AMI_ID:-}"
AWS_MCP_AMI_ID="${AWS_MCP_AMI_ID:-}"
AWS_TERMINATE_SUCCESS_INSTANCES="${AWS_TERMINATE_SUCCESS_INSTANCES:-1}"
AWS_DELETE_AMIS_ON_SUCCESS="${AWS_DELETE_AMIS_ON_SUCCESS:-0}"
BASE_OUT_DIR="${BASE_OUT_DIR:-$REPO_ROOT/experiment/launch-readiness-aws/$RUN_ID}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
CODEX_EXEC_TIMEOUT_SECONDS="${CODEX_EXEC_TIMEOUT_SECONDS:-900}"
REMOTE_SOURCE_REPO="${REMOTE_SOURCE_REPO:-/home/ubuntu/workspace/locality}"
REMOTE_WORKTREE_ROOT="${REMOTE_WORKTREE_ROOT:-/home/ubuntu/workspace}"
REMOTE_LOC_BIN="${REMOTE_LOC_BIN:-/usr/bin/loc}"
BENCHMARK_REF="${BENCHMARK_REF:-origin/main}"
SYNC_LOCAL_EXPERIMENT="${SYNC_LOCAL_EXPERIMENT:-1}"
SYNC_ARTIFACTS="${SYNC_ARTIFACTS:-1}"
AWS_POLL_SECONDS="${AWS_POLL_SECONDS:-10}"

if ! command -v aws >/dev/null 2>&1; then
  echo "aws is not available on PATH" >&2
  exit 127
fi
if ! command -v ssh >/dev/null 2>&1; then
  echo "ssh is not available on PATH" >&2
  exit 127
fi
if [ ! -r "$AWS_SSH_KEY_FILE" ]; then
  echo "AWS_SSH_KEY_FILE is not readable: $AWS_SSH_KEY_FILE" >&2
  exit 2
fi
case "$TRIALS" in
  ''|*[!0-9]*) echo "TRIALS must be a positive integer" >&2; exit 2 ;;
esac
if [ "$TRIALS" -lt 1 ]; then
  echo "TRIALS must be at least 1" >&2
  exit 2
fi

mkdir -p "$BASE_OUT_DIR"
export AWS_PAGER=""

aws_ec2() {
  aws ec2 "$@"
}

find_instance_by_name() {
  local name="$1"
  local ids
  local count

  ids="$(
    aws_ec2 describe-instances \
      --filters "Name=tag:Name,Values=$name" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
      --query 'Reservations[].Instances[].InstanceId' \
      --output text
  )"
  count="$(printf '%s\n' "$ids" | wc -w | tr -d ' ')"
  if [ "$count" != "1" ]; then
    echo "expected exactly one EC2 instance named $name, found $count: $ids" >&2
    exit 2
  fi
  printf '%s\n' "$ids"
}

instance_field() {
  local instance_id="$1"
  local query="$2"
  aws_ec2 describe-instances \
    --instance-ids "$instance_id" \
    --query "Reservations[0].Instances[0].$query" \
    --output text
}

tag_spec() {
  local resource_type="$1"
  local name="$2"
  local role="$3"
  local trial="$4"
  local source_instance="$5"

  printf 'ResourceType=%s,Tags=[{Key=Name,Value=%s},{Key=LaunchReadinessRun,Value=%s},{Key=LaunchReadinessRole,Value=%s},{Key=LaunchReadinessTrial,Value=%s},{Key=SourceInstance,Value=%s}]' \
    "$resource_type" "$name" "$RUN_ID" "$role" "$trial" "$source_instance"
}

create_ami() {
  local role="$1"
  local source_instance="$2"
  local ami_name="lr-$RUN_ID-$role"

  echo "Creating $role AMI from $source_instance: $ami_name" >&2
  if [ "$AWS_AMI_NO_REBOOT" = "1" ]; then
    aws_ec2 create-image \
      --instance-id "$source_instance" \
      --name "$ami_name" \
      --description "Launch readiness $role AMI for $RUN_ID from $source_instance" \
      --no-reboot \
      --tag-specifications \
        "$(tag_spec image "$ami_name" "$role" source "$source_instance")" \
        "$(tag_spec snapshot "$ami_name" "$role" source "$source_instance")" \
      --query ImageId \
      --output text
  else
    aws_ec2 create-image \
      --instance-id "$source_instance" \
      --name "$ami_name" \
      --description "Launch readiness $role AMI for $RUN_ID from $source_instance" \
      --tag-specifications \
        "$(tag_spec image "$ami_name" "$role" source "$source_instance")" \
        "$(tag_spec snapshot "$ami_name" "$role" source "$source_instance")" \
      --query ImageId \
      --output text
  fi
}

find_reusable_ami() {
  local role="$1"
  local source_instance="$2"
  local line
  local ami_id
  local state
  local created
  local name

  line="$(
    aws_ec2 describe-images \
      --owners self \
      --filters \
        "Name=tag:LaunchReadinessRole,Values=$role" \
        "Name=tag:SourceInstance,Values=$source_instance" \
        "Name=state,Values=available,pending" \
      --query 'sort_by(Images,&CreationDate)[-1].[ImageId,State,CreationDate,Name]' \
      --output text
  )"
  read -r ami_id state created name <<< "$line"
  if [ -n "${ami_id:-}" ] && [ "$ami_id" != "None" ]; then
    printf '%s\t%s\t%s\t%s\n' "$ami_id" "$state" "$created" "$name"
  fi
}

resolve_ami() {
  local role="$1"
  local source_instance="$2"
  local explicit_ami_id="$3"
  local reusable
  local ami_id
  local state
  local created
  local name

  if [ -n "$explicit_ami_id" ]; then
    echo "Using provided $role AMI: $explicit_ami_id" >&2
    printf '%s\tprovided\n' "$explicit_ami_id"
    return 0
  fi

  if [ "$AWS_REUSE_AMIS" = "1" ]; then
    reusable="$(find_reusable_ami "$role" "$source_instance")"
    if [ -n "$reusable" ]; then
      IFS=$'\t' read -r ami_id state created name <<< "$reusable"
      echo "Reusing $role AMI from previous experiment: $ami_id state=$state created=$created name=$name" >&2
      printf '%s\treused\n' "$ami_id"
      return 0
    fi
  fi

  ami_id="$(create_ami "$role" "$source_instance")"
  printf '%s\tcreated\n' "$ami_id"
}

source_security_group_ids() {
  local source_instance="$1"
  aws_ec2 describe-instances \
    --instance-ids "$source_instance" \
    --query 'Reservations[0].Instances[0].SecurityGroups[].GroupId' \
    --output text
}

launch_instance() {
  local role="$1"
  local trial="$2"
  local ami_id="$3"
  local source_instance="$4"
  local name="lr-$RUN_ID-$role-trial-$trial"
  local instance_type
  local subnet_id
  local key_name
  local sg_ids
  local iam_profile_arn
  local sg_args=()

  instance_type="${AWS_INSTANCE_TYPE:-$(instance_field "$source_instance" InstanceType)}"
  subnet_id="${AWS_SUBNET_ID:-$(instance_field "$source_instance" SubnetId)}"
  key_name="${AWS_KEY_NAME:-$(instance_field "$source_instance" KeyName)}"
  sg_ids="${AWS_SECURITY_GROUP_IDS:-$(source_security_group_ids "$source_instance")}"
  iam_profile_arn="$(instance_field "$source_instance" 'IamInstanceProfile.Arn')"
  read -r -a sg_args <<< "$sg_ids"

  echo "Launching $role trial $trial from $ami_id" >&2
  if [ -n "$iam_profile_arn" ] && [ "$iam_profile_arn" != "None" ]; then
    aws_ec2 run-instances \
      --image-id "$ami_id" \
      --count 1 \
      --instance-type "$instance_type" \
      --key-name "$key_name" \
      --subnet-id "$subnet_id" \
      --security-group-ids "${sg_args[@]}" \
      --iam-instance-profile "Arn=$iam_profile_arn" \
      --tag-specifications \
        "$(tag_spec instance "$name" "$role" "$trial" "$source_instance")" \
        "$(tag_spec volume "$name" "$role" "$trial" "$source_instance")" \
      --query 'Instances[0].InstanceId' \
      --output text
  else
    aws_ec2 run-instances \
      --image-id "$ami_id" \
      --count 1 \
      --instance-type "$instance_type" \
      --key-name "$key_name" \
      --subnet-id "$subnet_id" \
      --security-group-ids "${sg_args[@]}" \
      --tag-specifications \
        "$(tag_spec instance "$name" "$role" "$trial" "$source_instance")" \
        "$(tag_spec volume "$name" "$role" "$trial" "$source_instance")" \
      --query 'Instances[0].InstanceId' \
      --output text
  fi
}

wait_for_public_ip() {
  local instance_id="$1"
  local public_ip=""
  local attempt=1

  while [ "$attempt" -le 60 ]; do
    public_ip="$(instance_field "$instance_id" PublicIpAddress)"
    if [ -n "$public_ip" ] && [ "$public_ip" != "None" ]; then
      printf '%s\n' "$public_ip"
      return 0
    fi
    sleep "$AWS_POLL_SECONDS"
    attempt=$((attempt + 1))
  done
  echo "instance $instance_id did not receive a public IP" >&2
  return 1
}

wait_for_ssh() {
  local target="$1"
  local attempt=1

  while [ "$attempt" -le 60 ]; do
    if ssh -i "$AWS_SSH_KEY_FILE" -o StrictHostKeyChecking=accept-new -o BatchMode=yes -o ConnectTimeout=8 "$target" true >/dev/null 2>&1; then
      return 0
    fi
    sleep "$AWS_POLL_SECONDS"
    attempt=$((attempt + 1))
  done
  echo "SSH did not become ready for $target" >&2
  return 1
}

delete_ami_and_snapshots() {
  local ami_id="$1"
  local snapshots
  local snapshot_id

  snapshots="$(
    aws_ec2 describe-images \
      --image-ids "$ami_id" \
      --query 'Images[0].BlockDeviceMappings[].Ebs.SnapshotId' \
      --output text 2>/dev/null || true
  )"
  echo "Deregistering AMI $ami_id"
  aws_ec2 deregister-image --image-id "$ami_id" >/dev/null || true
  for snapshot_id in $snapshots; do
    if [ -n "$snapshot_id" ] && [ "$snapshot_id" != "None" ]; then
      echo "Deleting AMI snapshot $snapshot_id"
      aws_ec2 delete-snapshot --snapshot-id "$snapshot_id" >/dev/null || true
    fi
  done
}

run_trial() {
  local trial="$1"
  local locality_instance_id="$2"
  local mcp_instance_id="$3"
  local locality_ip="$4"
  local mcp_ip="$5"
  local trial_out_dir="$BASE_OUT_DIR/trial-$trial"
  local trial_run_id="$RUN_ID-trial-$trial"
  local rc
  shift 5

  mkdir -p "$trial_out_dir"
  {
    printf 'trial=%s\n' "$trial"
    printf 'run_id=%s\n' "$trial_run_id"
    printf 'locality_instance_id=%s\n' "$locality_instance_id"
    printf 'mcp_instance_id=%s\n' "$mcp_instance_id"
    printf 'locality_ssh_target=ubuntu@%s\n' "$locality_ip"
    printf 'mcp_ssh_target=ubuntu@%s\n' "$mcp_ip"
  } > "$trial_out_dir/aws-trial.env"

  set +e
  RUN_ID="$trial_run_id" \
  LOCAL_OUT_DIR="$trial_out_dir" \
  LOCALITY_SANDBOX="aws-locality-trial-$trial" \
  MCP_SANDBOX="aws-mcp-trial-$trial" \
  REMOTE_PROVIDER=ssh \
  LOCALITY_SSH_TARGET="ubuntu@$locality_ip" \
  MCP_SSH_TARGET="ubuntu@$mcp_ip" \
  SSH_OPTIONS="-i $AWS_SSH_KEY_FILE -o StrictHostKeyChecking=accept-new -o BatchMode=yes -o ConnectTimeout=15" \
  REMOTE_SOURCE_REPO="$REMOTE_SOURCE_REPO" \
  REMOTE_WORKTREE_ROOT="$REMOTE_WORKTREE_ROOT" \
  REMOTE_LOC_BIN="$REMOTE_LOC_BIN" \
  BENCHMARK_REF="$BENCHMARK_REF" \
  SYNC_LOCAL_EXPERIMENT="$SYNC_LOCAL_EXPERIMENT" \
  SYNC_ARTIFACTS="$SYNC_ARTIFACTS" \
  CODEX_MODEL="$CODEX_MODEL" \
  CODEX_REASONING_EFFORT="$CODEX_REASONING_EFFORT" \
  CODEX_EXEC_TIMEOUT_SECONDS="$CODEX_EXEC_TIMEOUT_SECONDS" \
    "$SCRIPT_DIR/run-agent-comparison.sh" "$@"
  rc=$?
  set -e

  printf 'benchmark_rc=%s\n' "$rc" >> "$trial_out_dir/aws-trial.env"
  if [ "$rc" -eq 0 ] && [ "$AWS_TERMINATE_SUCCESS_INSTANCES" = "1" ]; then
    echo "Trial $trial succeeded; terminating $locality_instance_id and $mcp_instance_id"
    aws_ec2 terminate-instances --instance-ids "$locality_instance_id" "$mcp_instance_id" >/dev/null
    printf 'terminated_success_instances=1\n' >> "$trial_out_dir/aws-trial.env"
  else
    echo "Trial $trial failed or cleanup disabled; keeping $locality_instance_id and $mcp_instance_id"
    printf 'terminated_success_instances=0\n' >> "$trial_out_dir/aws-trial.env"
  fi
  return "$rc"
}

declare -a locality_instance_ids=()
declare -a mcp_instance_ids=()
declare -a locality_ips=()
declare -a mcp_ips=()
declare -a trial_pids=()

locality_source_id="$(find_instance_by_name "$AWS_LOCALITY_SOURCE_NAME")"
mcp_source_id="$(find_instance_by_name "$AWS_MCP_SOURCE_NAME")"

locality_ami_record="$(resolve_ami locality "$locality_source_id" "$AWS_LOCALITY_AMI_ID")"
mcp_ami_record="$(resolve_ami mcp "$mcp_source_id" "$AWS_MCP_AMI_ID")"
IFS=$'\t' read -r locality_ami_id locality_ami_origin <<< "$locality_ami_record"
IFS=$'\t' read -r mcp_ami_id mcp_ami_origin <<< "$mcp_ami_record"

{
  printf 'run_id=%s\n' "$RUN_ID"
  printf 'base_out_dir=%s\n' "$BASE_OUT_DIR"
  printf 'locality_source_name=%s\n' "$AWS_LOCALITY_SOURCE_NAME"
  printf 'mcp_source_name=%s\n' "$AWS_MCP_SOURCE_NAME"
  printf 'locality_source_id=%s\n' "$locality_source_id"
  printf 'mcp_source_id=%s\n' "$mcp_source_id"
  printf 'locality_ami_id=%s\n' "$locality_ami_id"
  printf 'locality_ami_origin=%s\n' "$locality_ami_origin"
  printf 'mcp_ami_id=%s\n' "$mcp_ami_id"
  printf 'mcp_ami_origin=%s\n' "$mcp_ami_origin"
  printf 'aws_reuse_amis=%s\n' "$AWS_REUSE_AMIS"
  printf 'trials=%s\n' "$TRIALS"
  printf 'codex_model=%s\n' "$CODEX_MODEL"
  printf 'codex_reasoning_effort=%s\n' "$CODEX_REASONING_EFFORT"
  printf 'benchmark_ref=%s\n' "$BENCHMARK_REF"
} > "$BASE_OUT_DIR/aws-run.env"

echo "Waiting for AMIs: $locality_ami_id $mcp_ami_id"
aws_ec2 wait image-available --image-ids "$locality_ami_id" "$mcp_ami_id"

for trial in $(seq 1 "$TRIALS"); do
  locality_instance_ids[$trial]="$(launch_instance locality "$trial" "$locality_ami_id" "$locality_source_id")"
  mcp_instance_ids[$trial]="$(launch_instance mcp "$trial" "$mcp_ami_id" "$mcp_source_id")"
  printf '%s\t%s\t%s\n' "$trial" "${locality_instance_ids[$trial]}" "${mcp_instance_ids[$trial]}" >> "$BASE_OUT_DIR/aws-instances.tsv"
done

echo "Waiting for EC2 status checks"
aws_ec2 wait instance-status-ok --instance-ids "${locality_instance_ids[@]}" "${mcp_instance_ids[@]}"

for trial in $(seq 1 "$TRIALS"); do
  locality_ips[$trial]="$(wait_for_public_ip "${locality_instance_ids[$trial]}")"
  mcp_ips[$trial]="$(wait_for_public_ip "${mcp_instance_ids[$trial]}")"
  wait_for_ssh "ubuntu@${locality_ips[$trial]}"
  wait_for_ssh "ubuntu@${mcp_ips[$trial]}"
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$trial" \
    "${locality_instance_ids[$trial]}" \
    "${locality_ips[$trial]}" \
    "${mcp_instance_ids[$trial]}" \
    "${mcp_ips[$trial]}" >> "$BASE_OUT_DIR/aws-ssh-targets.tsv"
done

echo "Starting $TRIALS paired benchmark trials"
for trial in $(seq 1 "$TRIALS"); do
  run_trial \
    "$trial" \
    "${locality_instance_ids[$trial]}" \
    "${mcp_instance_ids[$trial]}" \
    "${locality_ips[$trial]}" \
    "${mcp_ips[$trial]}" \
    "$@" &
  trial_pids[$trial]=$!
done

overall_rc=0
for trial in $(seq 1 "$TRIALS"); do
  if ! wait "${trial_pids[$trial]}"; then
    overall_rc=1
  fi
done

if [ "$SYNC_ARTIFACTS" = "1" ]; then
  if python3 "$SCRIPT_DIR/scripts/token-usage-charts.py" "$BASE_OUT_DIR" "$BASE_OUT_DIR/token-usage" >/dev/null; then
    echo "Aggregate token usage charts: $BASE_OUT_DIR/token-usage"
  else
    echo "Failed to generate aggregate token usage charts for $BASE_OUT_DIR" >&2
    if [ "$overall_rc" -eq 0 ]; then
      overall_rc=1
    fi
  fi
fi

if [ "$overall_rc" -eq 0 ]; then
  echo "All AWS benchmark trials succeeded"
  if [ "$AWS_DELETE_AMIS_ON_SUCCESS" = "1" ]; then
    deleted_created_amis=0
    if [ "$locality_ami_origin" = "created" ]; then
      delete_ami_and_snapshots "$locality_ami_id"
      deleted_created_amis=1
    else
      echo "Keeping reused/provided locality AMI $locality_ami_id"
    fi
    if [ "$mcp_ami_origin" = "created" ]; then
      delete_ami_and_snapshots "$mcp_ami_id"
      deleted_created_amis=1
    else
      echo "Keeping reused/provided MCP AMI $mcp_ami_id"
    fi
    printf 'deleted_success_amis=%s\n' "$deleted_created_amis" >> "$BASE_OUT_DIR/aws-run.env"
  else
    printf 'deleted_success_amis=0\n' >> "$BASE_OUT_DIR/aws-run.env"
  fi
else
  echo "One or more AWS benchmark trials failed; keeping failed instances and AMIs for debugging" >&2
  printf 'deleted_success_amis=0\n' >> "$BASE_OUT_DIR/aws-run.env"
fi

echo "AWS benchmark output: $BASE_OUT_DIR"
exit "$overall_rc"
