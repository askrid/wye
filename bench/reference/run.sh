#!/usr/bin/env bash
# The reference benchmark: one commit, built and benchmarked against the
# pinned competitors on a fresh cloud instance, for the numbers Wye publishes
# (docs/benchmarks.md, "Reference environment"). Started only by hand.
#
#   AWS_PROFILE=<profile> bench/reference/run.sh [--commit <sha|tag>] [--instance-type T] [--runs N]
#
# Needs the wye-bench-bootstrap stack (bootstrap.yaml) in the region
# (AWS_REGION, default us-east-1), and the commit pushed: the instance clones
# the public repository and runs that commit's bench/reference/remote.sh.
# Each run creates the stack wye-bench-run-<id> (run.yaml), runs the benchmark
# through SSM Run Command, downloads the results into
# bench/reference/results/<date>-<commit>/run-<n>/, and deletes the stack on
# exit, also after a failure or Ctrl-C. --runs N repeats on N fresh stacks,
# one after another, to measure the variance between hosts.
set -euo pipefail

usage() {
  sed -n '6p' "$0" | sed 's/^# //' >&2
  exit 2
}
commit=HEAD instance_type=c7i.4xlarge runs=1
while (( $# )); do
  case $1 in
    --commit) (( $# >= 2 )) || usage; commit=$2; shift 2 ;;
    --instance-type) (( $# >= 2 )) || usage; instance_type=$2; shift 2 ;;
    --runs) (( $# >= 2 )) || usage; runs=$2; shift 2 ;;
    *) usage ;;
  esac
done
case $instance_type in
  c7i.2xlarge | c7i.4xlarge | c7i.8xlarge) ;;
  *) echo "instance type: c7i.2xlarge, c7i.4xlarge, or c7i.8xlarge" >&2; exit 2 ;;
esac
[[ $runs =~ ^[1-9][0-9]*$ ]] || usage
cd "$(dirname "$0")/../.."

export AWS_REGION=${AWS_REGION:-us-east-1} AWS_PAGER=
repo_url=https://github.com/askrid/wye
bootstrap=wye-bench-bootstrap
ami_parameter=/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id
# The instance powers off and terminates this long after boot, whatever
# happens here; the benchmark command must end before.
lifetime_minutes=180
command_minutes=$(( lifetime_minutes - 15 ))

die() { echo "error: $*" >&2; exit 1; }

sha=$(git rev-parse --verify --quiet "$commit^{commit}") || die "unknown commit: $commit"
[[ -n $(git branch -r --contains "$sha" 2>/dev/null) ]] ||
  die "$sha is on no remote branch: push it first (the instance clones $repo_url)"
short=$(git rev-parse --short=7 "$sha")

account=$(aws sts get-caller-identity --query Account --output text)
echo "==> account $account, region $AWS_REGION, profile ${AWS_PROFILE:-(none)}"
stack_output() {
  aws cloudformation describe-stacks --stack-name "$1" \
    --query "Stacks[0].Outputs[?OutputKey=='$2'].OutputValue" --output text
}
bucket=$(stack_output "$bootstrap" ResultsBucket 2>/dev/null) || bucket=''
[[ -n $bucket && $bucket != None ]] ||
  die "no $bootstrap stack here: deploy bench/reference/bootstrap.yaml first"
profile=$(stack_output "$bootstrap" InstanceProfile)

ami=$(aws ssm get-parameter --name "$ami_parameter" --query Parameter.Value --output text)
vpc=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true \
  --query 'Vpcs[0].VpcId' --output text)
[[ $vpc == vpc-* ]] || die "no default VPC in $AWS_REGION"
zones=$(aws ec2 describe-instance-type-offerings --location-type availability-zone \
  --filters "Name=instance-type,Values=$instance_type" \
  --query 'InstanceTypeOfferings[].Location' --output text | tr -s ' \t' ',,')
# The first zone, by name, that offers the type: the same zone every run.
subnet='' zone=''
read -r subnet zone < <(aws ec2 describe-subnets \
  --filters "Name=vpc-id,Values=$vpc" Name=default-for-az,Values=true \
  "Name=availability-zone,Values=$zones" \
  --query 'sort_by(Subnets, &AvailabilityZone)[0].[SubnetId, AvailabilityZone]' --output text) ||
  true
[[ $subnet == subnet-* ]] || die "no default subnet offers $instance_type"
echo "    $short on $instance_type in $zone, $ami ($ami_parameter)"

results=bench/reference/results/$(date -u +%Y-%m-%d)-$short
current_stack='' params=$(mktemp)
# Deletes the current run's stack and waits until it is gone.
delete_stack() {
  [[ -n $current_stack ]] || return 0
  echo "==> deleting $current_stack"
  if aws cloudformation delete-stack --stack-name "$current_stack" &&
    aws cloudformation wait stack-delete-complete --stack-name "$current_stack"; then
    current_stack=''
    return 0
  fi
  echo "error: could not delete $current_stack; run bench/reference/cleanup.sh" >&2
  return 1
}
# On every exit, also after a failure or an interrupt, so no instance
# outlives this script; the instance's lifetime limit covers this script
# dying unannounced.
on_exit() {
  local status=$?
  rm -f "$params"
  delete_stack || status=1
  exit "$status"
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# wait_online <instance>: until its SSM agent has registered.
wait_online() {
  local i ping
  for i in $(seq 1 60); do
    ping=$(aws ssm describe-instance-information --filters "Key=InstanceIds,Values=$1" \
      --query 'InstanceInformationList[0].PingStatus' --output text)
    [[ $ping == Online ]] && return 0
    sleep 10
  done
  die "$1 did not register with SSM in 10 minutes"
}

# run_one <dir>: one run on a fresh stack, its results in <dir>.
run_one() {
  local dir=$1 id stack instance command_id status start=$SECONDS
  id=$(date -u +%Y%m%d-%H%M%S)-$short
  stack=wye-bench-run-$id
  echo "==> $stack"
  current_stack=$stack
  aws cloudformation create-stack --stack-name "$stack" \
    --template-body file://bench/reference/run.yaml --on-failure DELETE \
    --tags Key=Project,Value=wye-bench "Key=Run,Value=$id" \
    --parameters "ParameterKey=RunId,ParameterValue=$id" \
    "ParameterKey=Commit,ParameterValue=$sha" \
    "ParameterKey=InstanceType,ParameterValue=$instance_type" \
    "ParameterKey=ImageId,ParameterValue=$ami" \
    "ParameterKey=InstanceProfile,ParameterValue=$profile" \
    "ParameterKey=VpcId,ParameterValue=$vpc" \
    "ParameterKey=SubnetId,ParameterValue=$subnet" \
    "ParameterKey=MaxLifetimeMinutes,ParameterValue=$lifetime_minutes" >/dev/null
  aws cloudformation wait stack-create-complete --stack-name "$stack"
  instance=$(stack_output "$stack" InstanceId)
  echo "    $instance up after $((SECONDS - start))s; waiting for SSM"
  wait_online "$instance"

  # The instance clones the commit and runs its own remote.sh. Run by sh;
  # no double quotes or backslashes, so each line is a JSON string as is.
  local lines=(
    "set -eu"
    "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    "export DEBIAN_FRONTEND=noninteractive HOME=/root"
    "command -v git >/dev/null || { apt-get -q -y -o DPkg::Lock::Timeout=600 update && apt-get -q -y -o DPkg::Lock::Timeout=600 install git; }"
    "rm -rf /opt/wye-bench && mkdir -p /opt/wye-bench/repo && cd /opt/wye-bench/repo"
    "git init -q && git fetch -q --depth 1 $repo_url $sha && git checkout -q --detach FETCH_HEAD"
    "exec bash bench/reference/remote.sh $sha $id $bucket"
  )
  local line json=''
  for line in "${lines[@]}"; do json="$json${json:+,}\"$line\""; done
  printf '{"commands":[%s],"executionTimeout":["%d"]}\n' "$json" $((command_minutes * 60)) > "$params"
  command_id=$(aws ssm send-command --instance-ids "$instance" \
    --document-name AWS-RunShellScript --parameters "file://$params" \
    --timeout-seconds 600 --comment "wye-bench $id" \
    --output-s3-bucket-name "$bucket" --output-s3-key-prefix "runs/$id/ssm" \
    --query Command.CommandId --output text)
  echo "    command $command_id (output: s3://$bucket/runs/$id/)"
  local deadline=$(( SECONDS + (command_minutes + 20) * 60 ))
  while :; do
    sleep 60
    # Right after sending, the invocation may not exist yet.
    status=$(aws ssm get-command-invocation --command-id "$command_id" --instance-id "$instance" \
      --query Status --output text 2>/dev/null || echo Unknown)
    echo "    $status after $(( (SECONDS - start) / 60 )) min"
    case $status in
      Pending | InProgress | Delayed | Cancelling | Unknown) ;;
      *) break ;;
    esac
    (( SECONDS < deadline )) || die "no outcome from $command_id past its timeout"
  done

  mkdir -p "$dir"
  aws s3 cp --quiet --recursive "s3://$bucket/runs/$id/" "$dir/" ||
    echo "warning: could not download every result" >&2
  printf '{\n  "run": "%s",\n  "commit": "%s",\n  "status": "%s",\n  "region": "%s",\n  "zone": "%s",\n  "instance_type": "%s",\n  "ami": "%s",\n  "ami_parameter": "%s",\n  "lifetime_minutes": %d,\n  "minutes": %d\n}\n' \
    "$id" "$sha" "$status" "$AWS_REGION" "$zone" "$instance_type" "$ami" "$ami_parameter" \
    "$lifetime_minutes" $(( (SECONDS - start) / 60 )) > "$dir/run.json"
  [[ $status == Success ]] || die "the benchmark ended $status; logs in $dir"
  echo "    results in $dir"
}

for i in $(seq 1 "$runs"); do
  n=1
  while [[ -e $results/run-$n ]]; do n=$((n + 1)); done
  run_one "$results/run-$n"
  # Deleted before the next run starts, so runs never overlap.
  delete_stack || exit 1
done
echo "==> done: commit the results in $results"
