#!/usr/bin/env bash
# Deletes what the reference benchmark left in the account: every run stack
# (wye-bench-run-*), and with --all also the results bucket's contents and
# the bootstrap stack (bucket, instance role, budget), which leaves the
# account as it was before bootstrap.yaml.
#
#   AWS_PROFILE=<profile> bench/reference/cleanup.sh [--all]
#
# Region: AWS_REGION, default us-east-1, as for run.sh.
set -euo pipefail

all=false
case ${1:-} in
  '') ;;
  --all) all=true ;;
  *) sed -n '7p' "$0" | sed 's/^# //' >&2; exit 2 ;;
esac
export AWS_REGION=${AWS_REGION:-us-east-1} AWS_PAGER=
bootstrap=wye-bench-bootstrap

account=$(aws sts get-caller-identity --query Account --output text)
echo "==> account $account, region $AWS_REGION, profile ${AWS_PROFILE:-(none)}"

# delete <stack>...: deletes the stacks, then waits for each to be gone.
delete() {
  local stack failed=0
  for stack in "$@"; do
    echo "    deleting $stack"
    aws cloudformation delete-stack --stack-name "$stack"
  done
  for stack in "$@"; do
    aws cloudformation wait stack-delete-complete --stack-name "$stack" || {
      echo "error: $stack was not deleted; see its events in the CloudFormation console" >&2
      failed=1
    }
  done
  return "$failed"
}

status=0
runs=$(aws cloudformation list-stacks \
  --query "StackSummaries[?starts_with(StackName, 'wye-bench-run-') && StackStatus != 'DELETE_COMPLETE'].StackName" \
  --output text)
if [[ -n $runs && $runs != None ]]; then
  echo "==> run stacks"
  # Word splitting on purpose: one stack name per word.
  # shellcheck disable=SC2086
  delete $runs || status=1
else
  echo "==> no run stacks"
fi

if $all; then
  bucket=$(aws cloudformation describe-stacks --stack-name "$bootstrap" \
    --query "Stacks[0].Outputs[?OutputKey=='ResultsBucket'].OutputValue" \
    --output text 2>/dev/null) || bucket=''
  if [[ -n $bucket && $bucket != None ]]; then
    echo "==> emptying $bucket"
    # Versioning is off, so this removes every object.
    aws s3 rm --quiet --recursive "s3://$bucket"
    echo "==> bootstrap stack"
    delete "$bootstrap" || status=1
  else
    echo "==> no $bootstrap stack"
  fi
fi

# Instances outside any stack would be a leak this script cannot see by
# stack; report any still tagged for the benchmark.
left=$(aws ec2 describe-instances \
  --filters Name=tag:Project,Values=wye-bench \
  Name=instance-state-name,Values=pending,running,stopping,stopped \
  --query 'Reservations[].Instances[].InstanceId' --output text)
if [[ -n $left ]]; then
  echo "error: instances tagged Project=wye-bench still exist: $left" >&2
  status=1
fi
exit "$status"
