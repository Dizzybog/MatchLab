"""StremThru-compatible Anime-Lists override merging and episode-map preparation.

Adapted from MunifTanjim/stremthru internal/animelists/dataset.go (GPL-3.0).
Upstream override XML is bundled under data/anime-list.override.xml.
"""

from __future__ import annotations

import xml.etree.ElementTree as ET
from importlib.resources import files


def _num(value: str | None, fallback: int = 0) -> int:
    try:
        return int(value) if value not in (None, "") else fallback
    except ValueError:
        return fallback


def _merge(original: ET.Element, override: ET.Element) -> None:
    for key, value in override.attrib.items():
        if value and key != "anidbid":
            original.set(key, value)
    for key in ("name", "before"):
        element = override.find(key)
        if element is not None and (element.text or "").strip():
            old = original.find(key)
            if old is not None:
                original.remove(old)
            original.append(ET.fromstring(ET.tostring(element)))
    updated = override.find("mapping-list")
    if updated is None or not len(updated):
        return
    current = original.find("mapping-list")
    if current is None:
        current = ET.SubElement(original, "mapping-list")
    by_key = {(m.get("anidbseason"), m.get("tvdbseason")): m
              for m in current.findall("mapping")}
    for m in updated.findall("mapping"):
        key = (m.get("anidbseason"), m.get("tvdbseason"))
        prior = by_key.get(key)
        new = ET.fromstring(ET.tostring(m))
        if prior is not None:
            if (prior.text or "").strip() and not (new.text or "").strip():
                new.text = prior.text
            current.remove(prior)
        current.append(new)


def merged_items(payload: bytes, override: bytes | None = None) -> list[ET.Element]:
    root = ET.fromstring(payload)
    if root.tag != "anime-list":
        raise ValueError("Invalid Anime-Lists XML")
    if override is None:
        override = files("MediaMatch").joinpath("data/anime-list.override.xml").read_bytes()
    corrections = ET.fromstring(override)
    by_id = {e.get("anidbid"): e for e in root.findall("anime")}
    for o in corrections.findall("anime"):
        key = o.get("anidbid")
        if key in by_id:
            _merge(by_id[key], o)
        else:
            root.append(o)
    return [e for e in root.findall("anime")
            if (e.get("anidbid") or "").isdigit()
            and (e.get("tvdbid") or "").isdigit()]


def prepare_episode_maps(items: list[ET.Element]) -> list[tuple]:
    """Follow StremThru's per-TVDB processing, including inferred part boundaries.

    Output is (anidb_id,tvdb_id,anidb_season,tvdb_season,start,end,offset,map).
    """
    by_tvdb: dict[str, list[ET.Element]] = {}
    for item in items:
        by_tvdb.setdefault(item.get("tvdbid", ""), []).append(item)
    output = []
    for tvdb_id, group in by_tvdb.items():
        last_part: dict[int, int] = {}
        part_index: dict[tuple[int, int], int] = {}
        maps: list[dict] = []
        for item in group:
            anidb = item.get("anidbid", "")
            default = item.get("defaulttvdbseason", "")
            if not default:
                continue
            season = -1 if default == "a" else _num(default, -999)
            if season == -999:
                continue
            offset = _num(item.get("episodeoffset"))
            row = dict(anidb=anidb, tvdb=tvdb_id, ani_season=1,
                       season=season, start=1 if season == -1 else 0,
                       end=0, offset=offset, mapping="")
            maps.append(row)
            seen = {(1, season): len(maps) - 1}
            if season > 0:
                part = last_part.get(season, 0) + 1
                last_part[season] = part
                part_index[(season, part)] = len(maps) - 1
                if part > 1:
                    row["start"] = 1
                    prev = maps[part_index[(season, part - 1)]]
                    prev["start"] = prev["start"] or 1
                    prev["end"] = prev["end"] or offset - prev["offset"]
            for element in item.findall("./mapping-list/mapping"):
                ani = _num(element.get("anidbseason"), -999)
                tv = _num(element.get("tvdbseason"), -999)
                if ani not in (0, 1) or tv == -999:
                    continue
                key = (ani, tv)
                if key in seen:
                    mapped = maps[seen[key]]
                else:
                    mapped = dict(anidb=anidb, tvdb=tvdb_id, ani_season=ani,
                                  season=tv, start=0, end=0, offset=0, mapping="")
                for xml_key, field in (("start", "start"), ("end", "end"),
                                       ("offset", "offset")):
                    if element.get(xml_key) not in (None, ""):
                        mapped[field] = _num(element.get(xml_key))
                if (element.text or "").strip():
                    mapped["mapping"] = element.text.strip()
                if key not in seen:
                    if tv == 2 and ani == 1:
                        first = (ani, 1)
                        if first in seen:
                            previous = maps[seen[first]]
                            if not previous["start"] and not previous["end"]:
                                previous["offset"] = previous["offset"] or (
                                    mapped["start"] - 1 + mapped["offset"])
                                previous["start"] = 1
                                previous["end"] = mapped["start"] - 1
                        else:
                            maps.append(dict(anidb=anidb, tvdb=tvdb_id,
                                ani_season=ani, season=1, start=1,
                                end=mapped["start"] - 1,
                                offset=mapped["start"] - 1 + mapped["offset"],
                                mapping=""))
                            seen[first] = len(maps) - 1
                    maps.append(mapped)
                    seen[key] = len(maps) - 1
                absolute = seen.get((1, -1))
                if absolute is not None:
                    absrow = maps[absolute]
                    if not mapped["end"] or absrow["end"] < mapped["end"]:
                        absrow["end"] = mapped["end"]
        output.extend((m["anidb"], m["tvdb"], m["ani_season"],
                       m["season"], m["start"], m["end"], m["offset"],
                       m["mapping"]) for m in maps)
    return output
