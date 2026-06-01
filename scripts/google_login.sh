#!/usr/bin/env bash
# Google OAuth login -> save account into ~/.antigravity_sw/accounts/
# Phù hợp với định dạng file account của Antigravity Switcher.
# Dùng: ./scripts/google_login.sh   (mở browser, login, callback về localhost)

set -euo pipefail

# ---- OAuth client (giống src-tauri/src/modules/oauth.rs) ----
CLIENT_ID="1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
CLIENT_SECRET="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"
AUTH_URL="https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_URL="https://oauth2.googleapis.com/token"
USERINFO_URL="https://www.googleapis.com/oauth2/v2/userinfo"
SCOPES="https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs"

# ---- Storage layout (khớp với app) ----
SW_DIR="${HOME}/.antigravity_sw"
ACC_DIR="${SW_DIR}/accounts"
INDEX_FILE="${SW_DIR}/accounts.json"
mkdir -p "${ACC_DIR}"

# ---- Tools check ----
for bin in python3 curl jq uuidgen; do
  command -v "$bin" >/dev/null 2>&1 || { echo "Thiếu lệnh: $bin" >&2; exit 1; }
done

STATE="$(uuidgen)"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT
CODE_FILE="${TMPDIR}/code"
LOG_FILE="${TMPDIR}/server.log"

# ---- Tiny HTTP listener: nhận /oauth-callback?code=...&state=... rồi exit ----
PY_LISTENER="${TMPDIR}/listener.py"
cat > "${PY_LISTENER}" <<'PYEOF'
import sys, socket, urllib.parse

expected_state = sys.argv[1]
code_file = sys.argv[2]

OK_HTML = (
    b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n"
    b"<html><body style='font-family:sans-serif;text-align:center;padding:50px;'>"
    b"<h1 style='color:green;'>Authorization Successful</h1>"
    b"<p>You can close this window.</p>"
    b"<script>setTimeout(()=>window.close(),1500);</script>"
    b"</body></html>"
)
BAD_HTML = (
    b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\n\r\n"
    b"<html><body><h1 style='color:red;'>Authorization Failed</h1></body></html>"
)

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0))
srv.listen(8)
port = srv.getsockname()[1]
print(port, flush=True)  # parent reads first line as port

deadline_loops = 0
while True:
    conn, _ = srv.accept()
    try:
        data = b""
        while b"\r\n\r\n" not in data and len(data) < 8192:
            chunk = conn.recv(4096)
            if not chunk:
                break
            data += chunk
        first_line = data.split(b"\r\n", 1)[0].decode("latin-1", "replace")
        parts = first_line.split(" ")
        if len(parts) >= 2 and parts[1].startswith("/oauth-callback"):
            qs = parts[1].split("?", 1)[1] if "?" in parts[1] else ""
            params = dict(urllib.parse.parse_qsl(qs))
            code = params.get("code")
            state = params.get("state")
            if code and state == expected_state:
                with open(code_file, "w") as f:
                    f.write(code)
                conn.sendall(OK_HTML)
                conn.close()
                srv.close()
                break
            else:
                conn.sendall(BAD_HTML)
        else:
            # favicon, etc.
            conn.sendall(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
    finally:
        try:
            conn.close()
        except Exception:
            pass
    deadline_loops += 1
    if deadline_loops > 50:
        break
PYEOF

# ---- Start listener, read port from first stdout line ----
PORT_FILE="${TMPDIR}/port"
python3 "${PY_LISTENER}" "${STATE}" "${CODE_FILE}" >"${PORT_FILE}" 2>"${LOG_FILE}" &
LISTENER_PID=$!

# Đợi port file có nội dung (timeout ~10 giây)
for _ in $(seq 1 100); do
  if [[ -s "${PORT_FILE}" ]]; then break; fi
  sleep 0.1
done

PORT="$(cat "${PORT_FILE}" 2>/dev/null || true)"
if [[ -z "${PORT}" ]]; then
  echo "Không khởi động được listener (xem ${LOG_FILE})" >&2
  exit 1
fi
if ! [[ "${PORT}" =~ ^[0-9]+$ ]]; then
  echo "Listener trả về port không hợp lệ: ${PORT}" >&2
  cat "${LOG_FILE}" >&2 || true
  exit 1
fi
REDIRECT_URI="http://127.0.0.1:${PORT}/oauth-callback"

# ---- Build auth URL ----
urlencode() { jq -nr --arg v "$1" '$v|@uri'; }
AUTH_REQ_URL="${AUTH_URL}?client_id=$(urlencode "${CLIENT_ID}")&redirect_uri=$(urlencode "${REDIRECT_URI}")&response_type=code&scope=$(urlencode "${SCOPES}")&access_type=offline&prompt=consent&include_granted_scopes=true&state=$(urlencode "${STATE}")"

echo ">> Mở browser để đăng nhập Google..."
echo "   Nếu browser không tự mở, hãy paste URL sau:"
echo "   ${AUTH_REQ_URL}"
if command -v open >/dev/null 2>&1; then
  open "${AUTH_REQ_URL}" >/dev/null 2>&1 || true
elif command -v xdg-open >/dev/null 2>&1; then
  xdg-open "${AUTH_REQ_URL}" >/dev/null 2>&1 || true
fi

echo ">> Đang chờ callback trên ${REDIRECT_URI} (Ctrl-C để hủy)..."
# Đợi listener tự exit (sau khi nhận code) — timeout 5 phút
for _ in $(seq 1 300); do
  if ! kill -0 "${LISTENER_PID}" 2>/dev/null; then
    break
  fi
  sleep 1
done

if [[ ! -s "${CODE_FILE}" ]]; then
  echo "Không nhận được authorization code (timeout hoặc lỗi)." >&2
  cat "${LOG_FILE}" >&2 || true
  exit 1
fi
CODE="$(cat "${CODE_FILE}")"

# ---- Exchange code -> tokens ----
echo ">> Đổi code lấy token..."
TOKEN_JSON="$(curl -sS -X POST "${TOKEN_URL}" \
  --data-urlencode "client_id=${CLIENT_ID}" \
  --data-urlencode "client_secret=${CLIENT_SECRET}" \
  --data-urlencode "code=${CODE}" \
  --data-urlencode "redirect_uri=${REDIRECT_URI}" \
  --data-urlencode "grant_type=authorization_code")"

ACCESS_TOKEN="$(echo "${TOKEN_JSON}" | jq -r '.access_token // empty')"
REFRESH_TOKEN="$(echo "${TOKEN_JSON}" | jq -r '.refresh_token // empty')"
EXPIRES_IN="$(echo "${TOKEN_JSON}" | jq -r '.expires_in // 3599')"
TOKEN_TYPE="$(echo "${TOKEN_JSON}" | jq -r '.token_type // "Bearer"')"

if [[ -z "${ACCESS_TOKEN}" ]]; then
  echo "Token exchange thất bại:" >&2
  echo "${TOKEN_JSON}" >&2
  exit 1
fi
if [[ -z "${REFRESH_TOKEN}" ]]; then
  echo "CẢNH BÁO: Google không trả refresh_token. Vào Google Account -> Security -> Third-party access và gỡ quyền cũ rồi thử lại." >&2
fi

# ---- Get user info ----
USER_JSON="$(curl -sS -H "Authorization: ${TOKEN_TYPE} ${ACCESS_TOKEN}" "${USERINFO_URL}")"
EMAIL="$(echo "${USER_JSON}" | jq -r '.email // empty')"
NAME="$(echo "${USER_JSON}" | jq -r '
  if (.name // "") != "" then .name
  elif ((.given_name // "") != "" or (.family_name // "") != "")
    then ((.given_name // "") + " " + (.family_name // "")) | gsub("^\\s+|\\s+$"; "")
  else null end')"

if [[ -z "${EMAIL}" ]]; then
  echo "Không lấy được email từ userinfo:" >&2
  echo "${USER_JSON}" >&2
  exit 1
fi

# ---- Reuse id nếu email đã tồn tại trong index ----
NOW="$(date +%s)"
EXISTING_ID=""
if [[ -f "${INDEX_FILE}" ]]; then
  EXISTING_ID="$(jq -r --arg e "${EMAIL}" '.accounts[]? | select(.email==$e) | .id' "${INDEX_FILE}" | head -n1)"
fi
ACC_ID="${EXISTING_ID:-$(uuidgen | tr 'A-Z' 'a-z')}"

EXPIRY_TS=$(( NOW + EXPIRES_IN ))
ACC_FILE="${ACC_DIR}/${ACC_ID}.json"

# created_at: giữ giá trị cũ nếu file đã tồn tại
CREATED_AT="${NOW}"
if [[ -f "${ACC_FILE}" ]]; then
  CREATED_AT="$(jq -r --arg n "${NOW}" '.created_at // ($n|tonumber)' "${ACC_FILE}")"
fi

# ---- Ghi file account ----
jq -n \
  --arg id          "${ACC_ID}" \
  --arg email       "${EMAIL}" \
  --arg name        "${NAME}" \
  --arg access      "${ACCESS_TOKEN}" \
  --arg refresh     "${REFRESH_TOKEN}" \
  --argjson expin   "${EXPIRES_IN}" \
  --argjson expts   "${EXPIRY_TS}" \
  --arg ttype       "${TOKEN_TYPE}" \
  --argjson created "${CREATED_AT}" \
  --argjson now     "${NOW}" \
  '{
    id: $id,
    email: $email,
    name: ( $name | select(length>0) ),
    token: {
      access_token: $access,
      refresh_token: $refresh,
      expires_in: $expin,
      expiry_timestamp: $expts,
      token_type: $ttype,
      email: $email
    },
    quota: { models: [], last_updated: $now, is_forbidden: false, subscription_tier: null },
    disabled: false,
    proxy_disabled: false,
    validation_blocked: false,
    created_at: $created,
    last_used: $now
  }' > "${ACC_FILE}"

# ---- Cập nhật accounts.json index ----
if [[ ! -f "${INDEX_FILE}" ]]; then
  echo '{"version":"2.0","accounts":[],"current_account_id":null}' > "${INDEX_FILE}"
fi

TMP_INDEX="$(mktemp)"
jq \
  --arg id "${ACC_ID}" \
  --arg email "${EMAIL}" \
  --arg name "${NAME}" \
  --argjson created "${CREATED_AT}" \
  --argjson now "${NOW}" \
  '
  .version = (.version // "2.0")
  | .accounts = (
      ( .accounts // [] )
      | map(select(.id != $id))
      + [{
          id: $id,
          email: $email,
          name: ( $name | select(length>0) ),
          disabled: false,
          proxy_disabled: false,
          created_at: $created,
          last_used: $now
        }]
    )
  | .current_account_id = ( .current_account_id // $id )
  ' "${INDEX_FILE}" > "${TMP_INDEX}"
mv "${TMP_INDEX}" "${INDEX_FILE}"

echo ">> Đã lưu: ${ACC_FILE}"
echo ">> Đã cập nhật index: ${INDEX_FILE}"
echo

# ---- Xuất danh sách email + refresh_token ----
echo "==================== ACCOUNTS (email | refresh_token) ===================="
jq -r '.accounts[] | "\(.email)\t\(.id)"' "${INDEX_FILE}" | while IFS=$'\t' read -r email id; do
  rt="$(jq -r '.token.refresh_token // ""' "${ACC_DIR}/${id}.json" 2>/dev/null || true)"
  printf '%s\t%s\n' "${email}" "${rt}"
done

echo
echo "==================== EXPORT (bash) ===================="
i=0
jq -r '.accounts[] | "\(.email)\t\(.id)"' "${INDEX_FILE}" | while IFS=$'\t' read -r email id; do
  i=$((i+1))
  rt="$(jq -r '.token.refresh_token // ""' "${ACC_DIR}/${id}.json" 2>/dev/null || true)"
  # Escape single quotes for safe shell single-quoted string
  esc_email="${email//\'/\'\\\'\'}"
  esc_rt="${rt//\'/\'\\\'\'}"
  printf "export GOOGLE_EMAIL_%d='%s'\n"         "$i" "$esc_email"
  printf "export GOOGLE_REFRESH_TOKEN_%d='%s'\n" "$i" "$esc_rt"
done
