"""The long-lived controller and local configuration API, supervised by systemd."""

import sys

from .server import main


if __name__ == "__main__":
    sys.exit(main(service=True))
