"""Сборка .app:  .venv/bin/python setup.py py2app"""
from setuptools import setup

setup(
    app=["app.py"],
    name="Googletor",
    data_files=[("", ["ui.html"])],
    options={"py2app": {
        "packages": ["libtorrent", "googleapiclient", "google_auth_oauthlib",
                     "google", "webview", "engine", "httplib2", "uritemplate"],
        "plist": {
            "CFBundleName": "Googletor",
            "CFBundleIdentifier": "app.googletor",
            "CFBundleShortVersionString": "0.1",
            "LSMinimumSystemVersion": "11.0",
        },
    }},
    setup_requires=["py2app"],
)
