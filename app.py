"""Googletor — окно приложения (pywebview)."""
import os
import shutil
import threading

import webview

import engine

HERE = os.path.dirname(os.path.abspath(__file__))
if "Resources" in HERE and not os.path.exists(os.path.join(HERE, "ui.html")):
    HERE = os.environ.get("RESOURCEPATH", HERE)


class Api:
    def __init__(self):
        self.jobs = {}
        self.window_gb = 2.0

    def add(self, source, dest):
        source = (source or "").strip()
        if not source:
            return {"error": "Вставьте magnet-ссылку или выберите .torrent"}
        if not (source.startswith("magnet:") or os.path.isfile(source)):
            return {"error": "Это не magnet-ссылка и не файл .torrent"}
        j = engine.Job(source, dest, self.window_gb)
        self.jobs[j.id] = j
        return {"id": j.id}

    def pick_torrent(self):
        w = webview.windows[0]
        r = w.create_file_dialog(webview.OPEN_DIALOG, file_types=("Torrent (*.torrent)",))
        return r[0] if r else ""

    def list(self):
        return [j.info() for j in self.jobs.values()]

    def cancel(self, jid):
        if jid in self.jobs:
            self.jobs[jid].cancel()

    def clear_finished(self):
        self.jobs = {k: j for k, j in self.jobs.items() if not j.finished}

    def settings(self):
        return {"creds": os.path.exists(engine.CREDS),
                "signed_in": engine.drive_signed_in(),
                "window_gb": self.window_gb,
                "icloud": os.path.isdir(engine.ICLOUD)}

    def set_window(self, gb):
        self.window_gb = max(0.5, float(gb))

    def pick_credentials(self):
        r = webview.windows[0].create_file_dialog(
            webview.OPEN_DIALOG, file_types=("JSON (*.json)",))
        if r:
            shutil.copy(r[0], engine.CREDS)
        return self.settings()

    def sign_in(self):
        try:
            engine.drive_service()
            return {"ok": True}
        except Exception as e:  # noqa
            return {"error": str(e)}

    def sign_out(self):
        if os.path.exists(engine.TOKEN):
            os.remove(engine.TOKEN)
        return self.settings()


if __name__ == "__main__":
    api = Api()
    webview.create_window("Googletor", os.path.join(HERE, "ui.html"),
                          js_api=api, width=760, height=720, min_size=(560, 520))
    webview.start()
