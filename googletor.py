#!/usr/bin/env python3
"""Googletor: скачивает торрент и стримом заливает его на Google Drive.

Файлы любого размера: на диске держится только скользящее окно (--window-gb),
уже загруженные куски вырезаются из временного файла (APFS hole-punch).
"""
import argparse
import fcntl
import mimetypes
import os
import shutil
import struct
import sys
import time

import libtorrent as lt
from google.auth.transport.requests import Request
from google.oauth2.credentials import Credentials
from google_auth_oauthlib.flow import InstalledAppFlow
from googleapiclient.discovery import build
from googleapiclient.http import MediaUpload

SCOPES = ["https://www.googleapis.com/auth/drive.file"]
CHUNK = 256 * 1024 * 1024  # кратно 256 КБ, как требует Drive
F_PUNCHHOLE = 99  # macOS


def drive_service(creds_path, token_path):
    creds = None
    if os.path.exists(token_path):
        creds = Credentials.from_authorized_user_file(token_path, SCOPES)
    if not creds or not creds.valid:
        if creds and creds.expired and creds.refresh_token:
            creds.refresh(Request())
        else:
            flow = InstalledAppFlow.from_client_secrets_file(creds_path, SCOPES)
            creds = flow.run_local_server(port=0)
        with open(token_path, "w") as f:
            f.write(creds.to_json())
    return build("drive", "v3", credentials=creds)


def punch_hole(path, start, length):
    """Освобождает место на диске под уже загруженным диапазоном."""
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
        pass  # не критично: просто не освободим место до конца файла


class TorrentFileUpload(MediaUpload):
    """Источник данных для Drive: блокируется, пока нужные куски не скачаны."""

    def __init__(self, ctx, index):
        self.ctx, self.index = ctx, index
        fs = ctx.ti.files()
        self._size = fs.file_size(index)
        self._path = os.path.join(ctx.save_path, fs.file_path(index))
        self._base = fs.file_offset(index)
        self._mime = mimetypes.guess_type(self._path)[0] or "application/octet-stream"
        self.first_piece = self._base // ctx.plen
        self.last_piece = max(self._base, self._base + self._size - 1) // ctx.plen

    def chunksize(self):
        return CHUNK

    def mimetype(self):
        return self._mime

    def size(self):
        return self._size

    def resumable(self):
        return True

    def has_stream(self):
        return False

    def stream(self):
        raise NotImplementedError

    def getbytes(self, begin, length):
        length = min(length, self._size - begin)
        c = self.ctx
        need_a = (self._base + begin) // c.plen
        need_b = (self._base + begin + length - 1) // c.plen
        self._set_window(need_a)
        while not all(c.th.have_piece(p) for p in range(need_a, need_b + 1)):
            c.report(self)
            time.sleep(1)
        with open(self._path, "rb") as f:
            f.seek(begin)
            data = f.read(length)
        punch_hole(self._path, begin, length)
        return data

    def _set_window(self, cur):
        c = self.ctx
        end = min(self.last_piece, cur + c.window_pieces)
        prios = [0] * c.ti.num_pieces()
        for p in range(cur, end + 1):
            prios[p] = 7 if p < cur + 4 else 4
        c.th.prioritize_pieces(prios)


class Ctx:
    pass


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("torrent", help="magnet-ссылка или путь к .torrent")
    ap.add_argument("--folder", default="Googletor", help="папка на Drive (создаётся)")
    ap.add_argument("--tmp", default="./tmp_download", help="временная папка")
    ap.add_argument("--window-gb", type=float, default=2.0, help="сколько ГБ держать на диске")
    ap.add_argument("--credentials", default="credentials.json")
    ap.add_argument("--token", default="token.json")
    a = ap.parse_args()

    drive = drive_service(a.credentials, a.token)
    folder = drive.files().create(
        body={"name": a.folder, "mimeType": "application/vnd.google-apps.folder"},
        fields="id",
    ).execute()["id"]

    ses = lt.session({"listen_interfaces": "0.0.0.0:6881"})
    os.makedirs(a.tmp, exist_ok=True)
    if a.torrent.startswith("magnet:"):
        p = lt.parse_magnet_uri(a.torrent)
    else:
        p = lt.add_torrent_params()
        p.ti = lt.torrent_info(a.torrent)
    p.save_path = os.path.abspath(a.tmp)
    th = ses.add_torrent(p)

    print("Получаю метаданные...")
    while not th.status().has_metadata:
        time.sleep(1)

    c = Ctx()
    c.th, c.ti, c.save_path = th, th.torrent_file(), os.path.abspath(a.tmp)
    c.plen = c.ti.piece_length()
    c.window_pieces = max(8, int(a.window_gb * 1024**3 // c.plen))
    last = [0.0]

    def report(up):
        if time.time() - last[0] < 5:
            return
        last[0] = time.time()
        s = th.status()
        print(f"\r{up._path.split('/')[-1][:40]} | ↓ {s.download_rate/1e6:.1f} МБ/с "
              f"| пиров {s.num_peers}   ", end="", flush=True)

    c.report = report

    fs = c.ti.files()
    for i in range(fs.num_files()):
        th.prioritize_files([1 if j == i else 0 for j in range(fs.num_files())])
        name = fs.file_path(i)
        if fs.file_size(i) == 0:
            continue
        print(f"\n[{i+1}/{fs.num_files()}] {name} ({fs.file_size(i)/1e9:.2f} ГБ)")
        media = TorrentFileUpload(c, i)
        parent = folder
        parts = name.split("/")
        for d in parts[1:-1] if len(parts) > 2 else []:
            parent = drive.files().create(
                body={"name": d, "mimeType": "application/vnd.google-apps.folder",
                      "parents": [parent]}, fields="id").execute()["id"]
        req = drive.files().create(
            body={"name": parts[-1], "parents": [parent]},
            media_body=media, fields="id")
        resp = None
        while resp is None:
            st, resp = req.next_chunk(num_retries=5)
            if st:
                print(f"\r  загружено {st.progress()*100:.1f}%{' '*40}", end="", flush=True)
        print("\n  готово")
        try:
            os.remove(media._path)
        except OSError:
            pass

    ses.remove_torrent(th)
    shutil.rmtree(a.tmp, ignore_errors=True)
    print("\nВсё загружено на Google Drive.")


if __name__ == "__main__":
    sys.exit(main())
