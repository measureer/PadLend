"""Console entry point for ``python -m padlend``."""

from __future__ import annotations

import sys


def main(argv: list[str] | None = None) -> int:
    from main import main as app_main

    return app_main(sys.argv[1:] if argv is None else argv)


if __name__ == "__main__":
    raise SystemExit(main())
