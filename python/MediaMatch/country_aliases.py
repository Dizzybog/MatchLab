"""Small, reviewed country aliases for V4 country-qualified TV series.

This corresponds to the country_aka / exact-country-title stage in SQL V4.
Do not treat release-language tags (MULTI, NOR, ENG) as country suffixes.
"""

from __future__ import annotations

import re

# Country names and commonly encountered local/English alternatives.
COUNTRY_ALIASES: dict[str, tuple[str, ...]] = {
    "US": ("us", "usa", "united states", "america"),
    "UK": ("uk", "gb", "britain", "united kingdom"),
    "NO": ("no", "norway", "norge", "norsk"),
    "SE": ("se", "sweden", "sverige"),
    "DK": ("dk", "denmark", "danmark"),
    "FI": ("fi", "finland", "suomi"),
    "DE": ("de", "germany", "deutschland"),
    "FR": ("fr", "france"),
    "ES": ("es", "spain", "espana", "españa"),
    "IT": ("it", "italy", "italia"),
    "CA": ("ca", "canada"),
    "AU": ("au", "australia"),
    "NZ": ("nz", "new zealand"),
    "JP": ("jp", "japan"),
    "KR": ("kr", "south korea", "korea"),
    "IN": ("in", "india"),
    "BR": ("br", "brazil", "brasil"),
    "MX": ("mx", "mexico", "méxico"),
    "PL": ("pl", "poland", "polska"),
    "NL": ("nl", "netherlands", "holland"),
}

# A single letter/short code only counts when explicitly separated in title.
_STOP = re.compile(
    r"(?i)[ ._-]+(?:s\d{1,3}(?:[ ._-]*e(?:p)?[ ._-]*\d{1,5})?"
    r"|\d{1,2}x\d{1,5}|(?:19|20)\d{2}|2160p|1080p|720p|576p|480p"
    r"|4k|uhd|bluray|blu-ray|bdrip|brrip|web[ ._-]*dl|webrip"
    r"|hdrip|dvdrip|x264|x265|h264|h265|hevc|av1|nf|netflix|amzn|amazon)\b"
)


def country_title_candidates(raw_title: str) -> tuple[str, ...]:
    """Generate exact country-qualified alternatives for a release title.

    Only use country expansion when the trailing country marker is explicit.
    The caller checks these candidates against real IMDb AKAs.
    """
    from .index import clean_title

    title = _STOP.split(raw_title, maxsplit=1)[0].strip(" ._-")
    if not title:
        return ()
    normalized = re.sub(r"[._-]+", " ", title)
    for code, aliases in COUNTRY_ALIASES.items():
        for alias in sorted(aliases, key=len, reverse=True):
            pattern = r"(?i)^(.*?)\s+" + re.escape(alias) + r"$"
            match = re.match(pattern, normalized)
            if not match or not match.group(1).strip():
                continue
            base = clean_title(match.group(1))
            return tuple(dict.fromkeys(
                clean_title(match.group(1) + " " + variant)
                for variant in (code, *aliases)
                if clean_title(match.group(1) + " " + variant)
            ))
    return ()
