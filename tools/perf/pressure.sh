#!/usr/bin/env bash
# Host pressure experiment only: this does not emulate a Cortex-A55 or Mali GPU.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: pressure.sh [OPTIONS] -- COMMAND [ARGUMENTS...]

Run COMMAND and its descendants in a temporary systemd user-service cgroup.
This is host pressure testing, not RG DS Plus performance simulation.

  --memory-mib N   Accounted memory ceiling in MiB (default: 768)
  --cpu-quota N    CPU quota in percent; 100 = one CPU (default: 400)
  --cpus LIST      Allowed logical CPUs, e.g. 2-5 or 2,4,6,8
                   Default: first four CPUs available to this process, or fewer
  --timeout N     Stop after N seconds, with up to 5 seconds to terminate
  --report-dir P  New directory for cgroup counters; must not already exist
  -h, --help      Show this help

Swap is disabled for the service. The 768 MiB default is an arbitrary stress
envelope, not a measurement of application RAM available on the 1 GB handheld.
Memory and CPU counters include the wrapper shell and all command descendants.
Exported environment variables and the absolute current directory are preserved.
Normal command exit codes propagate; systemd reports OOM/timeout/start failures
as nonzero. Post-run counters may be absent if the service is forcibly killed.
EOF
}

die() { printf 'pressure.sh: %s\n' "$*" >&2; exit 2; }
positive() {
    [[ $2 =~ ^[1-9][0-9]*$ && ${#2} -le 10 ]] && (( 10#$2 <= 2147483647 )) ||
        die "$1 requires a positive integer no greater than 2147483647"
}

memory_mib=768 cpu_quota=400 cpu_list='' timeout_seconds='' report_dir=''
while (( $# )); do
    case $1 in
        --memory-mib|--cpu-quota|--cpus|--timeout|--report-dir)
            option=$1
            (( $# >= 2 )) && [[ -n $2 ]] || die "$option requires a value"
            case $option in
                --memory-mib) positive "$option" "$2"; memory_mib=$2 ;;
                --cpu-quota) positive "$option" "$2"; cpu_quota=$2 ;;
                --cpus) cpu_list=$2 ;;
                --timeout) positive "$option" "$2"; timeout_seconds=$2 ;;
                --report-dir) report_dir=$2 ;;
            esac
            shift 2 ;;
        --) shift; break ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option $1; use -- before the command" ;;
    esac
done
(( $# )) || die 'a command is required after --'
command -v systemd-run >/dev/null || die 'systemd-run is required'
[[ -f /sys/fs/cgroup/cgroup.controllers ]] || die 'cgroup v2 is required'

# Read the caller's affinity rather than assuming host logical CPUs start at 0.
available_list=''
while IFS=: read -r key value; do
    if [[ $key == Cpus_allowed_list ]]; then available_list=${value//[[:space:]]/}; break; fi
done < /proc/self/status
[[ -n $available_list ]] || die 'cannot read available CPUs from /proc/self/status'
declare -A available_cpus=()
IFS=, read -r -a ranges <<< "$available_list"
default_cpus=()
for range in "${ranges[@]}"; do
    first=${range%-*}; last=${range#*-}
    for (( cpu=10#$first; cpu<=10#$last; cpu++ )); do
        available_cpus[$cpu]=1
        if (( ${#default_cpus[@]} < 4 )); then default_cpus+=("$cpu"); fi
    done
done
if [[ -z $cpu_list ]]; then
    cpu_list=$(IFS=,; printf '%s' "${default_cpus[*]}")
fi
[[ $cpu_list =~ ^[0-9]+(-[0-9]+)?(,[0-9]+(-[0-9]+)?)*$ ]] || die 'invalid --cpus list'
IFS=, read -r -a ranges <<< "$cpu_list"
for range in "${ranges[@]}"; do
    first=${range%-*}; last=${range#*-}
    [[ ${#first} -le 7 && ${#last} -le 7 ]] || die 'CPU number is too large'
    (( 10#$first <= 10#$last && 10#$last <= 1048575 )) || die 'invalid CPU range'
    for (( cpu=10#$first; cpu<=10#$last; cpu++ )); do
        [[ ${available_cpus[$cpu]:-} == 1 ]] || die "CPU $cpu is unavailable to this process"
    done
done

if [[ -n $report_dir ]]; then
    report_dir=$(realpath -m -- "$report_dir")
    [[ ! -e $report_dir ]] || die '--report-dir must name a new directory'
    mkdir -p -- "$(dirname -- "$report_dir")"
    mkdir -- "$report_dir"
    printf 'profile=host-pressure\nmemory_mib=%s\ncpu_quota_percent=%s\nallowed_cpus=%s\ntimeout_seconds=%s\n' \
        "$memory_mib" "$cpu_quota" "$cpu_list" "${timeout_seconds:-none}" > "$report_dir/requested.txt"
fi

args=(--user --wait --pipe --collect --service-type=exec --expand-environment=no
      --working-directory="$(pwd -P)"
      --description='Pocket Native host pressure experiment'
      --property="MemoryMax=${memory_mib}M" --property=MemorySwapMax=0
      --property="CPUQuota=${cpu_quota}%" --property="AllowedCPUs=$cpu_list"
      --property="CPUAffinity=$cpu_list" --property=MemoryAccounting=yes)
if [[ -n $timeout_seconds ]]; then
    args+=(--property="RuntimeMaxSec=${timeout_seconds}s" --property=TimeoutStopSec=5s)
fi
# systemd user services otherwise inherit the manager's environment, which can
# silently change PATH, EGL settings or evaluation inputs. Pass names, not values
# in the command line; systemd-run obtains each value from its own environment.
while IFS= read -r name; do
    [[ $name =~ ^[a-zA-Z_][a-zA-Z_0-9]*$ ]] && args+=(--setenv="$name")
done < <(compgen -e)

printf 'host-pressure: memory=%s MiB, swap=0, CPUQuota=%s%%, CPUs=%s; no Arm timing equivalence\n' \
    "$memory_mib" "$cpu_quota" "$cpu_list" >&2

# The child records counters before --collect removes the cgroup. No sampling
# process runs alongside the workload. Peak/CPU counters include shell overhead.
exec systemd-run "${args[@]}" /usr/bin/bash -c '
    set +e
    report_dir=$1
    shift
    cgroup_path=""
    while IFS= read -r line; do
        case $line in 0::*) cgroup_path=/sys/fs/cgroup/${line#0::}; break ;; esac
    done < /proc/self/cgroup
    if [[ -z $cgroup_path || ! -r $cgroup_path/memory.max || ! -r $cgroup_path/cpu.max ]]; then
        printf "pressure.sh: cgroup limits are unavailable\n" >&2
        exit 2
    fi
    capture() {
        local phase=$1 counter
        [[ -n $report_dir ]] || return 0
        mkdir -p -- "$report_dir/$phase"
        while IFS= read -r line; do
            case $line in Cpus_allowed_list:*) printf "%s\n" "$line" > "$report_dir/$phase/affinity.txt" ;; esac
        done < /proc/self/status
        for counter in memory.current memory.peak memory.max memory.swap.max memory.events \
                       cpu.stat cpu.max cpuset.cpus.effective memory.pressure cpu.pressure io.stat; do
            [[ ! -r $cgroup_path/$counter ]] || cat -- "$cgroup_path/$counter" > "$report_dir/$phase/$counter"
        done
    }
    if [[ -n $report_dir ]]; then
        printf "%s\n" "$cgroup_path" > "$report_dir/cgroup-path.txt"
    fi
    capture before
    "$@"
    command_status=$?
    if [[ -n $report_dir ]]; then printf "%s\n" "$command_status" > "$report_dir/command-exit-status.txt"; fi
    capture after
    exit "$command_status"
' pressure-child "$report_dir" "$@"
