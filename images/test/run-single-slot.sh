#!/usr/bin/env bash
# Runs `gitlab-runner run-single` with the VM agent's slot flags (crates/vm-agent/src/agent.rs)
# against a stub GitLab that answers every job request with 204, then sends SIGQUIT and checks
# that the idle slot stops requesting and exits.
# Usage: images/test/run-single-slot.sh [runner image]
set -euo pipefail

export MSYS_NO_PATHCONV=1

image="${1:-gitlab/gitlab-runner:v18.11.4}"
net="ciburst-slot-smoke"
stub="ciburst-gitlab-stub"
slot="ciburst-slot"

cleanup() {
  docker rm -f "$stub" "$slot" > /dev/null 2>&1 || true
  docker network rm "$net" > /dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

stub_code='
import http.server

class Stub(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        print(self.command, self.path, flush=True)
        self.send_response(204)
        self.end_headers()

    do_PUT = do_POST
    do_GET = do_POST

    def log_message(self, *args):
        pass

http.server.ThreadingHTTPServer(("", 80), Stub).serve_forever()
'

docker network create "$net" > /dev/null
docker run -d --name "$stub" --network "$net" python:3.13-slim python -c "$stub_code" > /dev/null
docker run -d --name "$slot" --network "$net" \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --entrypoint gitlab-runner "$image" run-single \
  --url "http://$stub" \
  --token glrt-smoke-token \
  --executor docker \
  --docker-image alpine:3.22 \
  --docker-privileged \
  --docker-volumes /cache \
  --docker-volumes /certs/client \
  --max-builds 0 \
  --wait-timeout 0 \
  --config /var/lib/ciburst/slot-1/config.toml \
  --cache-type s3 \
  --cache-s3-server-address s3.fr-par.scw.cloud \
  --cache-s3-bucket-name ci-cache-smoke \
  --cache-s3-bucket-location fr-par \
  --cache-s3-access-key SCWSMOKEACCESSKEY \
  --cache-s3-secret-key smoke-secret-key \
  --cache-shared > /dev/null

sleep 12
requests="$(docker logs "$stub" 2>&1 | grep -c 'POST /api/v4/jobs/request' || true)"
echo "job requests in 12 s while idle: $requests"

docker kill --signal QUIT "$slot" > /dev/null
start=$(date +%s)
while [ "$(docker inspect --format '{{.State.Running}}' "$slot")" = "true" ]; do
  if [ $(( $(date +%s) - start )) -gt 60 ]; then
    echo "slot still running 60 s after SIGQUIT" >&2
    docker logs "$slot" >&2
    exit 1
  fi
  sleep 1
done
code="$(docker inspect --format '{{.State.ExitCode}}' "$slot")"
echo "exit after SIGQUIT: $(( $(date +%s) - start )) s, code $code"

logs="$(docker logs "$slot" 2>&1)"
failures=0
if [ "$requests" -lt 2 ]; then
  echo "expected at least 2 job requests" >&2
  failures=$((failures + 1))
fi
if [[ "$logs" != *"Requested quit, waiting for builds to finish"* ]]; then
  echo "no quit message in the runner log" >&2
  failures=$((failures + 1))
fi
if [ "$code" != "0" ]; then
  echo "runner exited with $code" >&2
  failures=$((failures + 1))
fi
if [ "$failures" -ne 0 ]; then
  echo "$logs" >&2
  exit 1
fi
echo "run-single slot smoke passed"
