"""Движок Googletor: торрент -> Google Drive / iCloud Drive.

Google Drive: поток кусками по 256 МБ, на диске только скользящее окно,
залитое вырезается из временного файла (APFS hole-punch) -> файлы любого размера.
iCloud Drive: файл скачивается целиком во временную папку, переносится в
iCloud Drive; нужно свободное место под самый большой файл раздачи.
"""
import fcntl
import mimetypes
import os
import shutil
import struct
import subprocess
import threading
import time
import uuid

import libtorrent as lt
from google.auth.transport.requests import Request
from google.oauth2.credentials import Credentials
from google_auth_oauthlib.flow import InstalledAppFlow
from googleapiclient.discovery import build
from googleapiclient.http import MediaUpload

APP_DIR = os.path.expanduser("~/Library/Application Support/Googletor")
CREDS = os.path.join(APP_DIR, "credentials.json")
TOKEN = os.path.join(APP_DIR, "token.json")
ICLOUD = os.path.expanduser("~/Library/Mobile Documents/com~apple~CloudDocs")
SCOPES = ["https://www.googleapis.com/auth/drive.file"]
CHUNK = 256 * 1024 * 1024
F_PUNCHHOLE = 99

os.makedirs(APP_DIR, exist_ok=True)


class Cancelled(Exception):
    pass


# ---------- Google Drive ----------

def drive_signed_in():
    return os.path.exists(TOKEN)


def drive_service():
    creds = None
    if os.path.exists(TOKEN):
        creds = Credentials.from_authorized_user_file(TOKEN, SCOPES)
    if not creds or not creds.valid:
        if creds and creds.expired and creds.refresh_token:
            creds.refresh(Request())
        else:
            if not os.path.exists(CREDS):
                raise RuntimeError("Не задан credentials.json (Настройки → Google Drive)")
            flow = InstalledAppFlow.from_client_secrets_file(CREDS, SCOPES)
            creds = flow.run_local_server(port=0)
        with open(TOKEN, "w") as f:
            f.write(creds.to_json())
    return build("drive", "v3", credentials=creds, cache_discovery=False)


def punch_hole(path, start, length):
    blk = 4096
    s = (start + blk - 1) // blk * blk
    e = (start + length) // blk * blk
    if e <= s:
        return
    try:
        fd = os.open(path, os.O_RDWR)
        try:
            fcntl.fcntl(fd, F_PUNCHHOLE, struct.pack("IIqq", 0, 0, s, e - s))
        finally:
            os.close(fd)
    except OSError:
        pass


class TorrentFileUpload(MediaUpload):
    def __init__(self, job, index):
        self.job = job
        fs = job.ti.files()
        self._size = fs.file_size(index)
        self._path = os.path.join(job.save_path, fs.file_path(index))
        self._base = fs.file_offset(index)
        self._mime = mimetypes.guess_type(self._path)[0] or "application/octet-stream"
        self.last_piece = (self._base + self._size - 1) // job.plen

    def chunksize(self): return CHUNK
    def mimetype(self): return self._mime
    def size(self): return self._size
    def resumable(self): return True
    def has_stream(self): return False
    def stream(self): raise NotImplementedError

    def getbytes(self, begin, length):
        j = self.job
        length = min(length, self._size - begin)
        a = (self._base + begin) // j.plen
        b = (self._base + begin + length - 1) // j.plen
        end = min(self.last_piece, a + j.window_pieces)
        prios = [0] * j.ti.num_pieces()
        for p in range(a, end + 1):
            prios[p] = 7 if p < a + 4 else 4
        j.th.prioritize_pieces(prios)
        j.status = "Скачиваю"
        while not all(j.th.have_piece(p) for p in range(a, b + 1)):
            j.check_cancel()
            j.refresh_rates()
            time.sleep(0.5)
        with open(self._path, "rb") as f:
            f.seek(begin)
            data = f.read(length)
        punch_hole(self._path, begin, length)
        j.status = "Загружаю на Google Drive"
        return data


# ---------- Задача ----------

_ses = None
_ses_lock = threading.Lock()


def session():
    global _ses
    with _ses_lock:
        if _ses is None:
            _ses = lt.session({"listen_interfaces": "0.0.0.0:6881"})
        return _ses


class Job:
    def __init__(self, source, dest, window_gb=2.0):
        self.id = uuid.uuid4().hex[:8]
        self.source, self.dest = source, dest
        self.window_gb = window_gb
        self.name = source[:60] if not source.startswith("magnet:") else "magnet…"
        self.status = "В очереди"
        self.error = None
        self.done_bytes = 0
        self.total_bytes = 0
        self.rate = 0
        self.peers = 0
        self.cancelled = False
        self.finished = False
        self.save_path = os.path.join(APP_DIR, "tmp", self.id)
        self.th = self.ti = None
        self._folders = {}
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def cancel(self):
        self.cancelled = True

    def check_cancel(self):
        if self.cancelled:
            raise Cancelled()

    def refresh_rates(self):
        s = self.th.status()
        self.rate, self.peers = s.download_rate, s.num_peers

    def info(self):
        return dict(id=self.id, name=self.name, dest=self.dest, status=self.status,
                    error=self.error, done=self.done_bytes, total=self.total_bytes,
                    rate=self.rate, peers=self.peers, finished=self.finished)

    def _run(self):
        try:
            os.makedirs(self.save_path, exist_ok=True)
            self._download_and_deliver()
            self.status = "Готово"
        except Cancelled:
            self.status = "Отменено"
        except Exception as e:  # noqa
            self.status = "Ошибка"
            self.error = str(e)
        finally:
            self.finished = True
            self.rate = 0
            try:
                if self.th is not None:
                    session().remove_torrent(self.th)
            except Exception:
                pass
            shutil.rmtree(self.save_path, ignore_errors=True)

    def _download_and_deliver(self):
        if self.source.startswith("magnet:"):
            p = lt.parse_magnet_uri(self.source)
        else:
            p = lt.add_torrent_params()
            p.ti = lt.torrent_info(self.source)
        p.save_path = self.save_path
        self.th = session().add_torrent(p)
        self.status = "Получаю метаданные"
        while not self.th.status().has_metadata:
            self.check_cancel()
            time.sleep(0.5)
        self.ti = self.th.torrent_file()
        self.name = self.ti.name()
        self.plen = self.ti.piece_length()
        self.window_pieces = max(8, int(self.window_gb * 1024**3 // self.plen))
        fs = self.ti.files()
        n = fs.num_files()
        self.total_bytes = sum(fs.file_size(i) for i in range(n))

        drive = drive_service() if self.dest == "drive" else None
        if drive:
            root = self._drive_folder(drive, "Googletor", None)
        else:
            root = os.path.join(ICLOUD, "Googletor")
            if not os.path.isdir(ICLOUD):
                raise RuntimeError("iCloud Drive не найден (включите его в Системных настройках)")

        for i in range(n):
            self.check_cancel()
            size = fs.file_size(i)
            rel = fs.file_path(i)
            if size == 0:
                continue
            self.th.prioritize_files([1 if j == i else 0 for j in range(n)])
            if drive:
                self._upload_drive(drive, root, i, rel, size)
            else:
                self._deliver_icloud(root, i, rel, size)
            self.done_bytes += size

    # Drive
    def _drive_folder(self, drive, name, parent):
        key = (name, parent)
        if key in self._folders:
            return self._folders[key]
        body = {"name": name, "mimeType": "application/vnd.google-apps.folder"}
        if parent:
            body["parents"] = [parent]
        fid = drive.files().create(body=body, fields="id").execute()["id"]
        self._folders[key] = fid
        return fid

    def _upload_drive(self, drive, root, i, rel, size):
        parent = root
        parts = rel.split("/")
        for d in parts[:-1]:
            parent = self._drive_folder(drive, d, parent)
        media = TorrentFileUpload(self, i)
        req = drive.files().create(body={"name": parts[-1], "parents": [parent]},
                                   media_body=media, fields="id")
        base = self.done_bytes
        resp = None
        while resp is None:
            self.check_cancel()
            st, resp = req.next_chunk(num_retries=5)
            if st:
                self.done_bytes = base + int(st.progress() * size)
        self.done_bytes = base

    # iCloud
    def _deliver_icloud(self, root, i, rel, size):
        self.status = "Скачиваю"
        src = os.path.join(self.save_path, rel)
        self.th.prioritize_pieces([4] * self.ti.num_pieces())
        base = self.done_bytes
        fs = self.ti.files()
        a = fs.file_offset(i) // self.plen
        b = (fs.file_offset(i) + size - 1) // self.plen
        while not all(self.th.have_piece(p) for p in range(a, b + 1)):
            self.check_cancel()
            self.refresh_rates()
            frac = sum(self.th.have_piece(p) for p in range(a, b + 1)) / (b - a + 1)
            self.done_bytes = base + int(frac * size * 0.9)
            time.sleep(0.5)
        self.status = "Переношу в iCloud Drive"
        dst = os.path.join(root, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        shutil.move(src, dst)
        # освободить локальное место, когда iCloud выгрузит файл
        threading.Thread(target=_evict_later, args=(dst,), daemon=True).start()
        self.done_bytes = base


def _evict_later(path):
    time.sleep(60)
    for _ in range(600):  # до ~10 часов пробуем
        r = subprocess.run(["brctl", "evict", path], capture_output=True)
        if r.returncode == 0:
            return
        time.sleep(60)
