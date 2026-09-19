#!/bin/bash

cd "$(dirname -- "${BASH_SOURCE[0]}")" || exit

./daemon/build.sh || exit $?

# Build first so the daemon can recreate the selected profile stack with the
# newly built image. A provider-default Compose startup has no local profile
# filename, so bootstrap one through the daemon rather than claiming an
# arbitrary Gluetun-selected server is one of our local profiles.
case "$(readlink docker-compose.yml 2>/dev/null || true)" in
  *protonvpn*)
    provider="protonvpn"
    profile_overlay=/var/lib/webspeak3/profile-selection.protonvpn.override.yml
    ;;
  *nordvpn*)
    provider="nordvpn"
    profile_overlay=/var/lib/webspeak3/profile-selection.nordvpn.override.yml
    ;;
  *)
    provider=""
    profile_overlay=""
    ;;
esac

docker compose build || exit $?

if [[ -z "$provider" ]]; then
  docker compose down || exit $?
  docker compose up -d || exit $?
elif [[ ! -f "$profile_overlay" ]]; then
  # This explicit route chooses a local profile, recreates the stack, waits for
  # Gluetun, verifies leak=false, then persists its basename and overlay.
  rotation_response=$(curl --fail --silent --show-error -X POST "http://127.0.0.1:3000/rotate/${provider}/profile") || exit $?
  printf '%s\n' "$rotation_response"
  if ! jq -e '.success == true' >/dev/null <<<"$rotation_response"; then
    echo "Verified ${provider} profile bootstrap failed; the Compose stack was not treated as ready." >&2
    exit 1
  fi
else
  # Recreate the already verified profile with the newly built application
  # image. The overlay is provider-specific and contains no key material.
  docker compose down || exit $?
  docker compose -f docker-compose.yml -f docker-compose.override.yml -f "$profile_overlay" up -d --force-recreate || exit $?
fi
