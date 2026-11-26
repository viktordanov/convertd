#!/bin/bash
# One measured cell: start the service in a container with fixed limits, run
# ingest.py against it, keep the logs and the state, remove the container.
#
#   OUT_ROOT=DIR [IMAGE=rs-convert:local] [CPUS=2] [MEMORY=3584m] [TMPFS=1] \
#     tools/ingest/cell.sh NAME OFFICE_WORKERS RENDER_WORKERS KILL_MB -- ingest.py args...
#
# /tmp is a Docker volume (disk) unless TMPFS=1, which mounts a 1536 MiB
# tmpfs with TMP_MAX_BYTES at 1 GiB instead: tmpfs pages count against the
# memory limit. WORKER_MAX_RSS_MB is 3/4 of KILL_MB, at most 1024. EXTRA_ENV
# adds docker run flags, for example "-e QUEUE_WAIT_SECONDS=30".
set -u
NAME=$1 OFFICE=$2 RENDER=$3 KILL=$4
shift 5
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_ROOT:?set OUT_ROOT}/$NAME
IMAGE=${IMAGE:-rs-convert:local}
C=${CONTAINER:-rs-convert-ingest}
PORT=${PORT:-18083}
mkdir -p "$OUT"
MAXRSS=$(( KILL * 3 / 4 ))
[ "$MAXRSS" -gt 1024 ] && MAXRSS=1024
CONVERT_API_KEY=$(openssl rand -hex 16)
export CONVERT_API_KEY
docker rm -f "$C" >/dev/null 2>&1
docker volume rm "$C-tmp" >/dev/null 2>&1
if [ "${TMPFS:-0}" = 1 ]; then
  TMP=(--tmpfs /tmp:size=1536m -e TMP_MAX_BYTES=1073741824)
else
  TMP=(--mount "type=volume,src=$C-tmp,dst=/tmp")
fi
# shellcheck disable=SC2086 # EXTRA_ENV holds several flags on purpose
docker run -d --name "$C" --cpus "${CPUS:-2}" --memory "${MEMORY:-3584m}" --memory-swap "${MEMORY:-3584m}" \
  --read-only "${TMP[@]}" --cap-drop ALL --security-opt no-new-privileges \
  -e API_KEY="$CONVERT_API_KEY" -e OFFICE_WORKERS="$OFFICE" -e RENDER_WORKERS="$RENDER" \
  -e WORKER_KILL_RSS_MB="$KILL" -e WORKER_MAX_RSS_MB="$MAXRSS" \
  ${EXTRA_ENV:-} -p "127.0.0.1:$PORT:8080" "$IMAGE" > "$OUT/container.id"
for _ in $(seq 1 120); do
  curl -s "http://127.0.0.1:$PORT/readyz" | grep -q '"ready"' && break
  sleep 0.5
done
printf '{"office":%s,"render":%s,"kill_mb":%s,"max_rss_mb":%s,"tmpfs":%s,"extra":"%s"}\n' \
  "$OFFICE" "$RENDER" "$KILL" "$MAXRSS" "${TMPFS:-0}" "${EXTRA_ENV:-}" > "$OUT/cell.json"
python3 -I -u "$HERE/ingest.py" --base "http://127.0.0.1:$PORT" --container "$C" --out "$OUT" "$@" \
  > "$OUT/console.log" 2>&1
curl -s "http://127.0.0.1:$PORT/metrics" > "$OUT/metrics.txt"
docker logs "$C" > "$OUT/docker.log" 2>&1
docker inspect "$C" --format '{{json .State}}' > "$OUT/state.json"
docker rm -f "$C" >/dev/null
docker volume rm "$C-tmp" >/dev/null 2>&1
echo "cell $NAME done"
