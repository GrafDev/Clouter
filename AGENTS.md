# Clouter (папка проекта называется Googletor)

macOS-приложение: торрент (magnet / .torrent) → потоково в облако (Google Drive, Dropbox, OneDrive) без хранения всего файла на диске (окно в RAM/на диске).
Стек: Rust (ядро на `librqbit`) + Tauri 2 (оболочка) + чистый HTML/JS (UI). Интерфейс на русском, i18n в `i18n.js`.

## Где что лежит
- `Clouter.app` — **готовая последняя сборка** (копия из `rust/app/src-tauri/target/release/bundle/macos/`). Установленная копия: `/Applications/Clouter.app`.
- `rust/src/` — ядро (crate `googletor-core`):
  - `lib.rs` — основная логика заданий (торрент → загрузка), `magnet.rs` — разбор magnet-ссылок
  - `drive.rs`, `dropbox.rs`, `onedrive.rs` — клиенты облаков; `oauth.rs` — вход (OAuth); `accounts.rs` — аккаунты
  - `ramstore.rs` — хранилище кусков в памяти; `jobstate.rs` — состояние/возобновление заданий
  - `main.rs` — CLI-вход ядра
- `rust/app/src-tauri/` — оболочка Tauri: `src/main.rs` (все команды `#[tauri::command]`, регистрация в `invoke_handler`, обработка открытия .torrent из Finder, Dock), `tauri.conf.json` (окно, ассоциация .torrent), `icons/`, `capabilities/`.
- `rust/app/ui/` — фронтенд: `index.html` (весь UI и JS), `i18n.js` (переводы), `icon.png`. Вызывает Rust через `window.__TAURI__` (`withGlobalTauri: true`).
- `scripts/build-release.sh` — **единственный правильный способ собирать**: подставляет ключи облаков, запускает `cargo tauri build`, копирует `.app` в корень проекта. `scripts/make-icon.swift` — рисует иконку.
- `engine.py`, `app.py`, `googletor.py`, `ui.html`, `setup.py`, `.venv/` — **старый Python-прототип**, не используется. `magnet-test.html` — страница для проверки magnet-ссылок.

## Сборка и установка
```bash
./scripts/build-release.sh                 # пересобрать → ./Clouter.app (~20 с инкрементально)
pkill -x clouter                           # закрыть запущенное
rm -rf /Applications/Clouter.app && cp -R Clouter.app /Applications/Clouter.app
xattr -dr com.apple.quarantine /Applications/Clouter.app; open /Applications/Clouter.app
```
Каждый запуск скрипта **автоматически поднимает патч-версию** (tauri.conf.json + src-tauri/Cargo.toml); `NO_BUMP=1` отключает. Минор/мажор меняй руками в обоих файлах.
Не запускай `cargo tauri build` напрямую — получится сборка без ключей (вход в облако не заработает). Предупреждение `codesign -v: code has no resources...` ожидаемо и безвредно.
Правки только в `rust/app/ui/` тоже требуют пересборки (UI вшивается в бинарь).

## Ключи и секреты
- `.clouter-keys.env` (в `.gitignore`, права 600): `GOOGLETOR_CLIENT_ID`, `GOOGLETOR_CLIENT_SECRET`, `CLOUTER_DROPBOX_KEY`, `CLOUTER_ONEDRIVE_ID`. Вшиваются в сборку при компиляции.
- Резервная копия ключа Google: `~/Documents/clouter-secrets/`.
- Добавить ключ: `./scripts/build-release.sh --set CLOUTER_ONEDRIVE_ID=...`; из JSON Google: `./scripts/build-release.sh client_secret_xxx.json`.
- Никогда не печатай значения ключей в чат и не коммить их.

## Подводные камни
- `ls` в этом окружении иногда возвращает пустой вывод — проверяй `ls -la` / `ls -d Clouter.app`, не делай вывод «папка пустая».
- Новые команды Tauri нужно добавить и в `invoke_handler` (`src-tauri/src/main.rs`, конец файла), и вызвать из `index.html`.
- Ограничения Drive: 5 ТБ на файл, ~750 ГБ загрузки в сутки.
- Качать только то, на что у пользователя есть права.
