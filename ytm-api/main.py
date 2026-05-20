import json
from ytmusicapi import YTMusic

_AUTH_FILE = "ytm-api/browser.json"


def _ytm() -> YTMusic:
    try:
        return YTMusic(_AUTH_FILE)
    except Exception:
        return YTMusic()


def get_library_playlists() -> str:
    playlists = _ytm().get_library_playlists(limit=None)
    return json.dumps(playlists)


def search_playlists(query: str) -> str:
    results = _ytm().search(query, filter="playlists")
    return json.dumps(results)
