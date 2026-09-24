# Runs ON a Samba server, piped over ssh (no root needed):
#
#   ssh <server> "SECS=240 CLIENT=<client IP prefix> sh -s" < server-sample.sh > nas-sample.txt
#
# Every ~20 ms, prints a nanosecond timestamp and the state letter (R, S, D, ...)
# of every thread of each `smbd` serving CLIENT that started after this script
# did, so it catches the bench's connections and not older ones. `timeline
# --nas` reads the output: an `smbd` in `D` (blocked in the kernel) while a
# side `stat` waits is the server holding it. The gaps between timestamps
# double as a user-space scheduling probe on the server.
#
# `smbd` switches its effective user per request, so processes are matched by
# name alone, and the list refreshes every ~50 samples.
SECS=${SECS:-60}
CLIENT=${CLIENT:-192}
before=$(ps -o pid,comm | awk '$2 ~ /^smbd\[/ {print $1}' | tr '\n' ' ')
end=$(( $(date +%s) + SECS ))
i=0
pids=""
while [ "$(date +%s)" -lt "$end" ]; do
  if [ $((i % 50)) -eq 0 ]; then
    new=""
    for p in $(ps -o pid,comm | awk -v c="$CLIENT" 'index($2, "smbd[" c) == 1 {print $1}'); do
      case " $before " in *" $p "*) ;; *) new="$new $p";; esac
    done
    if [ "$new" != "$pids" ]; then pids=$new; echo "# pids:$pids"; fi
  fi
  i=$((i + 1))
  line=$(date +%s%N)
  for p in $pids; do
    for t in /proc/$p/task/*; do
      s=$(cut -d' ' -f3 "$t/stat" 2>/dev/null)
      line="$line ${t##*/}:${s:-x}"
    done
  done
  echo "$line"
  usleep 20000
done
