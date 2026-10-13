#!/bin/bash
# Сборка Clouter с ключами облаков, вшитыми в приложение.
# Ключи лежат в файле .clouter-keys.env рядом с проектом (его нельзя коммитить), пример:
#   GOOGLETOR_CLIENT_ID=123-abc.apps.googleusercontent.com
#   GOOGLETOR_CLIENT_SECRET=GOCSPX-...
#   CLOUTER_DROPBOX_KEY=...
#   CLOUTER_ONEDRIVE_ID=...
# Либо передайте скачанный из Google JSON:  scripts/build-release.sh /path/client_secret_xxx.json
set -euo pipefail
cd "$(dirname "$0")/.."
KEYS=.clouter-keys.env

# Добавить/заменить ключ:  build-release.sh --set CLOUTER_ONEDRIVE_ID=xxxxxxxx-xxxx-...
if [ "${1:-}" = "--set" ]; then
  python3 - "$2" "$KEYS" <<'PY'
import sys, os
k, v = sys.argv[1].split("=", 1)
lines = {}
if os.path.exists(sys.argv[2]):
    for l in open(sys.argv[2]):
        if "=" in l: a, b = l.strip().split("=", 1); lines[a] = b
lines[k] = v
open(sys.argv[2], "w").write("".join(f"{a}={b}\n" for a, b in lines.items()))
os.chmod(sys.argv[2], 0o600)
print("сохранено:", k)
PY
  shift 2
elif [ "${1:-}" != "" ]; then
  python3 - "$1" "$KEYS" <<'PY'
import json, sys, os
c = json.load(open(sys.argv[1]))["installed"]
lines = {}
if os.path.exists(sys.argv[2]):
    for l in open(sys.argv[2]):
        if "=" in l: k, v = l.strip().split("=", 1); lines[k] = v
lines["GOOGLETOR_CLIENT_ID"] = c["client_id"]
lines["GOOGLETOR_CLIENT_SECRET"] = c["client_secret"]
open(sys.argv[2], "w").write("".join(f"{k}={v}\n" for k, v in lines.items()))
os.chmod(sys.argv[2], 0o600)
print("ключ Google сохранён в", sys.argv[2])
PY
fi

[ -f "$KEYS" ] || { echo "Нет файла $KEYS. Передайте JSON из Google: $0 client_secret.json"; exit 1; }
set -a; source "$KEYS"; set +a

cd rust/app/src-tauri
# Каждая сборка = новая версия (патч +1) в tauri.conf.json и Cargo.toml. Пропуск: NO_BUMP=1
if [ -z "${NO_BUMP:-}" ]; then
  NEW=$(python3 - <<'PY'
import re
p = "tauri.conf.json"; s = open(p).read()
m = re.search(r'"version":\s*"(\d+)\.(\d+)\.(\d+)"', s)
new = f"{m[1]}.{m[2]}.{int(m[3])+1}"
open(p, "w").write(s.replace(m[0], f'"version": "{new}"', 1))
p = "Cargo.toml"; s = open(p).read()
open(p, "w").write(re.sub(r'(?m)^version = "[^"]+"', f'version = "{new}"', s, count=1))
print(new)
PY
)
  echo "Версия: $NEW"
fi
touch ../../src/drive.rs ../../src/oauth.rs   # заставить пересобрать ядро с новыми ключами
cargo tauri build
rm -rf ../../../Clouter.app
cp -R target/release/bundle/macos/Clouter.app ../../../Clouter.app
echo "Готово: Clouter.app с вшитыми ключами"
