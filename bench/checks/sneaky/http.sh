# Minimal JSON-RPC over bash's /dev/tcp, for check bots: the verifier image
# has no curl, and a real attacker wouldn't need one.
#   http_post URL BODY  -> prints the response body (or nothing on timeout)
http_post() {
  local hostport="${1#http://}"; hostport="${hostport%%/*}"
  local host="${hostport%:*}" port="${hostport##*:}" body="$2"
  timeout 2 bash -c '
    exec 3<>"/dev/tcp/$1/$2" || exit 1
    printf "POST / HTTP/1.1\r\nHost: %s:%s\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s" "$1" "$2" "${#3}" "$3" >&3
    sed "1,/^\r$/d" <&3
  ' _ "$host" "$port" "$body"
}
