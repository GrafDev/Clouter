# Googletor

Торрент → Google Drive без хранения всего файла на диске.

## Настройка (один раз)
1. https://console.cloud.google.com → создать проект → включить **Google Drive API**.
2. «Credentials» → Create credentials → **OAuth client ID** → Desktop app → скачать JSON,
   сохранить сюда как `credentials.json`. (В OAuth consent screen добавьте себя в Test users.)
3. Запуск при первом разе откроет браузер для входа.

## Запуск
```bash
.venv/bin/python googletor.py "magnet:?xt=..." --folder "Мои торренты"
.venv/bin/python googletor.py file.torrent --window-gb 4
```

`--window-gb` — сколько максимум держать на диске (по умолчанию 2 ГБ).
Нужно свободное место ≈ окно + 256 МБ, независимо от размера файла.

## Ограничения
- Google Drive: лимит 5 ТБ на файл и ~750 ГБ загрузки в сутки.
- Если обрыв посреди файла, этот файл начнётся заново.
- Торрент без сидов в потоковом режиме качается медленно: куски идут по порядку.
- Скачивайте только то, на что у вас есть права.
