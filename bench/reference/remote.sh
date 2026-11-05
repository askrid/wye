#!/usr/bin/env bash
# The reference benchmark run, on the instance run.sh starts: as root, through
# SSM Run Command, from a clone of the public repository at the commit it
# benchmarks, so that commit pins the whole run (this script, the competitors'
# versions and checksums, the LiteLLM lock file).
#
#   bench/reference/remote.sh <commit> <run-id> <bucket>
#
# Records the machine, builds Wye, fetches each competitor at the version
# docs/benchmarks.md compared against (verified by SHA-256), gives the gateway
# whole physical cores and the load generator and mock the rest, runs
# `wye-bench run` with every competitor, and uploads everything to
# s3://<bucket>/runs/<run-id>/, also when a step fails.
set -euo pipefail

if (( $# != 3 )); then
  echo "usage: bench/reference/remote.sh <commit> <run-id> <bucket>" >&2
  exit 2
fi
commit=$1 run_id=$2 bucket=$3
# The SSM agent's environment is minimal.
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=/opt/wye-bench
out=$work/out

# Physical cores (with their hyperthreads) the gateway runs on: 4 vCPUs on
# any c7i. The same on every instance type, so a larger one only gives the
# load generator more room.
gateway_cores=2

# Pinned downloads: version, URL, and SHA-256. Where the project publishes a
# checksum it is the published one; Bifrost published none before v2.3.3, so
# its checksum is the one first downloaded (2026-11-05).
rustup_url=https://static.rust-lang.org/rustup/archive/1.29.3/x86_64-unknown-linux-gnu/rustup-init
rustup_sha256=b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55
# AWS publishes a PGP signature, no checksum: hashed at first download.
awscli_url=https://awscli.amazonaws.com/awscli-exe-linux-x86_64-2.37.12.zip
awscli_sha256=e6d3c672fb36ed4b12c354c39a993eee2b34da1437cafb10bac4480840cfa83f
bifrost_version=v2.3.2
bifrost_url=https://downloads.getmaxim.ai/bifrost/v2.3.2/linux/amd64/bifrost-http
bifrost_sha256=29ba0456fdad1e4eeaadec36ca6034dd6a77c335882aad91a5e20fff26673d15
# The release build of 26f5aeb, the commit docs/benchmarks.md built from
# source: release profile (fat LTO, one codegen unit), default features
# (jemalloc), musl.
agentgateway_version=v1.7.0
agentgateway_url=https://github.com/agentgateway/agentgateway/releases/download/v1.7.0/agentgateway-linux-amd64
agentgateway_sha256=1b2fc01a2de29ca113bbb6258b5cae7b1eb3c724071c09d687a449132f2f42f8
krakend_version=3.0.1
krakend_url=https://github.com/krakend/krakend-ce/releases/download/v3.0.1/krakend_3.0.1_amd64_generic-linux.tar.gz
krakend_sha256=81bdbc9fd9a66dcd7d481b87f39bc2c975e9d67165aaa2e95c8705fe0155b2e2
# Its dependencies are pinned by hash in the lock file.
litellm_version=1.105.0

mkdir -p "$out"
exec > >(tee -a "$out/remote.log") 2>&1
echo "==> wye-bench reference run $run_id: $commit"

# Instance metadata, IMDSv2 only.
token=$(curl -fsS -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 21600' \
  http://169.254.169.254/latest/api/token)
imds() {
  curl -fsS -H "X-aws-ec2-metadata-token: $token" "http://169.254.169.254/latest/meta-data/$1"
}
AWS_DEFAULT_REGION=$(imds placement/region)
export AWS_DEFAULT_REGION

# Whatever happens, what the run produced goes to the bucket (once the AWS
# CLI is installed; before that, only the SSM output records the failure).
upload() {
  local status=$?
  echo "==> exit $status; uploading results"
  if command -v aws >/dev/null; then
    aws s3 cp --quiet --recursive "$out" "s3://$bucket/runs/$run_id/" ||
      echo "upload failed" >&2
  fi
  exit "$status"
}
trap upload EXIT

step() { echo "==> $*"; }

# fetch <url> <sha256> <file>: downloads and verifies, or fails.
fetch() {
  curl -fsSL --retry 3 -o "$3.part" "$1"
  echo "$2  $3.part" | sha256sum --check --quiet
  mv "$3.part" "$3"
}

step "quiet the machine"
# Package updates and snap refreshes would compete for CPU during the
# benchmark (and hold the dpkg lock now).
systemctl stop apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service || true
snap refresh --hold >/dev/null || true
export DEBIAN_FRONTEND=noninteractive
apt_get() { apt-get -q -y -o DPkg::Lock::Timeout=600 "$@"; }
apt_get update >/dev/null
apt_get install --no-install-recommends build-essential pkg-config git curl ca-certificates \
  unzip python3-venv util-linux >/dev/null

step "AWS CLI"
mkdir -p "$work/dl/awscli"
fetch "$awscli_url" "$awscli_sha256" "$work/dl/awscli.zip"
unzip -q -o "$work/dl/awscli.zip" -d "$work/dl/awscli"
"$work/dl/awscli/aws/install" --update --install-dir /opt/aws-cli --bin-dir /usr/local/bin >/dev/null

step "Rust (rust-toolchain.toml)"
fetch "$rustup_url" "$rustup_sha256" "$work/dl/rustup-init"
chmod +x "$work/dl/rustup-init"
export HOME=/root CARGO_HOME=/root/.cargo RUSTUP_HOME=/root/.rustup
"$work/dl/rustup-init" -y --quiet --no-modify-path --profile minimal --default-toolchain none
export PATH=$CARGO_HOME/bin:$PATH
cd "$repo"
[[ $(git rev-parse HEAD) == "$commit" ]] || { echo "checkout is not $commit" >&2; exit 1; }
rustup toolchain install >/dev/null

step "build"
cargo build --locked --release --quiet -p wye -p wye-bench
cargo build --locked --release --quiet -p wye --features alloc-stats --target-dir target/alloc

step "competitors"
bin=$work/bin
mkdir -p "$bin"
fetch "$bifrost_url" "$bifrost_sha256" "$bin/bifrost-http"
fetch "$agentgateway_url" "$agentgateway_sha256" "$bin/agentgateway"
chmod +x "$bin/bifrost-http" "$bin/agentgateway"
fetch "$krakend_url" "$krakend_sha256" "$work/dl/krakend.tar.gz"
tar -xzf "$work/dl/krakend.tar.gz" -C "$work/dl" ./usr/bin/krakend
mv "$work/dl/usr/bin/krakend" "$bin/krakend"
python3 -m venv "$work/litellm"
"$work/litellm/bin/pip" install --quiet --disable-pip-version-check --require-hashes \
  --only-binary=:all: --no-deps -r bench/reference/litellm-requirements.txt
installed=$("$work/litellm/bin/pip" show litellm | sed -n 's/^Version: //p')
[[ $installed == "$litellm_version" ]] || { echo "litellm $installed, not $litellm_version" >&2; exit 1; }

step "split the CPUs"
# One line per physical core: its hardware threads as a cpu list ("6,14").
cores=$(cat /sys/devices/system/cpu/cpu[0-9]*/topology/thread_siblings_list | sort -u -n -t, -k1,1)
total=$(wc -l <<<"$cores")
if (( total < 2 * gateway_cores )); then
  echo "$total physical cores; the gateway's $gateway_cores need as many again for the load" >&2
  exit 1
fi
# The gateway takes the last cores, away from cpu0, which takes more of the
# interrupts; wye-bench (load generator and mock) the rest.
gateway_cpus=$(tail -n "$gateway_cores" <<<"$cores" | paste -sd, -)
client_cpus=$(head -n "$((total - gateway_cores))" <<<"$cores" | paste -sd, -)
echo "    gateway: $gateway_cpus; wye-bench: $client_cpus"

step "record the machine"
# 2,000 paced streams, each a client and an upstream connection, through
# gateways that open more of their own.
ulimit -n 65536
lscpu > "$out/lscpu.txt"
FACT_INSTANCE_TYPE=$(imds instance-type)
FACT_AMI=$(imds ami-id)
FACT_ZONE=$(imds placement/availability-zone)
FACT_INSTANCE_ID=$(imds instance-id)
FACT_KERNEL=$(uname -a)
FACT_OS=$(. /etc/os-release && echo "$PRETTY_NAME")
FACT_RUSTC=$(rustc -V)
FACT_PYTHON=$(python3 -V)
FACT_ULIMIT_N=$(ulimit -n)
FACT_COMMIT=$commit FACT_RUN=$run_id
FACT_GATEWAY_CPUS=$gateway_cpus FACT_CLIENT_CPUS=$client_cpus
FACT_BIFROST=$bifrost_version FACT_AGENTGATEWAY=$agentgateway_version
FACT_KRAKEND=$krakend_version FACT_LITELLM=$litellm_version
export "${!FACT_@}"
python3 -I - "$out/system.json" <<'PY'
import json, os, sys
facts = {k[5:].lower(): v for k, v in os.environ.items() if k.startswith("FACT_")}
cpuinfo = open("/proc/cpuinfo").read().splitlines()
facts["cpu_model"] = next(l.split(":", 1)[1].strip() for l in cpuinfo if l.startswith("model name"))
facts["vcpus"] = os.cpu_count()
mem = next(l for l in open("/proc/meminfo") if l.startswith("MemTotal"))
facts["memory_kb"] = int(mem.split()[1])
names = ["bifrost", "agentgateway", "krakend", "litellm"]
facts["competitors"] = {k: facts.pop(k) for k in names}
json.dump(facts, open(sys.argv[1], "w"), indent=2, sort_keys=True)
PY
cat "$out/system.json"

step "benchmark"
export WYE_BENCH_GATEWAY_CPUS=$gateway_cpus
# The metric tables (Wye against each competitor) go to table.txt; phase
# durations stay in the log. Each gateway's configuration is written next to
# results.json.
taskset -c "$client_cpus" target/release/wye-bench run \
  --wye target/release/wye --out "$out/results.json" --repeat 3 \
  --wye-alloc target/alloc/release/wye \
  --bifrost "$bin/bifrost-http" --agentgateway "$bin/agentgateway" \
  --litellm "$work/litellm/bin/litellm" --krakend "$bin/krakend" > "$out/table.txt"
step "done"
