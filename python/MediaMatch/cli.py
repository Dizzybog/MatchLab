from __future__ import annotations

import argparse
import json
import logging
import os
import sys

from .api import match_title, serve
from .importers import import_anime_mappings, import_imdb, import_sql_crosswalk, update_anime, update_imdb
from .refresh import refresh_local
from .index import MetadataIndex


def main() -> None:
    parser = argparse.ArgumentParser(prog="media-match")
    parser.add_argument("--db", default="metadata.sqlite3")
    parser.add_argument("--log-level", choices=["DEBUG", "INFO", "WARNING", "ERROR"], default=os.getenv("LOG_LEVEL", "DEBUG").upper())
    commands = parser.add_subparsers(dest="command", required=True)
    imdb = commands.add_parser("import-imdb")
    imdb.add_argument("basics")
    imdb.add_argument("akas")
    imdb.add_argument("--episodes")
    commands.add_parser("update-imdb")
    commands.add_parser("update-anime")
    commands.add_parser("update-all")
    commands.add_parser("refresh-local")
    anime = commands.add_parser("import-anime")
    anime.add_argument("file")
    crosswalk = commands.add_parser("import-crosswalk")
    crosswalk.add_argument("series_tsv")
    crosswalk.add_argument("episodes_tsv")
    match = commands.add_parser("match")
    match.add_argument("title")
    match.add_argument("--year", type=int)
    match.add_argument("--series", action="store_const", const=True, default=None)
    daemon = commands.add_parser("service", help="API with automatic metadata updates")
    daemon.add_argument("--host", default="0.0.0.0")
    daemon.add_argument("--port", type=int, default=8765)
    server = commands.add_parser("serve")
    server.add_argument("--host", default="127.0.0.1")
    server.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    logging.basicConfig(level=getattr(logging, args.log_level), stream=sys.stdout,
                        format="%(asctime)s %(levelname)s [%(threadName)s] %(name)s: %(message)s",
                        force=True)
    logging.getLogger(__name__).info("startup command=%s db=%s log_level=%s", args.command, args.db, args.log_level)
    if args.command in {"update-all", "refresh-local"}:
        imdb_count, anime_count = refresh_local(args.db)
        print(f"Local metadata refreshed: {imdb_count} IMDb rows, {anime_count} anime IDs")
        return
    index = MetadataIndex(args.db)
    if args.command == "import-imdb":
        count = import_imdb(index, args.basics, args.akas, args.episodes)
        print(f"Imported {count} IMDb title records")
    elif args.command == "update-imdb":
        print(f"Imported {update_imdb(index)} IMDb title records")
    elif args.command == "update-anime":
        print(f"Imported {update_anime(index)} anime ID records")
    elif args.command == "import-anime":
        print(f"Imported {import_anime_mappings(index, args.file)} anime ID records")
    elif args.command == "import-crosswalk":
        links, maps = import_sql_crosswalk(index, args.series_tsv, args.episodes_tsv)
        print(f"Imported {links} anime series links and {maps} episode mappings")
    elif args.command == "match":
        print(json.dumps(match_title(index, args.title, args.year, series=args.series), ensure_ascii=False, indent=2))
    elif args.command == "service":
        from .service import run_service
        run_service(args.db, args.host, args.port)
    else:
        serve(index, args.host, args.port)


if __name__ == "__main__":
    main()
