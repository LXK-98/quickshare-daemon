#!/bin/bash
# Logs EVERY sighting with a timestamp. The gap while the sheet is closed is
# what makes two sessions distinguishable, which the deduped version destroyed.
end=$(( $(date +%s) + ${1:-200} ))
echo "watching until $(date -d "@$end" +%H:%M:%S) -- every sighting, no dedup"
while [ "$(date +%s)" -lt "$end" ]; do
  timeout 5 avahi-browse -rpt _FC9F5ED42C8A._tcp 2>/dev/null | grep '^=' |
  while IFS=';' read -r _ iface proto fullname _ _ host addr port txt; do
    [ -z "$fullname" ] && continue
    name=$(printf '%s' "$txt" | sed -n 's/.*"n=\([^"]*\)".*/\1/p' \
      | tr '_-' '/+' | base64 -d 2>/dev/null | tr -cd '[:print:]' | tail -c 24)
    # Only the phone; this host and the other machine are noise here.
    case "$name" in
      *Pixel*) printf '%s  %s  %s:%s\n' "$(date +%H:%M:%S)" "$fullname" "$addr" "$port" ;;
    esac
  done
  sleep 3
done
echo "done"
